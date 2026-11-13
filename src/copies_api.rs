//! The local HTTP interface of housekeeping, a driving adapter of the
//! housekeeper: list this member's copies, or run a pass now.
use crate::{api, failure::Failure, housekeeping::pass::Housekeeper};
use axum::{
    Json, Router,
    body::Bytes,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct PruneRequest {
    #[serde(default)]
    pub dry_run: bool,
}

pub fn routes(housekeeper: Arc<Housekeeper>) -> Router {
    Router::new()
        .route("/v1/copies", get(list))
        .route("/v1/copies/prune", post(prune))
        .with_state(housekeeper)
}

async fn list(State(housekeeper): State<Arc<Housekeeper>>) -> Response {
    let listing = tokio::task::spawn_blocking(move || housekeeper.listing())
        .await
        .expect("listings do not panic");
    match listing {
        Ok(listing) => Json(api::Response::new(listing)).into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(api::ErrorResponse::new(error)),
        )
            .into_response(),
    }
}

async fn prune(State(housekeeper): State<Arc<Housekeeper>>, body: Bytes) -> Response {
    let Ok(request) = serde_json::from_slice::<PruneRequest>(&body) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(api::ErrorResponse::new(Failure::new(
                "invalid_request",
                "The prune request is not valid.",
                "Run elsecall copies prune --help.",
            ))),
        )
            .into_response();
    };
    Json(api::Response::new(housekeeper.pass(request.dry_run).await)).into_response()
}
