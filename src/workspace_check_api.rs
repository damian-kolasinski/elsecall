//! The member API route through which a member holding copies asks their owner
//! whether its workspaces still exist, and tells it which copies it removed.
//! Authentication names the asking member; the owner answers only about
//! workspaces it pushed to that member.
use crate::{
    failure::Failure,
    fleet::{AuthenticatedPeer, Fleet},
    housekeeping::{
        caller_state::CallerState,
        exchange::{Question, ROUTE},
    },
    peer_api::failure,
};
use axum::{
    Extension, Json, Router,
    body::Bytes,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use std::sync::Arc;

pub fn routes(state: Arc<CallerState>) -> Router<Arc<Fleet>> {
    Router::new()
        .route(ROUTE, post(check))
        .layer(Extension(state))
}

async fn check(
    State(fleet): State<Arc<Fleet>>,
    Extension(peer): Extension<AuthenticatedPeer>,
    Extension(state): Extension<Arc<CallerState>>,
    body: Bytes,
) -> Response {
    // An enrolled member is still untrusted input.
    let question = match serde_json::from_slice::<Question>(&body) {
        Ok(question) if question.validate().is_ok() => question,
        _ => {
            return failure(
                StatusCode::BAD_REQUEST,
                Failure::new(
                    "invalid_request",
                    "The workspace question is invalid.",
                    "Install the same Elsecall version on every member.",
                ),
            );
        }
    };
    let asker = match fleet.roster() {
        Ok(roster) => match roster.member_by_key(&peer.key) {
            Some(member) => member.name.to_string(),
            None => return StatusCode::UNAUTHORIZED.into_response(),
        },
        Err(error) => return failure(StatusCode::SERVICE_UNAVAILABLE, error),
    };
    let answer = tokio::task::spawn_blocking(move || state.answer(&asker, &question))
        .await
        .expect("answers do not panic");
    Json(crate::api::Response::new(answer)).into_response()
}
