//! Sending a member its apply, and reading back its latest one: on this member
//! through its own applier, on another over the member route
//! (`POST`/`GET /peer/v1/tools/apply`). `tools apply` and the provisioning loop
//! both send through here.
use crate::{
    apply_api::PEER_ROUTE,
    install::{
        Applier,
        record::{Record, WireRecord},
        request::{ApplyRequest, HeldStep, InstallStep, MAXIMUM_STEPS, WireStep},
    },
    plan_api::Reach,
};
use elsecall_contract::failure::{Failure, Result};
use elsecall_fleet::fleet::Fleet;
use elsecall_peers::{peer_client, roster::MemberName};
use hyper::Method;
use serde::{Deserialize, Serialize};
use std::{sync::Arc, time::Duration};

/// Starting an apply is writing a file; reading one is reading it.
const PEER_TIMEOUT: Duration = Duration::from_secs(20);
/// What fits the member listener's 64 KiB body, with room for its framing.
const MAXIMUM_BODY: usize = 60 * 1024;

/// A member's latest apply, as its route answers.
#[derive(Serialize, Deserialize)]
pub(crate) struct Applied {
    pub(crate) apply: Option<WireRecord>,
}

/// Starts `reach`'s apply of `steps`, holding `held`: on this member through
/// its own applier, on another over the member route. Its new apply, as it
/// started.
pub(crate) async fn send(
    fleet: &Arc<Fleet>,
    applier: &Arc<Applier>,
    reach: &Reach,
    steps: Vec<InstallStep>,
    held: Vec<HeldStep>,
) -> Result<Option<WireRecord>> {
    let request = ApplyRequest {
        steps: steps.iter().map(WireStep::of).collect(),
        held: held.iter().map(HeldStep::wire).collect(),
    };
    within_bounds(&reach.name, &request)?;
    match &reach.peer {
        None => applier
            .start(steps, held)
            .map(|record| Some(WireRecord::of(&record))),
        Some(peer) => peer_client::call::<Applied>(
            fleet.key(),
            peer,
            Method::POST,
            PEER_ROUTE,
            Some(&request),
            PEER_TIMEOUT,
        )
        .await
        .map(|applied| applied.apply)
        .map_err(|error| refusal(&reach.name, &error)),
    }
}

/// `reach`'s latest apply, none when it never applied a plan.
pub(crate) async fn latest(
    fleet: &Arc<Fleet>,
    applier: &Applier,
    reach: &Reach,
) -> Result<Option<Record>> {
    match &reach.peer {
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
    }
}

/// A member accepts at most 256 steps in a body of at most 64 KiB; a plan
/// beyond that is refused here rather than by the member's listener.
fn within_bounds(member: &MemberName, request: &ApplyRequest) -> Result<()> {
    let bytes = serde_json::to_vec(request)
        .expect("a request serializes")
        .len();
    let steps = request.steps.len() + request.held.len();
    if steps > MAXIMUM_STEPS || bytes > MAXIMUM_BODY {
        return Err(Failure::new(
            "plan_too_large",
            format!("{member}'s plan has {steps} steps, more than one apply sends."),
            format!(
                "Leave tools out with elsecall tools leave-out --member {member}, apply, then unset them and apply again."
            ),
        ));
    }
    Ok(())
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
