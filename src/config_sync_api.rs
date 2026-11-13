//! The member API routes of configuration sync. The sync channel is behind the
//! sync-authority boundary: only the recorded master may open it. Any enrolled
//! member may read a member's sync state, or ask the master to push.
use crate::{
    config_sync::{
        protocol,
        service::{ConfigSync, PushRequest},
    },
    failure::Failure,
    fleet::{AuthenticatedPeer, Fleet, SyncAuthority},
    peer_api::failure,
    secure_channel::{Ephemeral, SecureStream},
};
use axum::{
    Extension, Json, Router,
    body::to_bytes,
    extract::{FromRequestParts, Request, State},
    http::{HeaderValue, StatusCode, header, request::Parts},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use hyper_util::rt::TokioIo;
use serde::Deserialize;
use std::sync::Arc;

impl FromRequestParts<Arc<Fleet>> for SyncAuthority {
    type Rejection = Response;
    async fn from_request_parts(parts: &mut Parts, fleet: &Arc<Fleet>) -> Result<Self, Response> {
        // The member API authenticates every request before any handler runs.
        let peer = parts
            .extensions
            .get::<AuthenticatedPeer>()
            .expect("member requests are authenticated before extraction");
        fleet
            .authorize_sync(&peer.key)
            .map_err(|error| failure(StatusCode::FORBIDDEN, error))
    }
}

pub fn routes(sync: Arc<ConfigSync>) -> Router<Arc<Fleet>> {
    Router::new()
        .route(protocol::ROUTE, post(attach))
        .route(crate::config_sync::service::STATUS_ROUTE, get(status))
        .route(crate::config_sync::service::PUSH_ROUTE, post(push))
        .layer(Extension(sync))
}

fn invalid(message: &str) -> Response {
    failure(
        StatusCode::BAD_REQUEST,
        Failure::new(
            "invalid_request",
            message,
            "Install the same Elsecall version on every member.",
        ),
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AttachRequest {
    ephemeral: String,
}

async fn attach(
    State(fleet): State<Arc<Fleet>>,
    authority: SyncAuthority,
    Extension(peer): Extension<AuthenticatedPeer>,
    Extension(sync): Extension<Arc<ConfigSync>>,
    mut request: Request,
) -> Response {
    let upgrade = request
        .extensions_mut()
        .remove::<hyper::upgrade::OnUpgrade>();
    let Ok(body) = to_bytes(std::mem::take(request.body_mut()), 4096).await else {
        return invalid("The sync request is invalid.");
    };
    let Some(theirs) = serde_json::from_slice::<AttachRequest>(&body)
        .ok()
        .and_then(|r| URL_SAFE_NO_PAD.decode(r.ephemeral).ok())
        .and_then(|bytes| <[u8; 32]>::try_from(bytes).ok())
    else {
        return invalid("The sync request is invalid.");
    };
    let Some(upgrade) = upgrade else {
        return invalid("Sync needs a connection upgrade.");
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
    let public = ours.public;
    let (accepted, context) = fleet.sign_channel(&peer, protocol::PURPOSE, &theirs, &public);
    let Some(keys) = ours.keys(&theirs, &context, false) else {
        return invalid("The sync request's key is invalid.");
    };
    let master = authority.master().clone();
    let sender = peer.key;
    tokio::spawn(async move {
        if let Ok(upgraded) = upgrade.await {
            let channel = SecureStream::new(TokioIo::new(upgraded), keys);
            sync.serve(channel, master, sender).await;
        }
    });
    let mut response = StatusCode::SWITCHING_PROTOCOLS.into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONNECTION, HeaderValue::from_static("upgrade"));
    headers.insert(
        header::UPGRADE,
        HeaderValue::from_static(protocol::PROTOCOL),
    );
    headers.insert(
        crate::peer_auth::CHANNEL_HEADER,
        HeaderValue::from_str(&accepted).expect("base64 header value"),
    );
    response
}

async fn status(Extension(sync): Extension<Arc<ConfigSync>>) -> Response {
    match tokio::task::spawn_blocking(move || sync.summary()).await {
        Ok(summary) => Json(crate::api::Response::new(summary)).into_response(),
        Err(_) => failure(
            StatusCode::INTERNAL_SERVER_ERROR,
            Failure::new(
                "sync_failed",
                "This member's sync state could not be read.",
                "Run elsecall sync status on that member.",
            ),
        ),
    }
}

async fn push(Extension(sync): Extension<Arc<ConfigSync>>, body: axum::body::Bytes) -> Response {
    let request = if body.is_empty() {
        PushRequest::default()
    } else {
        match serde_json::from_slice::<PushRequest>(&body) {
            Ok(request) => request,
            Err(_) => return invalid("The push request is invalid."),
        }
    };
    match sync.serve_push(request).await {
        Ok(round) => Json(crate::api::Response::new(round)).into_response(),
        Err(error) => failure(StatusCode::CONFLICT, error),
    }
}
