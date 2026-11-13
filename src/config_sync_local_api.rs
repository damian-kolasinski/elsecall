//! Configuration sync on the local socket, where the same-user rule authorizes the
//! operator. The service holds the member key and the replica; the CLI holds neither.
use crate::{
    api,
    config_sync::{service::ConfigSync, synced_path::SyncedPath},
    failure::Failure,
};
use axum::{
    Json, Router,
    body::Bytes,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{any, post},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::sync::Arc;

#[derive(Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct PlanRequest {
    #[serde(default)]
    pub paths: Vec<SyncedPath>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PathRequest {
    pub path: SyncedPath,
    #[serde(default)]
    pub confirm: Option<String>,
}

#[derive(Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct PushRequest {
    /// Only this member, pushing only if it differs.
    #[serde(default)]
    pub member: Option<crate::roster::MemberName>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryRequest {
    pub limit: usize,
}

pub fn routes(sync: Arc<ConfigSync>) -> Router {
    Router::new()
        .route("/v1/sync", post(status))
        .route("/v1/sync/service", post(service))
        .route("/v1/sync/rehearsal", post(rehearsal_settings))
        .route("/v1/sync/plan", post(plan))
        .route("/v1/sync/enable", post(enable))
        .route("/v1/sync/disable", post(disable))
        .route("/v1/sync/push", post(push))
        .route("/v1/sync/history", post(history))
        .with_state(sync)
}

/// Sync routes for a service whose fleet state could not be opened.
pub fn unavailable(error: Failure) -> Router {
    let respond = move || {
        let error = error.clone();
        async move {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(api::ErrorResponse::new(error)),
            )
        }
    };
    Router::new()
        .route("/v1/sync", any(respond.clone()))
        .route("/v1/sync/{*operation}", any(respond))
}

fn respond<T: Serialize>(result: crate::failure::Result<T>) -> Response {
    match result {
        Ok(value) => Json(api::Response::new(value)).into_response(),
        Err(error) => (StatusCode::CONFLICT, Json(api::ErrorResponse::new(error))).into_response(),
    }
}

fn parse<T: DeserializeOwned>(body: &Bytes) -> crate::failure::Result<T> {
    serde_json::from_slice(body).map_err(|_| {
        Failure::new(
            "invalid_request",
            "The sync request is invalid.",
            "Run elsecall sync --help for the expected arguments.",
        )
    })
}

async fn status(State(sync): State<Arc<ConfigSync>>) -> Response {
    respond(sync.fleet_status().await)
}
async fn plan(State(sync): State<Arc<ConfigSync>>, body: Bytes) -> Response {
    match parse::<PlanRequest>(&body) {
        Ok(request) => respond(sync.plan(request.paths).await),
        Err(error) => respond::<()>(Err(error)),
    }
}
async fn enable(State(sync): State<Arc<ConfigSync>>, body: Bytes) -> Response {
    match parse::<PathRequest>(&body) {
        Ok(request) => respond(sync.enable(request.path, request.confirm).await),
        Err(error) => respond::<()>(Err(error)),
    }
}
async fn disable(State(sync): State<Arc<ConfigSync>>, body: Bytes) -> Response {
    match parse::<PathRequest>(&body) {
        Ok(request) => respond(sync.disable(request.path, request.confirm).await),
        Err(error) => respond::<()>(Err(error)),
    }
}
async fn push(State(sync): State<Arc<ConfigSync>>, body: Bytes) -> Response {
    match parse::<PushRequest>(&body) {
        Ok(request) => respond(sync.request_push(request.member).await),
        Err(error) => respond::<()>(Err(error)),
    }
}
async fn history(State(sync): State<Arc<ConfigSync>>, body: Bytes) -> Response {
    match parse::<HistoryRequest>(&body) {
        Ok(request) => {
            let limit = request.limit.min(500);
            respond(
                tokio::task::spawn_blocking(move || sync.history(limit))
                    .await
                    .unwrap_or_else(|_| {
                        Err(Failure::new(
                            "sync_failed",
                            "Sync records could not be read.",
                            "Run elsecall service restart.",
                        ))
                    })
                    .map(|records| serde_json::json!({ "records": records })),
            )
        }
        Err(error) => respond::<()>(Err(error)),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ServiceRequest {
    confirm: Option<String>,
}
async fn service(State(sync): State<Arc<ConfigSync>>, body: Bytes) -> Response {
    match parse::<ServiceRequest>(&body) {
        Ok(request) => respond(sync.service_configuration(request.confirm).await),
        Err(error) => respond::<()>(Err(error)),
    }
}

use crate::service_configuration::RehearsalSettings;
async fn rehearsal_settings(State(sync): State<Arc<ConfigSync>>, body: Bytes) -> Response {
    match parse::<RehearsalSettings>(&body) {
        Ok(request) => respond(sync.rehearsal_settings(request).await),
        Err(error) => respond::<()>(Err(error)),
    }
}
