//! The service authorizes the caller's placed run and authenticates its assigned
//! member. Negotiated callers own run decryption; the service relays sealed bytes.
//! Legacy callers retain the plaintext relay. Both paths carry the caller/member
//! run protocol, including end-to-end heartbeats.
use crate::{
    api,
    failure::Failure,
    fleet::Fleet,
    peer_client,
    roster::MemberName,
    scheduling::{run_id::RunId, service::Scheduler},
    server::Peer,
};
use axum::{
    Json, Router,
    extract::{ConnectInfo, Path, Request, State},
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::post,
};
use hyper_util::rt::TokioIo;
use std::sync::Arc;

#[derive(Clone)]
struct Relay {
    scheduler: Arc<Scheduler>,
    fleet: Option<Arc<Fleet>>,
}

pub fn routes(scheduler: Arc<Scheduler>, fleet: Option<Arc<Fleet>>) -> Router {
    Router::new()
        .route("/v1/runs/{id}/attach", post(attach))
        .with_state(Relay { scheduler, fleet })
}

/// Copies bytes both ways. Once one side has ended, the other gets the grace
/// period to finish, so a member that went silent cannot keep the relay alive.
async fn relay_bytes<A, B>(caller: A, member: B)
where
    A: tokio::io::AsyncRead + tokio::io::AsyncWrite,
    B: tokio::io::AsyncRead + tokio::io::AsyncWrite,
{
    use tokio::io::AsyncWriteExt;
    let (mut caller_read, mut caller_write) = tokio::io::split(caller);
    let (mut member_read, mut member_write) = tokio::io::split(member);
    let up = async {
        let _ = tokio::io::copy(&mut caller_read, &mut member_write).await;
        let _ = member_write.shutdown().await;
    };
    let down = async {
        let _ = tokio::io::copy(&mut member_read, &mut caller_write).await;
        let _ = caller_write.shutdown().await;
    };
    tokio::pin!(up, down);
    let grace = crate::remote_run::link::GRACE;
    tokio::select! {
        _ = &mut up => { let _ = tokio::time::timeout(grace, &mut down).await; }
        _ = &mut down => { let _ = tokio::time::timeout(grace, &mut up).await; }
    }
}

fn failure(status: StatusCode, error: Failure) -> Response {
    (status, Json(api::ErrorResponse::new(error))).into_response()
}

async fn attach(
    State(relay): State<Relay>,
    ConnectInfo(caller): ConnectInfo<Peer>,
    Path(id): Path<String>,
    mut request: Request,
) -> Response {
    let not_found = || {
        Failure::new(
            "not_found",
            "No placed run of this caller waits to be attached.",
            "Run the command again with elsecall run.",
        )
    };
    let (Some(id), Some(pid)) = (RunId::parse(&id), caller.pid) else {
        return failure(StatusCode::NOT_FOUND, not_found());
    };
    let member = match relay.scheduler.placed_member(pid, &id) {
        Ok(member) => member,
        Err(error) => return failure(StatusCode::NOT_FOUND, error),
    };
    // Old callers keep the plaintext mode. New callers send only a bounded public
    // header and an empty body, so old services can upgrade that same connection.
    let sealed = match crate::sealed_run::one_header(request.headers(), header::UPGRADE.as_str()) {
        Some(crate::sealed_run::PROTOCOL) => {
            match crate::sealed_run::public_header(request.headers()) {
                Some(public) => Some(public),
                None => return failure(StatusCode::BAD_REQUEST, invalid_mode()),
            }
        }
        Some("elsecall-run/1") => None,
        _ => return failure(StatusCode::BAD_REQUEST, invalid_mode()),
    };
    if request.headers().contains_key(header::TRANSFER_ENCODING)
        || request
            .headers()
            .get(header::CONTENT_LENGTH)
            .is_some_and(|v| v != "0")
    {
        return failure(StatusCode::BAD_REQUEST, invalid_mode());
    }
    let Some(upgrade) = request
        .extensions_mut()
        .remove::<hyper::upgrade::OnUpgrade>()
    else {
        return failure(StatusCode::BAD_REQUEST, not_found());
    };
    let unavailable = || crate::scheduling::placed::not_in_fleet(&member);
    let Some(fleet) = relay.fleet else {
        return failure(StatusCode::BAD_GATEWAY, unavailable());
    };
    let target = fleet.roster().ok().and_then(|roster| {
        let name: MemberName = member.parse().ok()?;
        roster.member(&name).cloned()
    });
    let Some(target) = target else {
        return failure(StatusCode::BAD_GATEWAY, unavailable());
    };
    let mut response = StatusCode::SWITCHING_PROTOCOLS.into_response();
    let protocol = if let Some(public) = sealed {
        let channel =
            match peer_client::relay_attach(fleet.key(), &target, id.as_str(), public).await {
                Ok(channel) => channel,
                Err(error) => return attach_failure(error),
            };
        response.headers_mut().insert(
            crate::sealed_run::ACCEPTANCE,
            HeaderValue::from_str(&crate::sealed_run::encode(&channel.acceptance))
                .expect("public base64 metadata"),
        );
        // Preserve the service's peer-send bound below encryption. Local Unix
        // socket buffers must not hide a blocked TCP send from the CLI.
        let stream = crate::peer_transport::Observed::new(
            crate::send_inactivity::Writer::new(channel.stream, crate::remote_run::link::GRACE),
            target.name.as_str(),
            &format!("/peer/v1/placements/{}/attach", id.as_str()),
            crate::peer_transport::StreamTimeout::Caller,
        );
        tokio::spawn(async move {
            let Ok(upgraded) = upgrade.await else { return };
            relay_bytes(TokioIo::new(upgraded), stream).await;
        });
        crate::sealed_run::PROTOCOL
    } else {
        let channel = match peer_client::attach(fleet.key(), &target, id.as_str()).await {
            Ok(channel) => channel,
            Err(error) => return attach_failure(error),
        };
        tokio::spawn(async move {
            let Ok(upgraded) = upgrade.await else { return };
            relay_bytes(TokioIo::new(upgraded), channel).await;
        });
        "elsecall-run/1"
    };
    let headers = response.headers_mut();
    headers.insert(header::CONNECTION, HeaderValue::from_static("upgrade"));
    headers.insert(header::UPGRADE, HeaderValue::from_static(protocol));
    response
}

fn invalid_mode() -> Failure {
    Failure::new(
        "protocol_error",
        "The local run upgrade is invalid.",
        "Install matching CLI and service versions.",
    )
}
fn attach_failure(error: Failure) -> Response {
    let error = match error.code.as_str() {
        "not_found" | "member_unavailable" => error,
        _ => Failure::new("member_unavailable", error.message, error.hint),
    };
    failure(StatusCode::BAD_GATEWAY, error)
}
