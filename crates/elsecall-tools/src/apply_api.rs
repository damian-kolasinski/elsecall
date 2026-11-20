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
        record::{Record, WireRecord},
        request::{ApplyRequest, InstallStep, WireStep},
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
use elsecall_peers::{peer_client, roster::MemberName};
use hyper::Method;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};

pub const ROUTE: &str = "/v1/tools/apply";
pub const PEER_ROUTE: &str = "/peer/v1/tools/apply";
/// Starting an apply is writing a file; reading one is reading it.
const PEER_TIMEOUT: Duration = Duration::from_secs(20);

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
    Router::new().route(
        PEER_ROUTE,
        post(
            move |_master: ProvisioningAuthority, Json(request): Json<ApplyRequest>| async move {
                let steps = match request.accept() {
                    Ok(steps) => steps,
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
                match applier.start(steps) {
                    Ok(record) => Json(api::Response::new(Applied {
                        apply: Some(WireRecord::of(&record)),
                    }))
                    .into_response(),
                    Err(error) => elsecall_api_response::failure(StatusCode::CONFLICT, error),
                }
            },
        )
        .get(move || async move {
            Json(api::Response::new(Applied {
                apply: reading.latest().as_ref().map(WireRecord::of),
            }))
            .into_response()
        }),
    )
}

/// A member's latest apply, as its route answers.
#[derive(Serialize, Deserialize)]
struct Applied {
    apply: Option<WireRecord>,
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

async fn apply(applying: &Applying, body: &ApplyBody) -> Result<Value> {
    let (document, members) = plan_api::gather(&applying.planning, body.member.as_deref()).await?;
    let fleet = applying.planning.fleet()?;
    let sends = document.members.iter().map(|(member, plan)| {
        let reach = members.iter().find(|reach| &reach.name == member);
        let fleet = Arc::clone(&fleet);
        let applier = Arc::clone(&applying.applier);
        async move {
            let plan = match plan {
                Err(error) => {
                    return json!({"member": member, "state": "unavailable", "error": error});
                }
                Ok(plan) => plan,
            };
            let steps: Vec<InstallStep> = plan.steps.iter().map(InstallStep::of).collect();
            if steps.is_empty() {
                return json!({"member": member, "state": "nothing_to_install",
                              "unresolved": plan.unresolved.len()});
            }
            let started = match reach.and_then(|reach| reach.peer.as_ref()) {
                None => applier
                    .start(steps)
                    .map(|record| Some(WireRecord::of(&record))),
                Some(peer) => peer_client::call::<Applied>(
                    fleet.key(),
                    peer,
                    Method::POST,
                    PEER_ROUTE,
                    Some(&ApplyRequest {
                        steps: steps.iter().map(WireStep::of).collect(),
                    }),
                    PEER_TIMEOUT,
                )
                .await
                .map(|applied| applied.apply),
            };
            match started {
                Ok(apply) => json!({"member": member, "state": "started", "apply": apply,
                                    "unresolved": plan.unresolved.len()}),
                Err(error) => json!({"member": member, "state": "refused",
                                     "error": refusal(member, &error)}),
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
            let latest: Result<Option<Record>> = match &reach.peer {
                None => Ok(applier.latest()),
                Some(peer) => peer_client::call::<Applied>(
                    fleet.key(),
                    peer,
                    Method::GET,
                    PEER_ROUTE,
                    None::<&()>,
                    PEER_TIMEOUT,
                )
                .await
                .map_err(|error| refusal(&reach.name, &error))
                .and_then(|applied| match applied.apply {
                    None => Ok(None),
                    Some(wire) => wire.accept().map(Some).ok_or_else(|| {
                        Failure::new(
                            "protocol_error",
                            format!("{} reported an apply this build cannot read.", reach.name),
                            "Run elsecall fleet upgrade on the master so every member runs its build.",
                        )
                    }),
                }),
            };
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

/// Another member's refusal, with text written here; only the code is its own.
fn refusal(member: &MemberName, failure: &Failure) -> Failure {
    let code = crate::fleet_tool_inventory::unavailable_member(member, failure).code;
    match code.as_str() {
        "apply_in_progress" => Failure::new(
            "apply_in_progress",
            format!("{member} is already installing a plan."),
            format!(
                "Wait for it with elsecall tools applied --member {member} --wait, then apply again."
            ),
        ),
        "version_skew" | "not_found" => Failure::new(
            &code,
            format!("{member} runs another build and cannot install the plan."),
            "Run elsecall fleet upgrade on the master so every member runs its build, then retry.",
        ),
        _ => Failure::new(
            &code,
            format!("{member} could not be asked to install its plan."),
            format!("Check {member} with elsecall fleet (elsecall doctor there), then retry."),
        ),
    }
}
