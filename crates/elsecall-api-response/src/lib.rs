//! How a slice's HTTP routes answer with a failure: the error envelope every
//! Elsecall API returns, with the status that fits it.
use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use elsecall_contract::api;
use elsecall_contract::failure::Failure;

pub fn failure(status: StatusCode, error: Failure) -> Response {
    (status, Json(api::ErrorResponse::new(error))).into_response()
}
