//! The local HTTP interface of replay, a driving adapter: reads this member's
//! placeable records and replays their decisions under changed parameters.
use crate::{
    api,
    failure::Failure,
    scheduling::{
        replay::{self, Change, Replay},
        service::Scheduler,
        timestamp::Timestamp,
    },
};
use axum::{
    Json, Router,
    body::Bytes,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// The most placeable records one replay reads, newest first.
pub const MAXIMUM_RECORDS: u32 = 5000;

#[derive(Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ReplayRequest {
    /// Only requests made since then; every retained record by default.
    #[serde(default)]
    pub since: Option<Timestamp>,
    /// `key=value` settings, as `--set` takes them.
    #[serde(default)]
    pub set: Vec<String>,
}

#[derive(Serialize)]
struct ReplayResponse {
    /// Whose records were replayed.
    member: String,
    since: Option<Timestamp>,
    until: Timestamp,
    records_read: usize,
    /// Whether the newest `MAXIMUM_RECORDS` were all that was read.
    limited: bool,
    #[serde(flatten)]
    replay: Replay,
}

pub fn routes(scheduler: Arc<Scheduler>) -> Router {
    Router::new()
        .route("/v1/replay", post(replay))
        .with_state(scheduler)
}

/// An invalid replay request, from the CLI or at this boundary.
pub fn invalid(message: impl Into<String>) -> Failure {
    Failure::new(
        "invalid_request",
        message,
        "Run elsecall replay --help; --set takes the keys of config.toml's [placement] and [admission] sections.",
    )
}

fn error(status: StatusCode, failure: Failure) -> Response {
    (status, Json(api::ErrorResponse::new(failure))).into_response()
}

async fn replay(State(scheduler): State<Arc<Scheduler>>, body: Bytes) -> Response {
    let Ok(request) = serde_json::from_slice::<ReplayRequest>(&body) else {
        return error(
            StatusCode::BAD_REQUEST,
            invalid("The replay request is not valid."),
        );
    };
    let change = match Change::parse(&request.set) {
        Ok(change) => change,
        Err(why) => return error(StatusCode::BAD_REQUEST, invalid(why)),
    };
    let since = request.since;
    let result = tokio::task::spawn_blocking(move || {
        let until = Timestamp::now();
        // One more than the bound tells whether the bound applied.
        let mut records = scheduler.placeable_records(
            since.unwrap_or(Timestamp::from_millis(0)),
            MAXIMUM_RECORDS + 1,
        )?;
        let limited = records.len() > MAXIMUM_RECORDS as usize;
        if limited {
            records.remove(0);
        }
        Ok::<_, Failure>(ReplayResponse {
            member: scheduler.name(),
            since,
            until,
            records_read: records.len(),
            limited,
            replay: replay::replay(&records, &change),
        })
    })
    .await;
    match result {
        Ok(Ok(response)) => Json(api::Response::new(response)).into_response(),
        Ok(Err(failure)) => error(StatusCode::INTERNAL_SERVER_ERROR, failure),
        Err(_) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            Failure::new(
                "replay_failed",
                "The replay stopped unexpectedly.",
                "Run elsecall doctor and inspect its log path.",
            ),
        ),
    }
}
