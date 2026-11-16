//! Same-user socket routes for the staged window and app publication.
use crate::{api, paths::Paths};
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Serialize;

#[derive(Serialize)]
struct Offer {
    build: Option<String>,
}

#[derive(Serialize)]
struct Launched {
    launched: bool,
}

pub fn routes(paths: Paths) -> Router {
    Router::new()
        .route("/v1/ui", get(offer))
        .route("/v1/ui/update", post(update))
        .with_state(paths)
}

async fn offer(State(paths): State<Paths>) -> Response {
    let found = tokio::task::spawn_blocking(move || crate::window_release::installed_build(&paths))
        .await
        .expect("reading the window build does not panic");
    match found {
        Ok(build) => Json(api::Response::new(Offer {
            build: build.map(|id| id.to_string()),
        }))
        .into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(api::ErrorResponse::new(error)),
        )
            .into_response(),
    }
}

async fn update(State(paths): State<Paths>) -> Response {
    let launched = tokio::task::spawn_blocking(move || {
        crate::window_app::publish(&paths, &crate::macos::window_launcher::MacWindowLauncher)
    })
    .await
    .expect("replacing the window does not panic");
    match launched {
        Ok(()) => Json(api::Response::new(Launched { launched: true })).into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(api::ErrorResponse::new(error)),
        )
            .into_response(),
    }
}
