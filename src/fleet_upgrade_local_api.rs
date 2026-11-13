//! `elsecall fleet upgrade` on the local socket, where the same-user rule
//! authorizes the operator. The service holds the member key that signs the build.
use crate::{
    api,
    failure::Failure,
    fleet::Fleet,
    fleet_upgrade::master::{self, UpgradeRequest},
};
use axum::{
    Json, Router,
    body::Bytes,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use std::{path::PathBuf, sync::Arc};

#[derive(Clone)]
struct Distributing {
    fleet: Arc<Fleet>,
    /// The master's installed executable, distributed unless another is named.
    installed: PathBuf,
    /// One upgrade at a time from this member.
    running: Arc<tokio::sync::Mutex<()>>,
}

pub fn routes(fleet: Arc<Fleet>, installed: PathBuf) -> Router {
    Router::new()
        .route("/v1/fleet/upgrade", post(upgrade))
        .with_state(Distributing {
            fleet,
            installed,
            running: Arc::default(),
        })
}

fn refuse(error: Failure) -> Response {
    let status = if error.code == "invalid_request" {
        StatusCode::BAD_REQUEST
    } else {
        StatusCode::CONFLICT
    };
    (status, Json(api::ErrorResponse::new(error))).into_response()
}

async fn upgrade(State(distributing): State<Distributing>, body: Bytes) -> Response {
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
    let Ok(_running) = distributing.running.try_lock() else {
        return refuse(Failure::new(
            "upgrade_in_progress",
            "This master is already distributing a build.",
            "Wait for that elsecall fleet upgrade to finish, then retry.",
        ));
    };
    match master::upgrade(&distributing.fleet, distributing.installed.clone(), request).await {
        Ok(report) => Json(api::Response::new(report)).into_response(),
        Err(error) => refuse(error),
    }
}
