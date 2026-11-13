//! The member API routes of placement, behind the placement-authority boundary:
//! only the recorded master may offer runs or attach to them.
use crate::{
    failure::Failure,
    fleet::{AuthenticatedPeer, Fleet, PlacementAuthority},
    peer_api::failure,
    placement_host::PlacementHost,
    remote_run::runner,
    scheduling::{placed::PlacementOffer, quote::QuoteRequest, run_id::RunId},
    secure_channel::{Ephemeral, SecureStream},
};
use axum::{
    Extension, Json, Router,
    body::{Bytes, to_bytes},
    extract::{Path, Request, State},
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::post,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use hyper_util::rt::TokioIo;
use serde::Deserialize;
use std::sync::Arc;

pub fn routes(host: Arc<PlacementHost>) -> Router<Arc<Fleet>> {
    Router::new()
        .route("/peer/v1/placements", post(offer))
        .route("/peer/v1/placements/quote", post(quote))
        .route("/peer/v1/placements/{id}/attach", post(attach))
        .layer(Extension(host))
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

async fn offer(
    authority: PlacementAuthority,
    Extension(host): Extension<Arc<PlacementHost>>,
    body: Bytes,
) -> Response {
    // Placement authority is not trust in the content: the offer is validated.
    let Ok(offer) = serde_json::from_slice::<PlacementOffer>(&body) else {
        return invalid("The placement offer is invalid.");
    };
    match host.offer(authority.master().as_str(), &offer) {
        Ok(reply) => Json(crate::api::Response::new(reply)).into_response(),
        Err(error) => failure(StatusCode::CONFLICT, error),
    }
}

async fn quote(
    authority: PlacementAuthority,
    Extension(host): Extension<Arc<PlacementHost>>,
    body: Bytes,
) -> Response {
    let Ok(request) = serde_json::from_slice::<QuoteRequest>(&body) else {
        return invalid("The quote request is invalid.");
    };
    match host.quote(authority.master().as_str(), &request) {
        Ok(quote) => Json(crate::api::Response::new(quote)).into_response(),
        Err(error) => failure(StatusCode::CONFLICT, error),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AttachRequest {
    ephemeral: String,
}

async fn attach(
    State(fleet): State<Arc<Fleet>>,
    authority: PlacementAuthority,
    Extension(peer): Extension<AuthenticatedPeer>,
    Extension(host): Extension<Arc<PlacementHost>>,
    Path(id): Path<String>,
    mut request: Request,
) -> Response {
    let Some(id) = RunId::parse(&id) else {
        return invalid("Run IDs look like r-3f9a0c41d2b7.");
    };
    let upgrade = request
        .extensions_mut()
        .remove::<hyper::upgrade::OnUpgrade>();
    let Ok(body) = to_bytes(std::mem::take(request.body_mut()), 4096).await else {
        return invalid("The attach request is invalid.");
    };
    let Some(theirs) = serde_json::from_slice::<AttachRequest>(&body)
        .ok()
        .and_then(|r| URL_SAFE_NO_PAD.decode(r.ephemeral).ok())
        .and_then(|bytes| <[u8; 32]>::try_from(bytes).ok())
    else {
        return invalid("The attach request is invalid.");
    };
    let Some(upgrade) = upgrade else {
        return invalid("Attaching needs a connection upgrade.");
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
    let (accepted, context) = fleet.sign_channel(&peer, id.as_str(), &theirs, &public);
    // Checked before the admission is taken, so a bad key leaves it to lapse.
    let Some(keys) = ours.keys(&theirs, &context, false) else {
        return invalid("The attach request's key is invalid.");
    };
    let assignment = match host.take(authority.master().as_str(), &id) {
        Ok(assignment) => assignment,
        Err(error) => return failure(StatusCode::NOT_FOUND, error),
    };
    let runner_host = host.as_host();
    tokio::spawn(async move {
        match upgrade.await {
            Ok(upgraded) => {
                let stream = SecureStream::new(
                    crate::receive_inactivity::Reader::new(
                        crate::send_inactivity::Writer::new(
                            TokioIo::new(upgraded),
                            crate::remote_run::link::GRACE,
                        ),
                        crate::remote_run::link::GRACE,
                    ),
                    keys,
                );
                runner::run(stream, assignment, runner_host).await;
            }
            Err(_) => runner::never_attached(assignment, runner_host),
        }
    });
    let mut response = StatusCode::SWITCHING_PROTOCOLS.into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONNECTION, HeaderValue::from_static("upgrade"));
    headers.insert(header::UPGRADE, HeaderValue::from_static("elsecall-run/1"));
    headers.insert(
        crate::peer_auth::CHANNEL_HEADER,
        HeaderValue::from_str(&accepted).expect("base64 header value"),
    );
    response
}
