//! Applying the plan over HTTP, and the member routes that carry it out.
//!
//! `POST /v1/tools/apply` runs on the master. It plans the fleet as
//! `GET /v1/tools/plan` does and sends each planned member its steps
//! (`POST /peer/v1/tools/apply`); the master's own steps go to its own
//! [`Applier`]. A member answers as soon as its apply has started; the steps
//! then run there as its own admitted work. A dry run is the plan route itself.
//! `GET /v1/tools/apply` reads each member's latest apply
//! (`GET /peer/v1/tools/apply`), for `elsecall tools applied`.
//!
//! Only the master may ask a member to install ([`ProvisioningAuthority`]); any
//! enrolled member may read what it installed.
use crate::{
    install::{
        Applier,
        record::WireRecord,
        request::{ApplyRequest, InstallStep},
    },
    plan_api::{self, Planning},
};
use axum::{
    Json, Router,
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use elsecall_contract::{
    api,
    failure::{Failure, Result},
};
use elsecall_fleet::fleet::{Fleet, ProvisioningAuthority};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;

pub const ROUTE: &str = "/v1/tools/apply";
pub const PEER_ROUTE: &str = "/peer/v1/tools/apply";

struct Applying {
    planning: Planning,
    applier: Arc<Applier>,
}

pub fn routes(planning: Planning, applier: Arc<Applier>) -> Router {
    Router::new()
        .route(ROUTE, post(apply_route).get(applied_route))
        .with_state(Arc::new(Applying { planning, applier }))
}

/// The member routes, behind the member API's authentication.
pub fn peer_routes(applier: Arc<Applier>) -> Router<Arc<Fleet>> {
    let reading = Arc::clone(&applier);
    crate::artifact_api::peer_routes().route(
        PEER_ROUTE,
        post(
            move |_master: ProvisioningAuthority,
                  request: std::result::Result<
                Json<ApplyRequest>,
                axum::extract::rejection::JsonRejection,
            >| async move {
                let request = match request {
                    Ok(Json(request)) => request,
                    Err(rejection) => return unreadable(rejection),
                };
                let (steps, held) = match request.accept() {
                    Ok(accepted) => accepted,
                    Err(rule) => {
                        return elsecall_api_response::failure(
                            StatusCode::BAD_REQUEST,
                            Failure::new(
                                "invalid_request",
                                format!("The steps to install are not valid: {rule}."),
                                "Run elsecall tools apply on the master, which sends the plan's steps.",
                            ),
                        );
                    }
                };
                match applier.start(steps, held) {
                    Ok(record) => Json(api::Response::new(crate::apply_dispatch::Applied {
                        apply: Some(WireRecord::of(&record)),
                    }))
                    .into_response(),
                    Err(error) => elsecall_api_response::failure(StatusCode::CONFLICT, error),
                }
            },
        )
        .get(move || async move {
            Json(api::Response::new(crate::apply_dispatch::Applied {
                apply: reading.latest().as_ref().map(WireRecord::of),
            }))
            .into_response()
        }),
    )
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ApplyBody {
    #[serde(default)]
    pub(crate) member: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AppliedQuery {
    #[serde(default)]
    member: Option<String>,
}

async fn apply_route(
    State(applying): State<Arc<Applying>>,
    Json(body): Json<ApplyBody>,
) -> Response {
    respond(apply(&applying, &body).await)
}

/// A body that is not an apply request, answered with the API's own failure.
fn unreadable(_: axum::extract::rejection::JsonRejection) -> Response {
    elsecall_api_response::failure(
        StatusCode::BAD_REQUEST,
        Failure::new(
            "invalid_request",
            "The steps to install are not valid.",
            "Run elsecall tools apply on the master, which sends the plan's steps.",
        ),
    )
}

async fn applied_route(
    State(applying): State<Arc<Applying>>,
    Query(query): Query<AppliedQuery>,
) -> Response {
    respond(applied(&applying, query.member.as_deref()).await)
}

fn respond(result: Result<Value>) -> Response {
    match result {
        Ok(value) => Json(api::Response::new(value)).into_response(),
        Err(failure) => {
            let status = match failure.code.as_str() {
                "invalid_request" | "invalid_fleet_tools" => StatusCode::BAD_REQUEST,
                "not_master" => StatusCode::CONFLICT,
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            };
            (status, Json(api::ErrorResponse::new(failure))).into_response()
        }
    }
}

/// Plans, then starts every planned member's apply, the ones with nothing to
/// install too, so their last failures stop standing. It runs on its own task:
/// a caller that gives up never leaves some members asked and others not.
async fn apply(applying: &Arc<Applying>, body: &ApplyBody) -> Result<Value> {
    let applying = Arc::clone(applying);
    let member = body.member.clone();
    tokio::spawn(async move { start_all(&applying, member.as_deref()).await })
        .await
        .map_err(|_| {
            Failure::new(
                "tools_failed",
                "Applying the plan stopped unexpectedly.",
                "Run elsecall tools applied to see what started, then retry.",
            )
        })?
}

async fn start_all(applying: &Applying, only: Option<&str>) -> Result<Value> {
    let (document, members) = plan_api::gather(&applying.planning, only).await?;
    let fleet = applying.planning.fleet()?;
    let sends = document.members.iter().map(|(member, plan)| {
        let reach = members.iter().find(|reach| &reach.name == member);
        let fleet = Arc::clone(&fleet);
        let applier = Arc::clone(&applying.applier);
        async move {
            let (plan, reach) = match (plan, reach) {
                (Err(error), _) => {
                    return json!({"member": member, "state": "unavailable", "error": error});
                }
                (Ok(_), None) => {
                    return json!({"member": member, "state": "refused", "error": Failure::new(
                        "member_unreachable",
                        format!("{member} is not in this member's roster."),
                        "Run elsecall fleet on the master, then retry.",
                    )});
                }
                (Ok(plan), Some(reach)) => (plan, reach),
            };
            let steps: Vec<InstallStep> = plan.steps.iter().map(InstallStep::of).collect();
            let empty = steps.is_empty();
            let started =
                crate::apply_dispatch::send(&fleet, &applier, reach, steps, Vec::new()).await;
            let unresolved = plan.unresolved.len();
            match started {
                Ok(_) if empty => json!({"member": member,
                    "state": "nothing_to_install", "unresolved": unresolved}),
                Ok(apply) => json!({"member": member, "state": "started", "apply": apply,
                                    "unresolved": unresolved}),
                Err(error) => json!({"member": member, "state": "refused", "error": error}),
            }
        }
    });
    let members = futures_util::future::join_all(sends).await;
    Ok(json!({"master": document.master, "members": members}))
}

async fn applied(applying: &Applying, only: Option<&str>) -> Result<Value> {
    let fleet = applying.planning.fleet()?;
    let members = plan_api::members_to_read(&fleet, only)?;
    let reads = members.iter().map(|reach| {
        let fleet = Arc::clone(&fleet);
        let applier = Arc::clone(&applying.applier);
        async move {
            let latest = crate::apply_dispatch::latest(&fleet, &applier, reach).await;
            match latest {
                Ok(None) => json!({"member": reach.name, "state": "none"}),
                Ok(Some(record)) => json!({
                    "member": reach.name,
                    "state": if record.running() { "running" } else { "finished" },
                    "apply": WireRecord::of(&record),
                }),
                Err(error) => json!({"member": reach.name, "state": "unavailable", "error": error}),
            }
        }
    });
    Ok(json!({"members": futures_util::future::join_all(reads).await}))
}
