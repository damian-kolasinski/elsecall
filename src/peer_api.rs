//! The member-to-member API served on the tailnet. Every route requires a request
//! signed by an enrolled member; everything else gets an empty 401. Every
//! authenticated response is signed by this member.
use crate::{
    api,
    failure::Failure,
    fleet::{AuthenticatedPeer, Fleet},
    peer_auth::{RequestProof, SIGNATURE_HEADER},
    peer_client::RESPONSE_LIMIT,
    roster::Roster,
};
use axum::{
    Extension, Json, Router,
    body::{Body, Bytes, to_bytes},
    extract::{Request, State},
    http::{HeaderValue, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use std::{sync::Arc, time::Duration};

pub const REQUEST_LIMIT: usize = 64 * 1024;
/// How long a request body may take to arrive once its headers named an enrolled member.
const BODY_TIMEOUT: Duration = Duration::from_secs(5);

/// The member API, with `placement`'s routes behind the same authentication.
pub fn routes(fleet: Arc<Fleet>, placement: Router<Arc<Fleet>>) -> Router {
    Router::new()
        .route("/peer/v1/status", get(status))
        .route("/peer/v1/roster", get(roster).put(offer))
        .route(
            "/peer/v1/placement/probe",
            post(crate::placement_authority::probe),
        )
        .merge(placement)
        .fallback(|| async {
            failure(
                StatusCode::NOT_FOUND,
                Failure::new(
                    "not_found",
                    "Unknown member API route.",
                    "Install the same Elsecall version on every member.",
                ),
            )
        })
        .layer(middleware::from_fn_with_state(fleet.clone(), authenticate))
        .with_state(fleet)
}

pub fn failure(status: StatusCode, error: Failure) -> Response {
    (status, Json(api::ErrorResponse::new(error))).into_response()
}

fn data<T: serde::Serialize>(value: T) -> Response {
    Json(api::Response::new(value)).into_response()
}

async fn authenticate(State(fleet): State<Arc<Fleet>>, request: Request, next: Next) -> Response {
    let budget = request
        .extensions()
        .get::<crate::peer_listener::ConnectionBudget>()
        .cloned();
    let (parts, body) = request.into_parts();
    let header = |name: &str| {
        parts
            .headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    };
    // Turn away requests that cannot authenticate before spending time on their body.
    let Some(proof) = RequestProof::parse(header).filter(|p| fleet.is_enrolled(&p.sender)) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let Ok(Ok(bytes)) = tokio::time::timeout(BODY_TIMEOUT, to_bytes(body, REQUEST_LIMIT)).await
    else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let path = parts.uri.path_and_query().map(|p| p.as_str()).unwrap_or("");
    let Some(peer) = fleet.authenticate(&proof, parts.method.as_str(), path, &bytes) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let skew = crate::version_skew::refusal(
        parts.uri.path(),
        crate::build_identity::announced().as_ref(),
        header(crate::version_skew::BUILD_HEADER).as_deref(),
    );
    let response = match skew {
        Some(error) => failure(StatusCode::CONFLICT, error),
        None => {
            if parts.uri.path() == crate::fleet_tool_inventory::PEER_ROUTE
                && let Some(budget) = &budget
            {
                budget.extend(
                    crate::fleet_tool_inventory::member_timeout()
                        + crate::fleet_tool_inventory::EXCHANGE_MARGIN,
                );
            }
            let mut request = Request::from_parts(parts, Body::from(bytes));
            request.extensions_mut().insert(peer.clone());
            next.run(request).await
        }
    };
    let (mut parts, body) = response.into_parts();
    let Ok(bytes) = to_bytes(body, RESPONSE_LIMIT).await else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let signature = fleet.sign_response(&peer, parts.status.as_u16(), &bytes);
    parts.headers.insert(
        SIGNATURE_HEADER,
        HeaderValue::from_str(&signature).expect("base64 header value"),
    );
    Response::from_parts(parts, Body::from(bytes))
}

async fn status(State(fleet): State<Arc<Fleet>>) -> Response {
    match fleet.status().await {
        Ok(status) => data(status),
        Err(error) => failure(StatusCode::SERVICE_UNAVAILABLE, error),
    }
}

async fn roster(State(fleet): State<Arc<Fleet>>) -> Response {
    match fleet.roster() {
        Ok(roster) => data(roster),
        Err(error) => failure(StatusCode::SERVICE_UNAVAILABLE, error),
    }
}

async fn offer(
    State(fleet): State<Arc<Fleet>>,
    Extension(peer): Extension<AuthenticatedPeer>,
    body: Bytes,
) -> Response {
    // Authenticated members are still untrusted input: the roster validates itself.
    let Ok(roster) = serde_json::from_slice::<Roster>(&body) else {
        return failure(
            StatusCode::BAD_REQUEST,
            Failure::new(
                "invalid_request",
                "The offered roster is invalid.",
                "Install the same Elsecall version on every member.",
            ),
        );
    };
    match fleet.offer(&peer.key, roster) {
        Ok(outcome) => data(outcome),
        Err(error) => failure(StatusCode::CONFLICT, error),
    }
}
