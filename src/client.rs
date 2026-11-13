use crate::{
    api::{ErrorResponse, Response, SCHEMA_VERSION},
    failure::{Failure, Result},
    local_access, private_files,
};
use http_body_util::{BodyExt, Full, Limited};
use hyper::{Method, Request, body::Bytes, client::conn::http1};
use hyper_util::rt::TokioIo;
use serde::{Serialize, de::DeserializeOwned};
use std::{path::Path, time::Duration};
use tokio::net::UnixStream;

/// The usual bound on a request; admission waits are unbounded instead.
pub const TIMEOUT: Duration = Duration::from_secs(3);
const SMALL_RESPONSE: usize = 64 * 1024;
/// Record listings are larger than other responses.
pub const LARGE_RESPONSE: usize = 8 * 1024 * 1024;

pub async fn get<T: DeserializeOwned>(socket: &Path, route: &str) -> Result<T> {
    bounded(request(socket, Method::GET, route, None, SMALL_RESPONSE)).await
}

/// A GET that waits up to `timeout`. The fleet view uses this, since the service asks every member.
pub async fn get_within<T: DeserializeOwned>(
    socket: &Path,
    route: &str,
    timeout: Duration,
) -> Result<T> {
    send(socket, Method::GET, route, None, timeout).await
}

/// A GET whose response may be up to 8 MiB, such as a record listing.
pub async fn get_large<T: DeserializeOwned>(socket: &Path, route: &str) -> Result<T> {
    bounded(request(socket, Method::GET, route, None, LARGE_RESPONSE)).await
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wait {
    /// Fail with `service_timeout` after three seconds.
    Bounded,
    /// Wait as long as the service keeps the connection open, as admission does
    /// while a request queues.
    Unbounded,
}

/// Posts a JSON body.
pub async fn post<T: DeserializeOwned, B: Serialize>(
    socket: &Path,
    route: &str,
    body: &B,
    wait: Wait,
) -> Result<T> {
    let body = serde_json::to_vec(body).expect("request bodies serialize");
    let call = request(socket, Method::POST, route, Some(body), SMALL_RESPONSE);
    match wait {
        Wait::Bounded => bounded(call).await,
        Wait::Unbounded => call.await,
    }
}

/// Posts a JSON body and waits as long as the service works on it, for a response
/// of up to 8 MiB, such as a housekeeping report.
pub async fn post_large<T: DeserializeOwned, B: Serialize>(
    socket: &Path,
    route: &str,
    body: &B,
) -> Result<T> {
    let body = serde_json::to_vec(body).expect("request bodies serialize");
    request(socket, Method::POST, route, Some(body), LARGE_RESPONSE).await
}

async fn bounded<T>(call: impl Future<Output = Result<T>>) -> Result<T> {
    match tokio::time::timeout(TIMEOUT, call).await {
        Ok(result) => result,
        Err(_) => Err(Failure::new(
            "service_timeout",
            "The local service did not respond within three seconds.",
            "Run elsecall doctor, then elsecall service restart.",
        )),
    }
}

async fn request<T: DeserializeOwned>(
    socket: &Path,
    method: Method,
    route: &str,
    body: Option<Vec<u8>>,
    limit: usize,
) -> Result<T> {
    let state = socket
        .parent()
        .expect("socket lives in the state directory");
    if !state
        .try_exists()
        .map_err(|e| Failure::io("Inspect state directory", e))?
    {
        return Err(Failure::unavailable());
    }
    private_files::check_directory(state)?;
    let stream = UnixStream::connect(socket)
        .await
        .map_err(|_| Failure::unavailable())?;
    local_access::authorize_peer(
        stream
            .peer_cred()
            .map_err(|e| Failure::io("Identify local service", e))?
            .uid(),
    )?;
    let (mut sender, connection) = http1::handshake(TokioIo::new(stream))
        .await
        .map_err(|_| protocol_error())?;
    let mut connections = tokio::task::JoinSet::new();
    connections.spawn(connection);
    async {
        let mut request = Request::builder()
            .method(method)
            .uri(route)
            .header("Host", "localhost");
        if body.is_some() {
            request = request.header("Content-Type", "application/json");
        }
        let request = request
            .body(Full::new(Bytes::from(body.unwrap_or_default())))
            .map_err(|_| protocol_error())?;
        // A connection that closes before a response means the service stopped.
        let response = sender
            .send_request(request)
            .await
            .map_err(|_| Failure::unavailable())?;
        let success = response.status().is_success();
        let bytes = Limited::new(response.into_body(), limit)
            .collect()
            .await
            .map_err(|_| protocol_error())?
            .to_bytes();
        if success {
            let response: Response<T> =
                serde_json::from_slice(&bytes).map_err(|_| protocol_error())?;
            if response.schema_version != SCHEMA_VERSION {
                return Err(protocol_error());
            }
            Ok(response.data)
        } else {
            let response: ErrorResponse =
                serde_json::from_slice(&bytes).map_err(|_| protocol_error())?;
            if response.schema_version != SCHEMA_VERSION {
                return Err(protocol_error());
            }
            Err(response.error)
        }
    }
    .await
}

fn protocol_error() -> Failure {
    Failure::new(
        "protocol_error",
        "The local service returned an invalid or incompatible response.",
        "Reinstall matching CLI and service versions with elsecall install.",
    )
}

/// Posts a JSON body and waits up to `timeout`.
pub async fn post_within<T: DeserializeOwned>(
    socket: &Path,
    route: &str,
    body: serde_json::Value,
    timeout: Duration,
) -> Result<T> {
    send(socket, Method::POST, route, Some(body), timeout).await
}

/// Sends a request with an optional JSON body, waiting up to `timeout` for the answer.
/// Fleet operations use it, since they wait for other members.
pub async fn send<T: DeserializeOwned>(
    socket: &Path,
    method: Method,
    route: &str,
    body: Option<serde_json::Value>,
    timeout: std::time::Duration,
) -> Result<T> {
    let body = body.map(|body| body.to_string().into_bytes());
    dispatch(socket, method, route, body, timeout, SMALL_RESPONSE).await
}

/// A request whose answer may be up to 8 MiB, with the caller's own deadline.
pub(crate) async fn send_large<T: DeserializeOwned>(
    socket: &Path,
    method: Method,
    route: &str,
    timeout: std::time::Duration,
) -> Result<T> {
    dispatch(socket, method, route, None, timeout, LARGE_RESPONSE).await
}

/// A request whose answer may be up to `limit` bytes, with the caller's own deadline.
pub(crate) async fn send_bounded<T: DeserializeOwned>(
    socket: &Path,
    method: Method,
    route: &str,
    timeout: std::time::Duration,
    limit: usize,
) -> Result<T> {
    dispatch(socket, method, route, None, timeout, limit).await
}

async fn dispatch<T: DeserializeOwned>(
    socket: &Path,
    method: Method,
    route: &str,
    body: Option<Vec<u8>>,
    timeout: std::time::Duration,
    limit: usize,
) -> Result<T> {
    match tokio::time::timeout(timeout, request(socket, method, route, body, limit)).await {
        Ok(result) => result,
        Err(_) => Err(Failure::new(
            "service_timeout",
            format!(
                "The local service did not respond within {} seconds.",
                timeout.as_secs()
            ),
            "Run elsecall doctor, then elsecall service restart.",
        )),
    }
}
