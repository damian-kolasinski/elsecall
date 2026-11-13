//! The member API routes of upgrades. The upgrade channel is behind the
//! upgrade-authority boundary: only the recorded master may open it, since what it
//! carries is code this member will run. Any enrolled member may read how this
//! member's latest upgrade went.
use crate::{
    failure::Failure,
    fleet::{AuthenticatedPeer, Fleet, UpgradeAuthority},
    fleet_upgrade::{member::Upgrades, protocol},
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

impl FromRequestParts<Arc<Fleet>> for UpgradeAuthority {
    type Rejection = Response;
    async fn from_request_parts(parts: &mut Parts, fleet: &Arc<Fleet>) -> Result<Self, Response> {
        // The member API authenticates every request before any handler runs.
        let peer = parts
            .extensions
            .get::<AuthenticatedPeer>()
            .expect("member requests are authenticated before extraction");
        fleet
            .authorize_upgrade(&peer.key)
            .map_err(|error| failure(StatusCode::FORBIDDEN, error))
    }
}

pub fn routes(upgrades: Arc<Upgrades>) -> Router<Arc<Fleet>> {
    Router::new()
        .route(protocol::ROUTE, post(attach))
        .route(protocol::STATUS_ROUTE, get(status))
        .layer(Extension(upgrades))
}

/// Routes for a member whose upgrade record could not be opened: it refuses
/// upgrades with the reason, and the rest of the member API keeps working.
pub fn unavailable(error: Failure) -> Router<Arc<Fleet>> {
    let respond = move || {
        let error = error.clone();
        async move { failure(StatusCode::SERVICE_UNAVAILABLE, error) }
    };
    Router::new()
        .route(protocol::ROUTE, post(respond.clone()))
        .route(protocol::STATUS_ROUTE, get(respond))
}

fn invalid(message: &str) -> Response {
    failure(
        StatusCode::BAD_REQUEST,
        Failure::new(
            "invalid_request",
            message,
            "Run elsecall fleet upgrade on the master.",
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
    authority: UpgradeAuthority,
    Extension(peer): Extension<AuthenticatedPeer>,
    Extension(upgrades): Extension<Arc<Upgrades>>,
    mut request: Request,
) -> Response {
    let upgrade = request
        .extensions_mut()
        .remove::<hyper::upgrade::OnUpgrade>();
    let Ok(body) = to_bytes(std::mem::take(request.body_mut()), 4096).await else {
        return invalid("The upgrade request is invalid.");
    };
    let Some(theirs) = serde_json::from_slice::<AttachRequest>(&body)
        .ok()
        .and_then(|r| URL_SAFE_NO_PAD.decode(r.ephemeral).ok())
        .and_then(|bytes| <[u8; 32]>::try_from(bytes).ok())
    else {
        return invalid("The upgrade request is invalid.");
    };
    let Some(upgrade) = upgrade else {
        return invalid("An upgrade needs a connection upgrade.");
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
        return invalid("The upgrade request's key is invalid.");
    };
    let master = authority.master().clone();
    let sender = peer.key;
    tokio::spawn(async move {
        if let Ok(upgraded) = upgrade.await {
            let channel = SecureStream::new(TokioIo::new(upgraded), keys);
            upgrades.serve(channel, master, sender).await;
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

async fn status(Extension(upgrades): Extension<Arc<Upgrades>>) -> Response {
    match tokio::task::spawn_blocking(move || upgrades.status()).await {
        Ok(Ok(state)) => Json(crate::api::Response::new(state)).into_response(),
        Ok(Err(error)) => failure(StatusCode::SERVICE_UNAVAILABLE, error),
        Err(_) => failure(
            StatusCode::INTERNAL_SERVER_ERROR,
            Failure::new(
                "upgrade_state_unavailable",
                "This member's upgrade record could not be read.",
                "Run elsecall doctor on that member.",
            ),
        ),
    }
}
