//! Fleet operations on the local socket, where the same-user rule authorizes the
//! operator. The service holds the member key; the CLI never reads it.
use crate::{api, failure::Failure, fleet::Fleet, fleet_changes, fleet_view, roster::MemberName};
use axum::{
    Json, Router,
    body::Bytes,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{any, get, post},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::sync::Arc;

#[derive(Serialize, Deserialize)]
pub struct CodeRequest {
    pub code: String,
    #[serde(default)]
    pub force: bool,
}
#[derive(Serialize, Deserialize)]
pub struct MemberRequest {
    pub name: MemberName,
    #[serde(default)]
    pub force: bool,
}
#[derive(Serialize, Deserialize)]
pub struct ForceRequest {
    #[serde(default)]
    pub force: bool,
}
#[derive(Serialize, Deserialize)]
pub struct SyncRequest {
    #[serde(default)]
    pub force: bool,
    #[serde(default)]
    pub override_newer: bool,
}

#[derive(Clone)]
struct Viewing {
    fleet: Arc<Fleet>,
    placed: Arc<dyn fleet_view::PlacedRuns>,
}

pub fn routes(fleet: Arc<Fleet>, placed: Arc<dyn fleet_view::PlacedRuns>) -> Router {
    let viewing = Router::new()
        .route("/v1/fleet", get(view))
        .route("/v1/fleet/builds", get(builds))
        .with_state(Viewing {
            fleet: fleet.clone(),
            placed,
        });
    Router::new()
        .route("/v1/fleet/identity", get(identity))
        .route("/v1/fleet/enroll", post(enroll))
        .route("/v1/fleet/join", post(join))
        .route("/v1/fleet/remove", post(remove))
        .route("/v1/fleet/leave", post(leave))
        .route("/v1/fleet/sync", post(sync))
        .route("/v1/fleet/master", post(move_master))
        .route("/v1/fleet/placement-probe", post(probe_placement))
        .with_state(fleet)
        .merge(viewing)
}

/// Fleet routes for a service whose fleet state could not be opened: the local API
/// keeps working, and every fleet operation reports why fleet membership is unavailable.
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
        .route("/v1/fleet", any(respond.clone()))
        .route("/v1/fleet/{*operation}", any(respond))
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
            "The fleet request is invalid.",
            "Run elsecall fleet --help for the expected arguments.",
        )
    })
}

async fn view(State(viewing): State<Viewing>) -> Response {
    respond(Ok(fleet_view::view(&viewing.fleet, &*viewing.placed).await))
}
async fn builds(State(viewing): State<Viewing>) -> Response {
    respond(Ok(fleet_view::view(&viewing.fleet, &*viewing.placed)
        .await
        .builds()))
}
async fn identity(State(fleet): State<Arc<Fleet>>) -> Response {
    respond(fleet.identity())
}
async fn enroll(State(fleet): State<Arc<Fleet>>, body: Bytes) -> Response {
    match parse::<CodeRequest>(&body) {
        Ok(request) => respond(fleet_changes::enroll(&fleet, &request.code, request.force).await),
        Err(error) => respond::<()>(Err(error)),
    }
}
async fn join(State(fleet): State<Arc<Fleet>>, body: Bytes) -> Response {
    match parse::<CodeRequest>(&body) {
        Ok(request) => respond(fleet_changes::join(&fleet, &request.code).await),
        Err(error) => respond::<()>(Err(error)),
    }
}
async fn remove(State(fleet): State<Arc<Fleet>>, body: Bytes) -> Response {
    match parse::<MemberRequest>(&body) {
        Ok(request) => respond(fleet_changes::remove(&fleet, &request.name, request.force).await),
        Err(error) => respond::<()>(Err(error)),
    }
}
async fn leave(State(fleet): State<Arc<Fleet>>, body: Bytes) -> Response {
    match parse::<ForceRequest>(&body) {
        Ok(request) => respond(fleet_changes::leave(&fleet, request.force).await),
        Err(error) => respond::<()>(Err(error)),
    }
}
async fn sync(State(fleet): State<Arc<Fleet>>, body: Bytes) -> Response {
    match parse::<SyncRequest>(&body) {
        Ok(request) => {
            respond(fleet_changes::sync(&fleet, request.force, request.override_newer).await)
        }
        Err(error) => respond::<()>(Err(error)),
    }
}
async fn move_master(State(fleet): State<Arc<Fleet>>, body: Bytes) -> Response {
    match parse::<MemberRequest>(&body) {
        Ok(request) => {
            respond(fleet_changes::move_master(&fleet, &request.name, request.force).await)
        }
        Err(error) => respond::<()>(Err(error)),
    }
}
async fn probe_placement(State(fleet): State<Arc<Fleet>>, body: Bytes) -> Response {
    match parse::<MemberRequest>(&body) {
        Ok(request) => respond(fleet_changes::probe_placement(&fleet, &request.name).await),
        Err(error) => respond::<()>(Err(error)),
    }
}
