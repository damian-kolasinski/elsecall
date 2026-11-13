//! Work placed on another member: what the master offers, what the member answers,
//! and what both record. The master holds placed requests; the member it names
//! either admits one at once or refuses it.
use crate::{
    failure::{Failure, Result},
    scheduling::{
        capacity::Cores,
        leftovers::Leftovers,
        messages::MemberState,
        quote::{Quote, QuoteRequest},
        record::SizeBasis,
        run_id::RunId,
        timestamp::Timestamp,
    },
};
use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The caller's working tree, which the member copies.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceRef {
    /// The working tree's absolute root on the calling member.
    pub root: String,
    /// The repository's common git directory, which its worktrees share.
    pub common_dir: String,
}
impl WorkspaceRef {
    pub fn validate(&self) -> Result<()> {
        let absolute = |p: &str| {
            p.len() > 1
                && p.len() <= 1024
                && p.starts_with('/')
                && p[1..]
                    .split('/')
                    .all(|c| !c.is_empty() && c != "." && c != "..")
        };
        if absolute(&self.root) && absolute(&self.common_dir) {
            Ok(())
        } else {
            Err(crate::scheduling::request::invalid(
                "The workspace must be an absolute, normalized path of at most 1024 bytes.",
            ))
        }
    }
}

/// What the master asks a member to admit.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlacementOffer {
    pub run_id: RunId,
    pub size: Cores,
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workload: Option<crate::workload_metadata::Workload>,
    pub workspace: WorkspaceRef,
    /// How long the request may still take to start, sync included.
    pub start_within_ms: u64,
    /// Where the size came from; a learned size shrinks to fit the member.
    #[serde(default)]
    pub size_basis: Option<SizeBasis>,
    /// The run time the master expects, so the member can tell others when its
    /// capacity frees.
    #[serde(default)]
    pub expected_run_ms: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "answer", rename_all = "snake_case")]
pub enum OfferReply {
    /// Admitted: capacity and the copy are held until the run attaches or the
    /// attachment window passes.
    Accepted {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workload_contract: Option<u32>,
        state: MemberState,
        copy: String,
        location: String,
        #[serde(default)]
        warmth: Option<Warmth>,
    },
    /// No room now, or the member's own requests are waiting. `report` says what
    /// holds the member, as `fleet_busy` would.
    Busy { state: MemberState, report: Failure },
    /// Another run uses this workspace's copy.
    CopyBusy { state: MemberState },
    /// Larger than the member could ever admit.
    TooLarge {
        state: MemberState,
        failure: Failure,
    },
}

/// What the scheduler on this member knows about the fleet.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Standing {
    /// This member's name in the fleet, if it has one.
    pub name: Option<String>,
    /// The member this member records as master.
    pub master: Option<String>,
    pub members: Vec<String>,
}

/// The other members, as placement reaches them.
pub trait Members: Send + Sync {
    fn standing(&self) -> Standing;
    /// Offers `offer` to `member`. A failure means the member could not be asked.
    fn offer(&self, member: &str, offer: &PlacementOffer)
    -> BoxFuture<'static, Result<OfferReply>>;
    /// Asks `member` for its state and its copies of `request`'s workspaces.
    fn quote(&self, member: &str, request: &QuoteRequest) -> BoxFuture<'static, Result<Quote>>;
}

/// Neither in a fleet nor able to place: a machine on its own.
pub struct Alone;
impl Members for Alone {
    fn standing(&self) -> Standing {
        Standing::default()
    }
    fn offer(&self, member: &str, _: &PlacementOffer) -> BoxFuture<'static, Result<OfferReply>> {
        let failure = not_in_fleet(member);
        Box::pin(async move { Err(failure) })
    }
    fn quote(&self, member: &str, _: &QuoteRequest) -> BoxFuture<'static, Result<Quote>> {
        let failure = not_in_fleet(member);
        Box::pin(async move { Err(failure) })
    }
}

/// `member` cannot be offered work: this member's fleet does not list it.
pub fn not_in_fleet(member: &str) -> Failure {
    Failure::new(
        "member_unavailable",
        format!("{member} is not reachable: this member's fleet does not list it."),
        "Check the members with elsecall fleet, or drop --member.",
    )
}

/// Which side of a placed run a record describes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    /// The calling member, which placed the run.
    Caller,
    /// The member that ran it.
    Member,
}

/// How far a placed run got, as far as this side knows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reached {
    /// Admitted by the member; the workspace had not been synced.
    #[default]
    Placed,
    Synced,
    Started,
    Exited,
    /// The caller received the exit and the run ended normally.
    Finished,
}

/// A summary of one workspace sync.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SyncSummary {
    pub duration_ms: Option<u64>,
    /// Listings, content and history the caller sent.
    pub sent_bytes: Option<u64>,
    pub received_bytes: u64,
    pub pack_bytes: u64,
    /// Whether history came from one of the member's object sources.
    #[serde(default)]
    pub history_from_source: bool,
    /// The refs the copy carries besides HEAD: its upstream, remotes' default
    /// branches and declared refs, by full name.
    #[serde(default)]
    pub refs: Vec<String>,
    /// Commits sent only because a carried ref needs them, when there were any.
    /// Only the caller counts them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refs_only_commits: Option<u64>,
    pub files_written: usize,
    pub files_removed: usize,
    /// Paths an earlier run had changed in the copy.
    pub run_changes_undone: usize,
    pub content_from_copy: usize,
    pub content_from_history: usize,
    pub full_listing: bool,
    /// Files and bytes of content the copy holds, without ignored files.
    pub copy_files: usize,
    pub copy_content_bytes: u64,
    /// Milliseconds per phase, named `caller.<phase>` and `member.<phase>`.
    #[serde(default)]
    pub phases_ms: BTreeMap<String, u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlacedRecord {
    pub side: Side,
    pub calling_member: String,
    pub member: String,
    pub caller_workspace: String,
    /// The caller repository's common git directory, which its worktrees share.
    #[serde(default)]
    pub repository: Option<String>,
    pub copy: Option<String>,
    pub location: Option<String>,
    /// The copy's state when the member admitted the run.
    #[serde(default)]
    pub warmth: Option<Warmth>,
    pub reached: Reached,
    pub sync: Option<SyncSummary>,
    /// The command's exit when it is known, also when the run failed afterwards.
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub last_contact_at: Option<Timestamp>,
    /// On the member: processes the command left running after it exited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leftovers: Option<Leftovers>,
    /// On the member: how long the run waited, before its sync, for an earlier
    /// run's leftovers in the same copy to end or be stopped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leftovers_wait_ms: Option<u64>,
    /// What this member knows ended the run before it finished, beside its
    /// outcome: its own service restarting. Absent when it does not know, as
    /// when the other member's side went silent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interrupted: Option<Interruption>,
}

/// Why a member knows a placed run did not finish.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Interruption {
    /// This member's service restarted during the run, which ended its
    /// connection: a cause on this side, not the other member's.
    ServiceRestart,
}

impl PlacedRecord {
    /// Whether the run had to set its copy up first.
    pub fn cold(&self) -> bool {
        matches!(self.warmth, Some(Warmth::Cold | Warmth::ColdWithHistory))
    }
}

/// The state of a member's copy of a workspace.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Warmth {
    /// A copy that an earlier run synced, with its build state.
    Warm,
    /// No copy, but the repository's history is there from another worktree.
    ColdWithHistory,
    /// Neither a copy nor the repository's history.
    Cold,
}

/// What a caller tells its own member about the placed run it drove.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlacedReport {
    pub reached: Reached,
    pub sync: Option<SyncSummary>,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub warnings: Vec<String>,
    /// CPU time the command used on the member, as the member measured it.
    #[serde(default)]
    pub cpu_ms: Option<u64>,
}

/// What the caller needs to drive a placed run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlacedGrant {
    /// The latest the command may start; the hold limit applies end to end.
    pub start_by: Timestamp,
    pub copy: String,
    /// The calling member's own name, for messages about the run.
    pub calling_member: String,
    /// Synced paths the member was brought up to date with before the run; a
    /// consistency check of a location within one is current by construction.
    #[serde(default)]
    pub synced_paths: Vec<String>,
}
