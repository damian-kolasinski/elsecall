//! `elsecall fleet upgrade` on the local socket, where the same-user rule
//! authorizes the operator. The service holds the member key that signs the build.
use crate::fleet_upgrade::{distributor::Distributor, master::UpgradeRequest};
use axum::{
    Json, Router,
    body::Bytes,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use elsecall_failure::Failure;
use elsecall_wire::api;
use std::sync::Arc;

pub fn routes(distributor: Arc<Distributor>) -> Router {
    Router::new()
        .route("/v1/fleet/upgrade", post(upgrade))
        .with_state(distributor)
}

fn refuse(error: Failure) -> Response {
    let status = if error.code == "invalid_request" {
        StatusCode::BAD_REQUEST
    } else {
        StatusCode::CONFLICT
    };
    (status, Json(api::ErrorResponse::new(error))).into_response()
}

async fn upgrade(State(distributor): State<Arc<Distributor>>, body: Bytes) -> Response {
    let Ok(request) = serde_json::from_slice::<UpgradeRequest>(&body) else {
        return refuse(Failure::new(
            "invalid_request",
            "The upgrade request is invalid.",
            "Run elsecall fleet upgrade --help for the expected arguments.",
        ));
    };
    if request
        .executable
        .as_ref()
        .is_some_and(|path| !path.is_absolute())
    {
        return refuse(Failure::new(
            "invalid_request",
            "--executable must name an absolute path.",
            "Pass the build's full path.",
        ));
    }
    match distributor.distribute(request).await {
        Ok(report) => Json(api::Response::new(report)).into_response(),
        Err(error) => refuse(error),
    }
}
