//! Whether outside pressure blocks this member, and which outside processes it
//! warns about, on the local socket, for doctor on a member that is not in a
//! fleet and for agents asking this member alone.
use crate::{
    api,
    failure::Failure,
    outside_pressure::{BlockedReport, Warning, WarningReport},
};
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub const PATH: &str = "/v1/outside-pressure";

/// This member's blocked state; `blocked` is null while outside pressure does
/// not hold its gate closed.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ThisMember {
    pub member: String,
    #[serde(
        default,
        deserialize_with = "crate::outside_pressure::blocked::lenient"
    )]
    pub blocked: Option<crate::outside_pressure::Blocked>,
    /// Early warnings about outside processes, the longest-standing first;
    /// empty from services that predate them.
    #[serde(
        default,
        deserialize_with = "crate::outside_pressure::blocked::lenient_list"
    )]
    pub warnings: Vec<Warning>,
}

type MemberName = Arc<dyn Fn() -> String + Send + Sync>;
type Reports = (
    Arc<dyn BlockedReport>,
    Option<Arc<dyn WarningReport>>,
    MemberName,
);

/// `warnings` is `None` when early warnings could not start; the service log
/// says why.
pub fn routes(
    report: Arc<dyn BlockedReport>,
    warnings: Option<Arc<dyn WarningReport>>,
    member: MemberName,
) -> Router {
    Router::new()
        .route(PATH, get(this_member))
        .with_state((report, warnings, member))
}

/// The route for a service whose detection could not start.
pub fn unavailable(error: Failure) -> Router {
    Router::new().route(
        PATH,
        get(move || async move {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(api::ErrorResponse::new(error)),
            )
        }),
    )
}

async fn this_member(State((report, warnings, member)): State<Reports>) -> Response {
    Json(api::Response::new(ThisMember {
        member: member(),
        blocked: report.blocked(),
        warnings: warnings.map(|w| w.warnings()).unwrap_or_default(),
    }))
    .into_response()
}
