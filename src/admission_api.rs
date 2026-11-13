//! The local HTTP interface of admission, a driving adapter of the scheduler.
use crate::{
    api,
    failure::Result,
    scheduling::{request, run_id::RunId, service::Scheduler},
    server::Peer,
};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{ConnectInfo, Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::sync::Arc;

pub const MAXIMUM_LISTED: u32 = 500;

pub fn routes(scheduler: Arc<Scheduler>) -> Router {
    Router::new()
        .route("/v1/admissions", post(admit))
        .route("/v1/runs", get(runs))
        .route("/v1/runs/{id}", get(record))
        .route("/v1/runs/{id}/command", post(command_started))
        .route("/v1/runs/{id}/end", post(run_ended))
        .route("/v1/runs/{id}/observation", post(observation))
        .route("/v1/runs/{id}/placed", post(placed_report))
        .route("/v1/leases/release", post(release))
        .with_state(scheduler)
}

fn respond<T: Serialize>(result: Result<T>) -> Response {
    match result {
        Ok(data) => Json(api::Response::new(data)).into_response(),
        Err(failure) => {
            let status = match failure.code.as_str() {
                "invalid_request" => StatusCode::BAD_REQUEST,
                "not_found" => StatusCode::NOT_FOUND,
                "fleet_busy" | "request_too_large" | "deadline_skipped" | "member_unavailable"
                | "config_sync_failed" | "cancelled" => StatusCode::CONFLICT,
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            };
            (status, Json(api::ErrorResponse::new(failure))).into_response()
        }
    }
}

/// Request bodies are validated here, at the boundary, as untrusted input.
fn parse<T: DeserializeOwned>(body: &Bytes) -> Result<T> {
    serde_json::from_slice(body).map_err(|_| request::invalid("The request body is not valid."))
}
fn run_id(text: &str) -> Result<RunId> {
    RunId::parse(text).ok_or_else(|| request::invalid("Run IDs look like r-3f9a0c41d2b7."))
}
fn peer(peer: &Peer) -> Result<u32> {
    peer.pid
        .ok_or_else(|| request::invalid("The requesting process could not be identified."))
}

async fn admit(
    State(scheduler): State<Arc<Scheduler>>,
    ConnectInfo(caller): ConnectInfo<Peer>,
    body: Bytes,
) -> Response {
    let result = async { scheduler.request(peer(&caller)?, parse(&body)?).await };
    respond(result.await)
}

#[derive(Deserialize)]
struct Listing {
    limit: Option<u32>,
}
async fn runs(State(scheduler): State<Arc<Scheduler>>, Query(listing): Query<Listing>) -> Response {
    respond(scheduler.runs(listing.limit.unwrap_or(50).clamp(1, MAXIMUM_LISTED)))
}
async fn record(State(scheduler): State<Arc<Scheduler>>, Path(id): Path<String>) -> Response {
    respond(run_id(&id).and_then(|id| scheduler.record(&id)))
}
async fn command_started(
    State(scheduler): State<Arc<Scheduler>>,
    ConnectInfo(caller): ConnectInfo<Peer>,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    respond((|| {
        scheduler.command_started(peer(&caller)?, &run_id(&id)?, parse(&body)?)
    })())
}
async fn run_ended(
    State(scheduler): State<Arc<Scheduler>>,
    ConnectInfo(caller): ConnectInfo<Peer>,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    respond((|| {
        scheduler.run_ended(peer(&caller)?, &run_id(&id)?, parse(&body)?)
    })())
}
async fn placed_report(
    State(scheduler): State<Arc<Scheduler>>,
    ConnectInfo(caller): ConnectInfo<Peer>,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    respond((|| {
        scheduler.placed_report(peer(&caller)?, &run_id(&id)?, parse(&body)?)
    })())
}

async fn release(
    State(scheduler): State<Arc<Scheduler>>,
    ConnectInfo(caller): ConnectInfo<Peer>,
    body: Bytes,
) -> Response {
    respond((|| scheduler.release(peer(&caller)?, parse(&body)?))())
}

async fn observation(
    State(scheduler): State<Arc<Scheduler>>,
    ConnectInfo(caller): ConnectInfo<Peer>,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    respond((|| {
        if body.len() > crate::workload_metadata::MAXIMUM_JSON_BYTES {
            return Err(request::invalid("Workload observation is too large."));
        }
        scheduler.observe_workload(peer(&caller)?, &run_id(&id)?, parse(&body)?)
    })())
}
