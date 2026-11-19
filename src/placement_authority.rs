//! The authorization boundary for placement authority: requests that only the
//! recorded master may make. Placement itself arrives in later steps; a probe proves
//! the boundary.
use axum::{
    Json,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use elsecall_api_response::failure;
use elsecall_fleet::fleet::{Fleet, PlacementAccepted, PlacementAuthority};
use std::sync::Arc;

/// Accepts only the recorded master. Placement handlers take `PlacementAuthority`
/// the same way.
pub async fn probe(State(fleet): State<Arc<Fleet>>, authority: PlacementAuthority) -> Response {
    match fleet.status().await {
        Ok(status) => Json(elsecall_wire::api::Response::new(PlacementAccepted {
            member: status.member,
            accepted_from: authority.master().clone(),
        }))
        .into_response(),
        Err(error) => failure(StatusCode::SERVICE_UNAVAILABLE, error),
    }
}
