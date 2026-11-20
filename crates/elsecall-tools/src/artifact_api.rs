//! Serving and fetching artifacts over the members' channel: see [`artifact`].
//!
//! `POST /peer/v1/tools/artifact/mise` upgrades to `elsecall-artifact/1` and
//! streams this member's mise executable, the file the inventory runs, after
//! following links. Before upgrading, the member answers 404
//! `artifact_unavailable` when it has none, and 503 `artifact_busy` while it
//! already sends two artifacts. Any enrolled member may ask. The content is
//! streamed from the file as it is read, never held whole, and a copy that has
//! not ended within its deadline is dropped.
use crate::{
    artifact::{self, Artifact, PROTOCOL},
    mise_cli,
};
use axum::{
    Extension, Router,
    body::to_bytes,
    extract::{Request, State},
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::post,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use elsecall_api_response::failure;
use elsecall_contract::failure::Failure;
use elsecall_fleet::fleet::{AuthenticatedPeer, Fleet};
use elsecall_peers::{
    peer_client,
    roster::Member,
    secure_channel::{Ephemeral, SecureStream},
};
use hyper_util::rt::TokioIo;
use serde::Deserialize;
use std::{path::PathBuf, sync::Arc};

/// Artifacts this member sends at once; more wait for none and are refused.
static SENDING: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);
/// How long sending one artifact may take: a mise executable at a slow
/// member's pace.
const SEND_DEADLINE: std::time::Duration = std::time::Duration::from_secs(10 * 60);

pub(crate) fn peer_routes() -> Router<Arc<Fleet>> {
    Router::new().route(&Artifact::Mise.path(), post(serve_mise))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AttachRequest {
    ephemeral: String,
}

async fn serve_mise(
    State(fleet): State<Arc<Fleet>>,
    Extension(peer): Extension<AuthenticatedPeer>,
    request: Request,
) -> Response {
    let source = tokio::task::spawn_blocking(|| {
        mise_cli::located()
            .ok()
            .and_then(|(_, program)| program)
            .and_then(|program| std::fs::canonicalize(program).ok())
    })
    .await
    .ok()
    .flatten();
    let Some(source) = source else {
        return failure(
            StatusCode::NOT_FOUND,
            Failure::new(
                "artifact_unavailable",
                "This member has no mise to copy.",
                "Install mise on the master, then apply again.",
            ),
        );
    };
    upgrade(
        &fleet,
        &peer,
        request,
        &Artifact::Mise,
        Source::File(source),
    )
    .await
}

/// What a source streams.
enum Source {
    File(PathBuf),
}

async fn upgrade(
    fleet: &Fleet,
    peer: &AuthenticatedPeer,
    mut request: Request,
    artifact: &Artifact,
    source: Source,
) -> Response {
    let invalid = || {
        failure(
            StatusCode::BAD_REQUEST,
            Failure::new(
                "invalid_request",
                "The artifact request is invalid.",
                "Run elsecall tools apply on the master.",
            ),
        )
    };
    let upgrade = request
        .extensions_mut()
        .remove::<hyper::upgrade::OnUpgrade>();
    let Ok(body) = to_bytes(std::mem::take(request.body_mut()), 4096).await else {
        return invalid();
    };
    let Some(theirs) = serde_json::from_slice::<AttachRequest>(&body)
        .ok()
        .and_then(|request| URL_SAFE_NO_PAD.decode(request.ephemeral).ok())
        .and_then(|bytes| <[u8; 32]>::try_from(bytes).ok())
    else {
        return invalid();
    };
    let Some(upgrade) = upgrade else {
        return invalid();
    };
    let Some(ours) = Ephemeral::generate() else {
        return failure(
            StatusCode::SERVICE_UNAVAILABLE,
            Failure::new(
                "random_unavailable",
                "The operating system's random number generator failed.",
                "Retry; restart the machine if it keeps failing.",
            ),
        );
    };
    let Ok(permit) = SENDING.try_acquire() else {
        return failure(
            StatusCode::SERVICE_UNAVAILABLE,
            Failure::new(
                "artifact_busy",
                "This member is already sending as many copies as it sends at once.",
                "Retry once the other copies finish.",
            ),
        );
    };
    let public = ours.public;
    let (accepted, context) = fleet.sign_channel(peer, &artifact.purpose(), &theirs, &public);
    let Some(keys) = ours.keys(&theirs, &context, false) else {
        return invalid();
    };
    tokio::spawn(async move {
        let _permit = permit;
        let Ok(upgraded) = upgrade.await else {
            return;
        };
        let mut channel = SecureStream::new(TokioIo::new(upgraded), keys);
        let sending = async {
            match source {
                Source::File(path) => {
                    let mut reader = tokio::process::Command::new("/bin/cat");
                    reader
                        .arg(path)
                        .stdin(std::process::Stdio::null())
                        .stdout(std::process::Stdio::piped())
                        .stderr(std::process::Stdio::null())
                        .kill_on_drop(true);
                    let mut child = elsecall_host::child_start::spawn_async(&mut reader)?;
                    let mut stdout = child.stdout.take().expect("piped stdout");
                    let sent = artifact::send_content(&mut stdout, &mut channel).await?;
                    // Only a source that read the whole file vouches for it.
                    if child.wait().await?.success() {
                        sent.finish(&mut channel).await.map(drop)
                    } else {
                        Err(std::io::Error::other("the artifact could not be read"))
                    }
                }
            }
        };
        let sent = tokio::time::timeout(SEND_DEADLINE, sending).await;
        if !matches!(sent, Ok(Ok(()))) {
            elsecall_host::service_log!(
                "{}",
                serde_json::json!({"event": "tools_artifact_unsent"})
            );
        }
    });
    let mut response = StatusCode::SWITCHING_PROTOCOLS.into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONNECTION, HeaderValue::from_static("upgrade"));
    headers.insert(header::UPGRADE, HeaderValue::from_static(PROTOCOL));
    headers.insert(
        elsecall_peers::peer_auth::CHANNEL_HEADER,
        HeaderValue::from_str(&accepted).expect("base64 header value"),
    );
    response
}

/// Opens `artifact`'s channel to `member`, ready for [`artifact::receive`].
pub(crate) async fn open(
    fleet: &Fleet,
    member: &Member,
    artifact: &Artifact,
) -> elsecall_contract::failure::Result<peer_client::RunChannel> {
    peer_client::channel(
        fleet.key(),
        member,
        &artifact.path(),
        &artifact.purpose(),
        PROTOCOL,
    )
    .await
}
