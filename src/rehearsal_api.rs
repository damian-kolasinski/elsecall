//! Local master orchestration and enrolled, master-authorized member operations.
use crate::{
    api,
    failure::Result,
    fleet::{Fleet, PlacementAuthority},
    peer_client,
    rehearsal::{
        invalid,
        member::Member,
        protocol::{Action, Call, Reply},
    },
};
use axum::{
    Extension, Json, Router,
    body::Bytes,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use hyper::Method;
use std::{sync::Arc, time::Duration};
#[derive(Clone)]
struct Local {
    fleet: Arc<Fleet>,
    member: Arc<Member>,
}
pub fn routes(fleet: Arc<Fleet>, member: Arc<Member>) -> Router {
    Router::new()
        .route("/v1/rehearsal", post(local))
        .with_state(Local { fleet, member })
}
pub fn peer_routes(member: Arc<Member>) -> Router<Arc<Fleet>> {
    Router::new()
        .route("/peer/v1/rehearsal", post(peer))
        .layer(Extension(member))
}
fn answer(result: Result<Reply>) -> Response {
    match result {
        Ok(data) => Json(api::Response::new(data)).into_response(),
        Err(error) => (StatusCode::CONFLICT, Json(api::ErrorResponse::new(error))).into_response(),
    }
}
async fn local(State(state): State<Local>, body: Bytes) -> Response {
    answer(
        async {
            let request: Call = serde_json::from_slice(&body)
                .map_err(|_| invalid("Invalid rehearsal operation."))?;
            let roster = state.fleet.roster()?;
            if state.fleet.name_in(&roster) != roster.master().name {
                return Err(crate::failure::Failure::new(
                    "not_master",
                    "Rehearsal must be orchestrated on the master.",
                    "Run elsecall rehearse on the recorded master.",
                ));
            }
            if request.member == state.fleet.name_in(&roster).as_str() {
                return state.member.call(request.operation).await;
            }
            let member = roster
                .members()
                .iter()
                .find(|m| m.name.as_str() == request.member)
                .ok_or_else(|| invalid("The rehearsal member is not enrolled."))?;
            let retry = if matches!(
                request.operation,
                Action::Inspect { .. }
                    | Action::Poll { .. }
                    | Action::RecordedLoad { .. }
                    | Action::Trust
            ) {
                peer_client::Retry::Idempotent
            } else {
                peer_client::Retry::Never
            };
            peer_client::call_with_retry(
                state.fleet.member_key(),
                member,
                Method::POST,
                "/peer/v1/rehearsal",
                Some(&request.operation),
                Duration::from_secs(30),
                retry,
            )
            .await
        }
        .await,
    )
}
async fn peer(
    _authority: PlacementAuthority,
    Extension(member): Extension<Arc<Member>>,
    body: Bytes,
) -> Response {
    answer(
        async {
            let action: Action = serde_json::from_slice(&body)
                .map_err(|_| invalid("Invalid rehearsal operation."))?;
            member.call(action).await
        }
        .await,
    )
}
