//! What `elsecall tools provisioning` prints: the loop's view of each member.
use crate::{provisioning_loop::tracking::Tracked, tool_inventory::Manager};
use elsecall_contract::{failure::Failure, scheduling_model::timestamp::Timestamp};
use elsecall_peers::roster::{MemberName, Readiness};
use serde::Serialize;

#[derive(Debug, Serialize)]
pub(crate) struct Report {
    pub(crate) master: MemberName,
    pub(crate) following: bool,
    pub(crate) members: Vec<MemberReport>,
}

#[derive(Debug, Serialize)]
pub(crate) struct MemberReport {
    member: MemberName,
    state: &'static str,
    readiness: Readiness,
    planned_at: Option<Timestamp>,
    signal_at: Option<Timestamp>,
    plan: Option<PlanCounts>,
    held: Vec<Held>,
    error: Option<Failure>,
}

#[derive(Debug, Serialize)]
struct PlanCounts {
    steps: usize,
    unresolved: usize,
}

#[derive(Debug, Serialize)]
struct Held {
    manager: Manager,
    tool: String,
    version: String,
    code: String,
}

impl MemberReport {
    pub(crate) fn of(
        member: &MemberName,
        master: bool,
        readiness: Readiness,
        tracked: Option<&Tracked>,
    ) -> Self {
        let planned = tracked.and_then(|tracked| tracked.planned.as_ref());
        Self {
            member: member.clone(),
            state: match tracked {
                _ if master => "master",
                None => "unknown",
                Some(tracked) => tracked.state(),
            },
            readiness,
            planned_at: planned.map(|planned| planned.at),
            signal_at: tracked.and_then(|tracked| tracked.signal_at),
            plan: planned.map(|planned| PlanCounts {
                steps: planned.steps,
                unresolved: planned.unresolved,
            }),
            held: planned
                .iter()
                .flat_map(|planned| &planned.held)
                .map(|held| Held {
                    manager: held.step.manager,
                    tool: held.step.tool.as_str().to_owned(),
                    version: held.step.version.as_str().to_owned(),
                    code: held.code.clone(),
                })
                .collect(),
            error: tracked.and_then(|tracked| {
                tracked
                    .unreachable
                    .as_ref()
                    .or(tracked.error.as_ref())
                    .cloned()
            }),
        }
    }
}
