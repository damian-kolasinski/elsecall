//! The authorization boundary for placement authority: requests that only the
//! recorded master may make. Placement itself arrives in later steps; a probe proves
//! the boundary.
use crate::{
    fleet::{AuthenticatedPeer, Fleet, PlacementAccepted, PlacementAuthority},
    peer_api::failure,
};
use axum::{
    Json,
    extract::{FromRequestParts, State},
    http::{StatusCode, request::Parts},
    response::{IntoResponse, Response},
};
use std::sync::Arc;

impl FromRequestParts<Arc<Fleet>> for PlacementAuthority {
    type Rejection = Response;
    async fn from_request_parts(parts: &mut Parts, fleet: &Arc<Fleet>) -> Result<Self, Response> {
        // The member API authenticates every request before any handler runs.
        let peer = parts
            .extensions
            .get::<AuthenticatedPeer>()
            .expect("member requests are authenticated before extraction");
        fleet
            .authorize_placement(&peer.key)
            .map_err(|error| failure(StatusCode::FORBIDDEN, error))
    }
}

/// Accepts only the recorded master. Placement handlers take `PlacementAuthority`
/// the same way.
pub async fn probe(State(fleet): State<Arc<Fleet>>, authority: PlacementAuthority) -> Response {
    match fleet.status().await {
        Ok(status) => Json(crate::api::Response::new(PlacementAccepted {
            member: status.member,
            accepted_from: authority.master().clone(),
        }))
        .into_response(),
        Err(error) => failure(StatusCode::SERVICE_UNAVAILABLE, error),
    }
}
