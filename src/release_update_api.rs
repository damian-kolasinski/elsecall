//! Same-user local API for release discovery and durable update jobs.
use crate::{
    api,
    failure::Failure,
    release_update::{JobId, service::UpdateService},
};
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use std::sync::Arc;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstallRequest {
    #[serde(default)]
    pub reopen: bool,
    #[serde(default = "default_wait")]
    pub wait_seconds: u64,
}
fn default_wait() -> u64 {
    300
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PrepareRequest {
    id: JobId,
}

pub fn routes(service: Arc<UpdateService>) -> Router {
    Router::new()
        .route("/v1/update", get(status))
        .route("/v1/update/check", post(check))
        .route("/v1/update/install", post(install))
        .route("/v1/update/prepare", post(prepare))
        .with_state(service)
}
fn failure(error: Failure) -> Response {
    (StatusCode::CONFLICT, Json(api::ErrorResponse::new(error))).into_response()
}
async fn status(State(service): State<Arc<UpdateService>>) -> Response {
    match service.status() {
        Ok(status) => Json(api::Response::new(status)).into_response(),
        Err(error) => failure(error),
    }
}
async fn check(State(service): State<Arc<UpdateService>>) -> Response {
    match service.check().await {
        Ok(status) => Json(api::Response::new(status)).into_response(),
        Err(error) => failure(error),
    }
}
async fn install(State(service): State<Arc<UpdateService>>, body: axum::body::Bytes) -> Response {
    let Ok(request) = serde_json::from_slice::<InstallRequest>(&body) else {
        return failure(crate::release_update::invalid("Invalid update request."));
    };
    match service.start(request.reopen, request.wait_seconds).await {
        Ok(job) => (StatusCode::ACCEPTED, Json(api::Response::new(job))).into_response(),
        Err(error) => failure(error),
    }
}
async fn prepare(State(service): State<Arc<UpdateService>>, body: axum::body::Bytes) -> Response {
    let Ok(request) = serde_json::from_slice::<PrepareRequest>(&body) else {
        return failure(crate::release_update::invalid(
            "Invalid update preparation request.",
        ));
    };
    match service.prepare(&request.id).await {
        Ok(()) => Json(api::Response::new(serde_json::json!({"ready":true}))).into_response(),
        Err(error) => failure(error),
    }
}

/// Optional updater state must not take down the fleet or health routes.
pub fn unavailable(error: Failure) -> Router {
    let mut router = Router::new();
    for route in [
        "/v1/update",
        "/v1/update/check",
        "/v1/update/install",
        "/v1/update/prepare",
    ] {
        let error = error.clone();
        router = router.route(
            route,
            axum::routing::any(move || {
                let error = error.clone();
                async move { failure(error) }
            }),
        );
    }
    router
}
