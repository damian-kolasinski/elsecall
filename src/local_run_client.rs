//! Local attach negotiation. An actual legacy 101 continues on this same
//! connection; negotiation never repeats a peer attach or command execution.
use crate::{
    api::ErrorResponse,
    failure::{Failure, Result},
    local_access, private_files,
    remote_run::link::Link,
    scheduling::run_id::RunId,
    sealed_run,
    secure_channel::{Ephemeral, SecureStream},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use http_body_util::{BodyExt, Full, Limited};
use hyper::{Method, Request, body::Bytes, client::conn::http1, header};
use hyper_util::rt::TokioIo;
use std::path::Path;
use tokio::net::UnixStream;

pub(crate) async fn attach(socket: &Path, run: &RunId) -> Result<Link> {
    let ephemeral = Ephemeral::generate().ok_or_else(|| {
        Failure::new(
            "random_unavailable",
            "The operating system's random number generator failed.",
            "Retry the command.",
        )
    })?;
    let route = format!("/v1/runs/{run}/attach");
    let state = socket
        .parent()
        .expect("socket lives in the state directory");
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
    let (mut sender, connection) = http1::handshake(TokioIo::new(crate::socket_io::Socket::new(
        stream,
        crate::remote_run::link::GRACE,
    )))
    .await
    .map_err(|_| protocol_error())?;
    tokio::spawn(async move {
        let _ = connection.with_upgrades().await;
    });
    let request = Request::builder()
        .method(Method::POST)
        .uri(&route)
        .header("Host", "localhost")
        .header("Connection", "upgrade")
        .header("Upgrade", sealed_run::PROTOCOL)
        .header(
            sealed_run::EPHEMERAL,
            URL_SAFE_NO_PAD.encode(ephemeral.public),
        )
        .body(Full::new(Bytes::new()))
        .map_err(|_| protocol_error())?;
    let mut response = sender
        .send_request(request)
        .await
        .map_err(|_| Failure::unavailable())?;
    if response.status() == hyper::StatusCode::SWITCHING_PROTOCOLS {
        let protocol = sealed_run::one_header(response.headers(), header::UPGRADE.as_str())
            .ok_or_else(protocol_error)?;
        let keys = match protocol {
            sealed_run::PROTOCOL => Some(
                sealed_run::decode(response.headers())
                    .and_then(|acceptance| acceptance.keys(run.as_str(), ephemeral))
                    .ok_or_else(protocol_error)?,
            ),
            "elsecall-run/1" if !response.headers().contains_key(sealed_run::ACCEPTANCE) => None,
            _ => return Err(protocol_error()),
        };
        let upgraded = hyper::upgrade::on(&mut response)
            .await
            .map_err(|_| protocol_error())?;
        let stream = TokioIo::new(upgraded);
        return Ok(match keys {
            Some(keys) => Link::open_monitored(SecureStream::new(
                crate::receive_inactivity::Reader::new(
                    crate::send_inactivity::Writer::new(stream, crate::remote_run::link::GRACE),
                    crate::remote_run::link::GRACE,
                ),
                keys,
            )),
            None => Link::open(stream),
        });
    }
    let bytes = Limited::new(response.into_body(), 64 * 1024)
        .collect()
        .await
        .map_err(|_| protocol_error())?
        .to_bytes();
    let response: ErrorResponse = serde_json::from_slice(&bytes).map_err(|_| protocol_error())?;
    Err(response.error)
}
fn protocol_error() -> Failure {
    Failure::new(
        "protocol_error",
        "The local service returned an invalid or incompatible run upgrade.",
        "Install matching CLI and service versions with elsecall install.",
    )
}

#[cfg(test)]
mod tests;
