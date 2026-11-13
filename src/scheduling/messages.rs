//! Messages between the CLI and the service about admitted work.
use crate::{
    inspection::LoadSample,
    scheduling::{
        admission::QueueCounts,
        capacity::{Capacity, Cores},
        load::Gate,
        record::Record,
        request::Kind,
        run_id::RunId,
        run_phases::{RemoteFailurePhase, RunPhases},
    },
};
use serde::{Deserialize, Serialize};

/// Capacity granted to a run or lease.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Grant {
    pub run_id: RunId,
    pub member: String,
    pub size: Cores,
    /// The grant this one runs inside; it then uses no capacity of its own.
    pub nested_in: Option<RunId>,
    pub waited_ms: u64,
    pub placement_note: Option<String>,
    /// Set when the work runs on another member.
    #[serde(default)]
    pub placed: Option<crate::scheduling::placed::PlacedGrant>,
    /// The holder already held this lease, so acquiring again returned it.
    #[serde(default)]
    pub reused: bool,
}

/// The process that runs an admitted command, reported by the run's caller.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandStarted {
    pub pid: u32,
}

/// How an admitted run ended, reported by its caller.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum RunEnded {
    Exited {
        exit_code: i32,
        /// CPU time the command used, when it could be measured.
        #[serde(default)]
        cpu_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run_phases: Option<RunPhases>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        plan_observation: Option<crate::workload_metadata::Observation>,
    },
    Signaled {
        signal: i32,
        #[serde(default)]
        cpu_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run_phases: Option<RunPhases>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        plan_observation: Option<crate::workload_metadata::Observation>,
    },
    /// Elsecall failed the run after admission, such as a command that could not start.
    Failed {
        code: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        remote_failure_phase: Option<RemoteFailurePhase>,
    },
    PeerFailed {
        failure: crate::remote_run::protocol::StreamFailure,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        remote_failure_phase: Option<RemoteFailurePhase>,
    },
}

impl RunEnded {
    pub fn without_phases(&self) -> Option<Self> {
        let mut legacy = self.clone();
        let had_phases = match &mut legacy {
            Self::Exited { run_phases, .. } | Self::Signaled { run_phases, .. } => {
                run_phases.take().is_some()
            }
            Self::Failed {
                remote_failure_phase,
                ..
            }
            | Self::PeerFailed {
                remote_failure_phase,
                ..
            } => remote_failure_phase.take().is_some(),
        };
        had_phases.then_some(legacy)
    }

    pub fn failure_phase(&self) -> Option<RemoteFailurePhase> {
        match self {
            Self::Failed {
                remote_failure_phase,
                ..
            }
            | Self::PeerFailed {
                remote_failure_phase,
                ..
            } => *remote_failure_phase,
            _ => None,
        }
    }
}

/// Releases one lease by ID, or every lease of a holder process.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Release {
    #[serde(default)]
    pub run_id: Option<RunId>,
    #[serde(default)]
    pub holder_pid: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Released {
    pub released: Vec<RunId>,
}

/// This member's admission state now.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MemberState {
    pub member: String,
    pub capacity: Capacity,
    pub in_use: Cores,
    pub free: Cores,
    pub queued: QueueCounts,
    pub load: Option<LoadSample>,
    pub gate: Gate,
    /// One core's measured speed; `None` until the first measurement. A value
    /// another member reports outside a plausible range is dropped.
    #[serde(default, deserialize_with = "crate::scheduling::speed::plausible")]
    pub speed: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Runs {
    pub member: MemberState,
    pub runs: Vec<RunSummary>,
}

/// A record without its decision inputs; `explain` shows those.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunSummary {
    pub run_id: RunId,
    pub kind: Kind,
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workload: Option<crate::workload_metadata::Workload>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_observation: Option<crate::workload_metadata::Observation>,
    pub size: Cores,
    pub urgency: String,
    pub state: crate::scheduling::record::State,
    pub outcome: Option<crate::scheduling::record::Outcome>,
    pub member: Option<String>,
    pub nested_in: Option<RunId>,
    pub requested_at: crate::scheduling::timestamp::Timestamp,
    pub waited_ms: Option<u64>,
    pub run_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_phases: Option<RunPhases>,
    #[serde(default = "unknown")]
    pub calling_member: String,
    /// What the caller asked for: `pinned`, `placeable` or a required member.
    #[serde(default = "pinned")]
    pub placement: crate::scheduling::request::Placement,
    /// Set when the work ran on another member than the one that asked for it.
    #[serde(default)]
    pub placed: Option<PlacedSummary>,
    /// Why it ran where it did, or why it did not run.
    #[serde(default)]
    pub reason: Option<String>,
}

fn unknown() -> String {
    "unknown".into()
}
fn pinned() -> crate::scheduling::request::Placement {
    crate::scheduling::request::Placement::Pinned
}

/// A placed run in `elsecall runs`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlacedSummary {
    pub side: crate::scheduling::placed::Side,
    pub calling_member: String,
    pub member: String,
    pub warmth: Option<crate::scheduling::placed::Warmth>,
    pub reached: crate::scheduling::placed::Reached,
    pub sync_ms: Option<u64>,
    /// How long bringing the member's synced configuration up to date took.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_sync_ms: Option<u64>,
}
impl From<Record> for RunSummary {
    fn from(record: Record) -> Self {
        Self {
            run_id: record.run_id,
            kind: record.kind,
            label: record.label,
            workload: record.workload,
            plan_observation: record.plan_observation,
            size: record.size,
            urgency: match record.urgency {
                crate::scheduling::request::Urgency::Interactive { .. } => "interactive",
                crate::scheduling::request::Urgency::Deferrable { .. } => "deferrable",
            }
            .into(),
            state: record.state,
            outcome: record.outcome,
            member: record.member,
            nested_in: record.nested_in,
            requested_at: record.requested_at,
            waited_ms: record.waited_ms,
            run_ms: record.run_ms,
            run_phases: record.run_phases,
            calling_member: record.calling_member,
            placement: record.placement,
            reason: record.decision.map(|d| d.reason).or(record.placement_note),
            placed: record.placed.map(|p| PlacedSummary {
                config_sync_ms: record.config_sync.as_ref().map(|s| s.duration_ms),
                side: p.side,
                calling_member: p.calling_member,
                member: p.member,
                warmth: p.warmth,
                reached: p.reached,
                sync_ms: p.sync.and_then(|s| s.duration_ms),
            }),
        }
    }
}
