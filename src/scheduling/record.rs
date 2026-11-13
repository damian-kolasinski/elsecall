//! What Elsecall remembers about each request: the request, the decision and its
//! inputs, and the outcome. Never arguments, environment values or secrets.
use crate::{
    inspection::LoadSample,
    scheduling::{
        admission::QueueCounts,
        capacity::{Capacity, Cores},
        load::Gate,
        parameters::Parameters,
        processes::ProcessIdentity,
        request::{Kind, Placement, Urgency},
        run_id::RunId,
        timestamp::{Timestamp, seconds},
    },
};
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Record {
    pub run_id: RunId,
    pub kind: Kind,
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workload: Option<crate::workload_metadata::Workload>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_observation: Option<crate::workload_metadata::Observation>,
    #[serde(default)]
    pub synthetic: bool,
    /// Service-owned work, established from the local requester identity.
    #[serde(default)]
    pub internal: bool,
    /// Contextual outgoing peer failures during this request (not causal attribution).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_io: Option<crate::peer_diagnostics::History>,
    pub placement: Placement,
    pub urgency: Urgency,
    pub size: Cores,
    /// Where the size came from; absent in records from before sizes were learned.
    #[serde(default)]
    pub size_basis: Option<SizeBasis>,
    pub calling_member: String,
    /// Stable opaque working-tree/repository identities for history replay.
    /// Absent in older records and outside a working tree; never raw paths.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<WorkspaceIdentity>,
    /// True when this recording distinguishes an absent working tree from old
    /// history that never captured workspace identity.
    #[serde(default)]
    pub workspace_recorded: bool,
    /// Where the work ran; empty until it started.
    pub member: Option<String>,
    pub placement_note: Option<String>,
    /// The grant this request ran inside, if it was nested.
    pub nested_in: Option<RunId>,
    /// The outermost grant whose capacity a nested request shares.
    pub shares_capacity_of: Option<RunId>,
    pub state: State,
    pub outcome: Option<Outcome>,
    pub requested_at: Timestamp,
    pub started_at: Option<Timestamp>,
    pub ended_at: Option<Timestamp>,
    pub waited_ms: Option<u64>,
    /// Time the grant was held. For phase observations see `run_phases`.
    pub run_ms: Option<u64>,
    /// Additional run boundaries. Admission wait and workspace sync are
    /// already recorded as `waited_ms` and `placed.sync.duration_ms`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_phases: Option<crate::scheduling::run_phases::RunPhases>,
    /// CPU time the command and the descendants it waited for used, when measured.
    #[serde(default)]
    pub cpu_ms: Option<u64>,
    pub hold_limit: Option<HoldLimitRecord>,
    pub decision: Option<Decision>,
    pub warnings: Vec<String>,
    /// The `elsecall` process that made the request.
    pub requested_by: ProcessIdentity,
    /// Processes whose exit ends the request: while it waits, the requester and
    /// the holder; once it runs, the holders. Identified by PID and start time.
    pub processes: Vec<ProcessIdentity>,
    /// Set for a run placed on another member, on both members.
    #[serde(default)]
    pub placed: Option<crate::scheduling::placed::PlacedRecord>,
    /// Where the request's properties came from, including the label rule that
    /// applied; absent in records from before label rules and for hosted work.
    #[serde(default)]
    pub settings: Option<crate::scheduling::settings::Settings>,
    /// How many more times the lease's holder acquired it while it was active;
    /// each of those acquires returned this lease.
    #[serde(default)]
    pub reacquired: u32,
    /// On the master, for a run placed on another member: the configuration sync
    /// that brought the member up to date before the run, or failed it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_sync: Option<crate::scheduling::pre_run_sync::PreRunSyncRecord>,
    /// Every completed or hold-censored pre-run attempt. Missing in older records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pre_run_history: Option<crate::scheduling::pre_run_sync::History>,
    /// On the master, for a placeable run: each member whose pre-run
    /// configuration sync failed, and where placement sent the request instead,
    /// oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fallbacks: Vec<crate::scheduling::pre_run_sync::Fallback>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceIdentity {
    pub workspace: String,
    pub repository: String,
}

impl WorkspaceIdentity {
    pub fn of(member: &str, workspace: &crate::scheduling::placed::WorkspaceRef) -> Self {
        use sha2::{Digest, Sha256};
        let identity = |path: &str| {
            let mut hash = Sha256::new();
            hash.update(member.as_bytes());
            hash.update([0]);
            hash.update(path.as_bytes());
            format!("{:x}", hash.finalize())
        };
        Self {
            workspace: identity(&workspace.root),
            repository: identity(&workspace.common_dir),
        }
    }
}

/// A year of a thousand cores: a larger CPU time is not a measurement.
pub const MAXIMUM_CPU_MS: u64 = 1000 * 365 * 24 * 60 * 60 * 1000;

/// Where a request's size came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "from", rename_all = "snake_case")]
pub enum SizeBasis {
    /// The caller's `--size` or the consumer configuration's label default.
    Declared,
    /// Learned from the label's recent runs.
    Learned { samples: usize },
    /// Neither declared nor learned: one core.
    Default,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Queued,
    Running,
    Ended,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Outcome {
    /// The command exited with this code.
    Exited { exit_code: i32 },
    /// The command was ended by this signal.
    Signaled { signal: i32 },
    /// The lease was released explicitly.
    Released,
    /// Every process holding the grant exited. For a run, the caller exited
    /// before reporting the command's outcome, so the outcome is unknown.
    HolderExited,
    /// Elsecall failed the request with this failure code.
    Failed { code: String },
    /// The request ended before it started.
    Cancelled { reason: String },
}
impl Outcome {
    /// Whether the run time reflects the work, for run-time estimates.
    pub fn completed(&self) -> bool {
        matches!(
            self,
            Outcome::Exited { .. } | Outcome::Released | Outcome::HolderExited
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct HoldLimitRecord {
    #[serde(with = "seconds")]
    pub seconds: Duration,
    pub basis: HoldBasis,
    #[serde(with = "seconds::optional")]
    pub expected_run_seconds: Option<Duration>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HoldBasis {
    Requested,
    /// Ten minutes minus the expected run time, at least 30 seconds.
    Default,
}

/// One admission decision and every input that produced it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Decision {
    pub decided_at: Timestamp,
    pub verdict: Verdict,
    pub reason: String,
    pub candidates: Vec<Candidate>,
    /// Members placement could not ask, and why.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unreachable: Vec<crate::scheduling::placement::Unreachable>,
    /// The parameters this member decided with; absent in records from before
    /// they were tunable, which used the defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameters: Option<Parameters>,
    /// For a placeable request: the scoring the candidates come from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scoring: Option<Scoring>,
    /// For a placeable request: what its first scoring, on arrival, expected of
    /// each candidate, for judging the wait it predicted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arrival: Option<Arrival>,
}

/// The placement scoring a decision's candidates come from. With each
/// candidate's `inputs` and the decision's `parameters`, it is everything the
/// scoring used, so `replay` can repeat it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Scoring {
    pub scored_at: Timestamp,
    /// The size scored, before a member shrank a learned size to fit.
    pub size: Cores,
    #[serde(rename = "hold_left_ms", with = "crate::scheduling::timestamp::millis")]
    pub hold_left: Duration,
    /// The member the scoring chose. A request already waiting somewhere moves
    /// to it only by the moving margin, so it can differ from where it ran.
    pub winner: Option<String>,
}

/// What placement expected when a request arrived: the scoring that decided
/// where it was sent first.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Arrival {
    pub scored_at: Timestamp,
    pub winner: Option<String>,
    pub candidates: Vec<ArrivalEstimate>,
    /// The size scored; with each candidate's `state` and `inputs`, everything
    /// the scoring used, so `replay` can repeat it. Absent in records from
    /// before arrival inputs were recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<Cores>,
    #[serde(
        default,
        rename = "hold_left_ms",
        with = "crate::scheduling::timestamp::millis::optional",
        skip_serializing_if = "Option::is_none"
    )]
    pub hold_left: Option<Duration>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ArrivalEstimate {
    pub member: String,
    #[serde(with = "seconds::optional")]
    pub wait_seconds: Option<Duration>,
    #[serde(with = "seconds")]
    pub sync_seconds: Duration,
    #[serde(with = "seconds")]
    pub run_seconds: Duration,
    #[serde(with = "seconds::optional")]
    pub completion_seconds: Option<Duration>,
    pub eligible: bool,
    /// The member's admission state as the scoring saw it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<crate::scheduling::messages::MemberState>,
    /// What the scoring used beyond `state`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inputs: Option<crate::scheduling::placement::Inputs>,
}

impl Arrival {
    /// What `scoring`, the first, expected of each candidate, with what it
    /// scored them with.
    pub fn of(verdict: &crate::scheduling::placement::Verdict, scoring: &Scoring) -> Self {
        Self {
            scored_at: scoring.scored_at,
            winner: verdict.winner.clone(),
            candidates: verdict.candidates.iter().map(ArrivalEstimate::of).collect(),
            size: Some(scoring.size),
            hold_left: Some(scoring.hold_left),
        }
    }
}

impl ArrivalEstimate {
    /// What a scoring expected of one candidate, with what it scored it with.
    pub fn of(scored: &crate::scheduling::placement::Scored) -> Self {
        Self {
            member: scored.observation.state.member.clone(),
            wait_seconds: scored.estimate.wait_seconds,
            sync_seconds: scored.estimate.sync_seconds,
            run_seconds: scored.estimate.run_seconds,
            completion_seconds: scored.estimate.completion_seconds,
            eligible: scored.estimate.eligible,
            state: Some(scored.observation.state.clone()),
            inputs: Some(scored.observation.inputs()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Started,
    Nested,
    FleetBusy,
    RequestTooLarge,
    DeadlineSkipped,
    MemberUnavailable,
    /// The chosen member's synced configuration could not be brought up to date.
    ConfigSyncFailed,
}

/// A member's state when the decision was made.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Candidate {
    pub member: String,
    pub chosen: bool,
    pub why: String,
    pub capacity: Capacity,
    pub in_use: Cores,
    pub free: Cores,
    pub queued: QueueCounts,
    pub load: Option<LoadSample>,
    /// Average utilization over the saturation window.
    pub cpu_utilization_window: Option<f64>,
    pub gate: Gate,
    #[serde(with = "seconds::optional")]
    pub expected_run_seconds: Option<Duration>,
    pub expected_completion: Option<Timestamp>,
    /// The components of its expected completion, for placeable requests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimate: Option<crate::scheduling::placement::Estimate>,
    /// What placement scored it with beyond the state above, for placeable
    /// requests; absent in records from before they were recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inputs: Option<crate::scheduling::placement::Inputs>,
}

impl Candidate {
    /// A placeable request's candidate as its decision, made `now`, records it.
    pub fn scored(
        scored: &crate::scheduling::placement::Scored,
        chosen: bool,
        now: Timestamp,
    ) -> Self {
        let state = &scored.observation.state;
        Self {
            member: state.member.clone(),
            chosen,
            why: scored.why.clone(),
            capacity: state.capacity,
            in_use: state.in_use,
            free: state.free,
            queued: state.queued,
            load: state.load.clone(),
            cpu_utilization_window: scored.observation.utilization,
            gate: state.gate,
            expected_run_seconds: Some(scored.estimate.run_seconds),
            expected_completion: scored.estimate.completion_seconds.map(|c| now.after(c)),
            estimate: Some(scored.estimate.clone()),
            inputs: Some(scored.observation.inputs()),
        }
    }
}
