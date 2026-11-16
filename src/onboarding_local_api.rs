//! Onboarding routes are merged only into the authenticated same-user Unix socket.
use crate::{
    api,
    failure::Failure,
    onboarding_id::OperationId,
    onboarding_operation::{self, Operations},
};
use axum::{
    Json, Router,
    body::Bytes,
    extract::rejection::BytesRejection,
    extract::{DefaultBodyLimit, Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use std::{path::PathBuf, sync::Arc};

pub fn routes(directory: PathBuf) -> Router {
    Router::new()
        .route("/v1/onboarding/prepare", post(prepare))
        .route("/v1/onboarding/{id}", get(status))
        .route("/v1/onboarding/{id}/cancel", post(cancel))
        .layer(DefaultBodyLimit::max(1024))
        .with_state(Arc::new(Operations::new(directory)))
}
fn request(body: Result<Bytes, BytesRejection>) -> crate::failure::Result<()> {
    let body = body.map_err(|error| {
        if error.status() == StatusCode::PAYLOAD_TOO_LARGE {
            Failure::new(
                "onboarding_request_too_large",
                "The onboarding request exceeds 1 KiB.",
                "Send {}.",
            )
        } else {
            Failure::new(
                "invalid_request",
                "The onboarding request could not be read.",
                "Send {}.",
            )
        }
    })?;
    match serde_json::from_slice::<serde_json::Value>(&body) {
        Ok(serde_json::Value::Object(fields)) if fields.is_empty() => Ok(()),
        _ => Err(Failure::new(
            "invalid_request",
            "The onboarding request must be an empty JSON object.",
            "Send {}.",
        )),
    }
}
fn respond(result: crate::failure::Result<crate::onboarding_operation::Operation>) -> Response {
    match result {
        Ok(operation) => Json(api::Response::new(operation)).into_response(),
        Err(error) => {
            let status = match error.code.as_str() {
                "onboarding_operation_not_found" => StatusCode::NOT_FOUND,
                "onboarding_request_too_large" => StatusCode::PAYLOAD_TOO_LARGE,
                "invalid_request" | "invalid_onboarding_id" => StatusCode::BAD_REQUEST,
                _ => StatusCode::CONFLICT,
            };
            (status, Json(api::ErrorResponse::new(error))).into_response()
        }
    }
}
async fn prepare(
    State(operations): State<Arc<Operations>>,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    respond(request(body).and_then(|_| operations.prepare(onboarding_operation::now())))
}
async fn status(State(operations): State<Arc<Operations>>, Path(id): Path<String>) -> Response {
    respond(
        OperationId::try_from(id)
            .and_then(|id| operations.status(&id, onboarding_operation::now(), false)),
    )
}
async fn cancel(
    State(operations): State<Arc<Operations>>,
    Path(id): Path<String>,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    respond(
        request(body)
            .and_then(|_| OperationId::try_from(id))
            .and_then(|id| operations.status(&id, onboarding_operation::now(), true)),
    )
}
