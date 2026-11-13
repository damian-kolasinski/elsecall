//! Paginated history endpoints. The CLI assembles files; services never buffer
//! a fleet's entire trace. Peer routes inherit enrolled-key authentication.
use crate::{
    api,
    failure::Result,
    fleet::Fleet,
    report_api::Reporter,
    report_protocol::Cursor,
    reporting::{
        gathering::Source,
        window::{Window, invalid},
    },
    scheduling::timestamp::Timestamp,
    workload::trace::Page,
};
use axum::{
    Extension, Json, Router,
    body::Bytes,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use serde::{Deserialize, Serialize};
use std::{sync::Arc, time::Duration};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageRequest {
    pub since_ms: u64,
    pub until_ms: u64,
    pub member: Option<String>,
    pub before: Option<Cursor>,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct Members {
    pub members: Vec<String>,
    pub master: Option<String>,
    pub fleet_error: Option<String>,
}
fn window(request: &PageRequest) -> Result<Window> {
    Window::new(
        Timestamp::from_millis(request.since_ms),
        Timestamp::from_millis(request.until_ms),
        Timestamp::now(),
    )
}
fn source(reporter: &Reporter, member: Option<&str>) -> Result<Source> {
    reporter
        .fleet()
        .sources
        .into_iter()
        .find(|s| member.is_none_or(|m| s.member == m))
        .ok_or_else(|| invalid("The trace member is not in this fleet."))
}
pub fn routes(reporter: Reporter) -> Router {
    Router::new()
        .route("/v1/trace/members", post(members))
        .route("/v1/trace/requests", post(requests))
        .route("/v1/trace/load", post(load))
        .with_state(reporter)
}
pub fn peer_routes(reporter: Reporter) -> Router<Arc<Fleet>> {
    Router::new()
        .route("/peer/v1/trace/requests", post(peer_requests))
        .layer(Extension(reporter))
}
fn answer<T: Serialize>(result: Result<T>) -> Response {
    match result {
        Ok(data) => Json(api::Response::new(data)).into_response(),
        Err(e) => (
            if e.code == "invalid_request" {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::SERVICE_UNAVAILABLE
            },
            Json(api::ErrorResponse::new(e)),
        )
            .into_response(),
    }
}
async fn members(State(reporter): State<Reporter>) -> Response {
    let fleet = reporter.fleet();
    answer(Ok(Members {
        members: fleet.sources.into_iter().map(|s| s.member).collect(),
        master: fleet.master,
        fleet_error: fleet.fleet_error.map(|e| e.code),
    }))
}
async fn page(reporter: Reporter, body: Bytes, peer: bool) -> Result<Page> {
    let r: PageRequest =
        serde_json::from_slice(&body).map_err(|_| invalid("Invalid trace page request."))?;
    let w = window(&r)?;
    if peer && r.member.is_some() {
        return Err(invalid("Peer trace requests read only this member."));
    }
    let own_name = reporter.own.scheduler.name();
    source(
        &reporter,
        if peer {
            Some(&own_name)
        } else {
            r.member.as_deref()
        },
    )?
    .history
    .trace_page(w, r.before)
    .await
}
async fn requests(State(reporter): State<Reporter>, body: Bytes) -> Response {
    answer(page(reporter, body, false).await)
}
async fn peer_requests(Extension(reporter): Extension<Reporter>, body: Bytes) -> Response {
    answer(page(reporter, body, true).await)
}
async fn load(State(reporter): State<Reporter>, body: Bytes) -> Response {
    let work = async {
        let r: PageRequest =
            serde_json::from_slice(&body).map_err(|_| invalid("Invalid trace load request."))?;
        let w = window(&r)?;
        if w.span() > Duration::from_secs(12 * 3600) {
            return Err(invalid("Trace load pages span at most 12h."));
        }
        source(&reporter, r.member.as_deref())?
            .history
            .load(w, Duration::from_secs(60))
            .await
    };
    answer(work.await)
}
