//! The provisioning loop over the local socket: `GET /v1/tools/provisioning`
//! is what it knows of each member, from memory; `POST` with `{member}` asks it
//! to plan and apply that member now.
use crate::provisioning_loop::{ProvisioningLoop, report::Report};
use axum::{
    Json, Router,
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use elsecall_contract::{api, failure::Result};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub const ROUTE: &str = "/v1/tools/provisioning";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProvisionBody {
    pub(crate) member: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ViewQuery {
    #[serde(default)]
    member: Option<String>,
}

pub fn routes(provisioning: Arc<ProvisioningLoop>) -> Router {
    Router::new()
        .route(
            ROUTE,
            get(
                |State(provisioning): State<Arc<ProvisioningLoop>>,
                 Query(query): Query<ViewQuery>| async move {
                    respond(provisioning.report(query.member.as_deref()))
                },
            )
            .post(
                |State(provisioning): State<Arc<ProvisioningLoop>>,
                 Json(body): Json<ProvisionBody>| async move {
                    respond(provisioning.ask(&body.member))
                },
            ),
        )
        .with_state(provisioning)
}

fn respond(result: Result<Report>) -> Response {
    match result {
        Ok(report) => Json(api::Response::new(report)).into_response(),
        Err(failure) => {
            let status = match failure.code.as_str() {
                "invalid_request" => StatusCode::BAD_REQUEST,
                "not_master" => StatusCode::CONFLICT,
                _ => StatusCode::SERVICE_UNAVAILABLE,
            };
            (status, Json(api::ErrorResponse::new(failure))).into_response()
        }
    }
}
