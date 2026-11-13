//! TCP transport for the member API: binds only this machine's tailnet addresses and
//! bounds what an unauthenticated connection can cost.
use crate::{
    fleet::Fleet,
    roster::MemberName,
    tailnet::{Tailnet, is_member_address},
};
use axum::Router;
use hyper_util::{
    rt::{TokioIo, TokioTimer},
    service::TowerToHyperService,
};
use std::{
    collections::BTreeSet,
    future::Future,
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{net::TcpListener, sync::Semaphore, sync::watch};

const MAX_CONNECTIONS: usize = 64;
const HEADER_TIMEOUT: Duration = Duration::from_secs(5);
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(30);
const REDISCOVERY: Duration = Duration::from_secs(30);

/// The deadline of one member connection, measured from when it is accepted.
/// Authenticated tool-inventory requests extend it; everything else keeps
/// [`CONNECTION_TIMEOUT`].
#[derive(Clone)]
pub(crate) struct ConnectionBudget {
    started: Instant,
    limit: Arc<watch::Sender<Duration>>,
}

impl ConnectionBudget {
    fn arm() -> (Self, watch::Receiver<Duration>) {
        let (sender, receiver) = watch::channel(CONNECTION_TIMEOUT);
        (
            Self {
                started: Instant::now(),
                limit: Arc::new(sender),
            },
            receiver,
        )
    }

    fn started(&self) -> Instant {
        self.started
    }

    /// Let this connection run for `extra` from now, when that is later than
    /// its current deadline.
    pub(crate) fn extend(&self, extra: Duration) {
        let wanted = self.started.elapsed().saturating_add(extra);
        self.limit.send_if_modified(|limit| {
            if *limit >= wanted {
                return false;
            }
            *limit = wanted;
            true
        });
    }
}

/// Where the member listener looks for addresses, and its port.
pub struct Settings {
    pub tailnet: Arc<dyn Tailnet>,
    pub port: u16,
}

/// Discovers tailnet addresses, binds each one on the port, and keeps retrying in the
/// background for addresses that appear later. Returns after the first attempt.
pub async fn start(fleet: Arc<Fleet>, settings: Settings, app: Router) {
    let Settings { tailnet, port } = settings;
    let mut bound = BTreeSet::new();
    let limit = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    let mut last_problem = None;
    attempt(
        &fleet,
        &tailnet,
        port,
        &app,
        &limit,
        &mut bound,
        &mut last_problem,
    )
    .await;
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(REDISCOVERY).await;
            attempt(
                &fleet,
                &tailnet,
                port,
                &app,
                &limit,
                &mut bound,
                &mut last_problem,
            )
            .await;
        }
    });
}

async fn attempt(
    fleet: &Arc<Fleet>,
    tailnet: &Arc<dyn Tailnet>,
    port: u16,
    app: &Router,
    limit: &Arc<Semaphore>,
    bound: &mut BTreeSet<IpAddr>,
    last_problem: &mut Option<String>,
) {
    let discovered = {
        let tailnet = tailnet.clone();
        tokio::task::spawn_blocking(move || tailnet.discover()).await
    };
    let presence = match discovered {
        Ok(Ok(presence)) => presence,
        Ok(Err(error)) => return note(last_problem, &error.code),
        Err(_) => return note(last_problem, "tailnet_discovery_failed"),
    };
    let Ok(name) = presence.name.parse::<MemberName>() else {
        return note(last_problem, "invalid_tailnet_name");
    };
    for address in presence.addresses {
        if bound.contains(&address) || !is_member_address(address) {
            continue;
        }
        let endpoint = SocketAddr::new(address, port);
        match TcpListener::bind(endpoint).await {
            Ok(listener) => {
                bound.insert(address);
                crate::service_log!(
                    "{{\"event\":\"member_listener_bound\",\"endpoint\":\"{endpoint}\"}}"
                );
                tokio::spawn(accept(listener, app.clone(), limit.clone()));
            }
            Err(error) => note(
                last_problem,
                &format!("bind_failed:{endpoint}:{}", error.kind()),
            ),
        }
    }
    if !bound.is_empty() {
        *last_problem = None;
    }
    fleet.set_presence(
        name,
        bound.iter().map(|a| SocketAddr::new(*a, port)).collect(),
    );
}

/// Logs a listener problem once per change, so retries do not flood the log.
fn note(last: &mut Option<String>, problem: &str) {
    if last.as_deref() != Some(problem) {
        crate::service_log!("{{\"event\":\"member_listener_unavailable\",\"reason\":{problem:?}}}");
        *last = Some(problem.to_owned());
    }
}

async fn accept(listener: TcpListener, app: Router, limit: Arc<Semaphore>) {
    loop {
        let Ok(permit) = limit.clone().acquire_owned().await else {
            return;
        };
        let stream = match listener.accept().await {
            Ok((stream, _)) => stream,
            Err(_) => {
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let app = app.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let (budget, mut deadline) = ConnectionBudget::arm();
            let started = budget.started();
            let service = TowerToHyperService::new(app.layer(axum::Extension(budget)));
            let mut builder = hyper::server::conn::http1::Builder::new();
            builder
                .timer(TokioTimer::new())
                .header_read_timeout(HEADER_TIMEOUT)
                .keep_alive(false);
            // Upgraded channels outlive this HTTP exchange and are owned by
            // their runner. `drive` bounds the exchange itself.
            let connection = builder
                .serve_connection(
                    TokioIo::new(crate::socket_io::Socket::new(
                        stream,
                        crate::remote_run::link::GRACE,
                    )),
                    service,
                )
                .with_upgrades();
            drive(connection, &mut deadline, started).await;
        });
    }
}

async fn drive<E>(
    connection: impl Future<Output = Result<(), E>>,
    deadline: &mut watch::Receiver<Duration>,
    started: Instant,
) {
    // Placement attach, configuration sync and upgrade attach answer 101 and
    // keep the socket on the upgraded connection, which this deadline does not
    // cover: a drain may run for an hour, and those streams have their own
    // bounds. Report routes are ordinary HTTP and finish inside the 30 second
    // bound. Tool inventory is an ordinary GET, and nothing here exempts a
    // route, so once that request is authenticated `peer_api` extends this
    // deadline to the member budget plus ten seconds. Header reads stay at
    // five seconds, and an unauthenticated body still has five seconds.
    let mut connection = std::pin::pin!(connection);
    loop {
        let limit = *deadline.borrow();
        let elapsed = started.elapsed();
        if elapsed >= limit {
            break;
        }
        tokio::select! {
            biased;
            result = connection.as_mut() => {
                let _ = result;
                break;
            }
            _ = tokio::time::sleep(limit - elapsed) => {
                if started.elapsed() >= *deadline.borrow() {
                    break;
                }
            }
            changed = deadline.changed() => {
                if changed.is_err() {
                    break;
                }
            }
        }
    }
}
