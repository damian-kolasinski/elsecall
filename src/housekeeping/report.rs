//! What `elsecall copies` and `elsecall copies prune` show.
use crate::{
    failure::Failure,
    housekeeping::{
        caller_state::Tidied,
        catalog::{Reason, Removal},
        policy::Spared,
        settings::Settings,
    },
    scheduling::timestamp::Timestamp,
    workspace::location::Setting,
};
use serde::Serialize;
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CopyState {
    /// A run uses it, or one was admitted and is about to attach.
    InUse,
    Idle,
}

/// One copy on this member.
#[derive(Clone, Debug, Serialize)]
pub struct CopyView {
    pub calling_member: String,
    pub caller_path: String,
    pub location: PathBuf,
    pub setting: Setting,
    pub state: CopyState,
    pub last_used_at: Timestamp,
    /// When the copy becomes idle long enough to be removed, unless used again.
    pub idle_expires_at: Timestamp,
    /// Allocated on disk, ignored build state included; null until measured.
    pub size_bytes: Option<u64>,
    pub measured_at: Option<Timestamp>,
    pub store: PathBuf,
}

/// One shared object store.
#[derive(Clone, Debug, Serialize)]
pub struct StoreView {
    pub calling_member: String,
    pub path: PathBuf,
    pub size_bytes: Option<u64>,
    pub measured_at: Option<Timestamp>,
    /// Copies on this member that use it.
    pub copies: usize,
}

/// A volume holding copies.
#[derive(Clone, Debug, Serialize)]
pub struct DiskView {
    pub path: PathBuf,
    pub total_bytes: u64,
    pub available_bytes: u64,
    /// Free space below this is disk pressure.
    pub threshold_bytes: u64,
    pub pressure: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct Listing {
    pub member: String,
    pub workspace_root: PathBuf,
    pub copies: Vec<CopyView>,
    pub stores: Vec<StoreView>,
    /// Copies and stores together, as last measured.
    pub total_bytes: u64,
    pub disk: Option<DiskView>,
    pub disk_error: Option<Failure>,
    pub housekeeping: HousekeepingView,
    /// The latest removals, newest first.
    pub removals: Vec<Removal>,
}

#[derive(Clone, Debug, Serialize)]
pub struct HousekeepingView {
    pub settings: Settings,
    pub last_pass: Option<PassSummary>,
}

/// This member's copies in brief.
#[derive(Clone, Debug, Serialize)]
pub struct Summary {
    pub copies: usize,
    /// As last measured; copies not measured yet count nothing.
    pub copies_bytes: u64,
    pub unmeasured_copies: usize,
    pub stores: usize,
    pub stores_bytes: u64,
    pub disk: Option<DiskView>,
    pub last_pass_at: Option<Timestamp>,
}

/// A pass in brief, for listings.
#[derive(Clone, Debug, Serialize)]
pub struct PassSummary {
    pub started_at: Timestamp,
    pub duration_ms: u64,
    pub removed: usize,
    pub freed_bytes: u64,
    pub problems: usize,
    pub disk_pressure_unrelieved: bool,
}

/// A copy a pass removed, or would remove.
#[derive(Clone, Debug, Serialize)]
pub struct RemovedCopy {
    pub calling_member: String,
    pub caller_path: String,
    pub location: PathBuf,
    pub reason: Reason,
    pub size_bytes: Option<u64>,
    pub last_used_at: Timestamp,
}

/// A copy a pass would remove but must not now.
#[derive(Clone, Debug, Serialize)]
pub struct SparedCopy {
    pub location: PathBuf,
    pub reason: Reason,
    pub spared: Spared,
}

#[derive(Clone, Debug, Serialize)]
pub struct StoreChange {
    pub path: PathBuf,
    pub size_bytes: Option<u64>,
    /// For compaction: the packs before, and the size after.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub packs_before: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_after_bytes: Option<u64>,
}

/// A store kept although no known copy uses it.
#[derive(Clone, Debug, Serialize)]
pub struct KeptStore {
    pub path: PathBuf,
    /// Copies it holds references for that this member does not track, such as a
    /// same-path copy from before housekeeping; it stays while they may exist.
    pub unknown_copies: usize,
}

/// How a volume under pressure fared.
#[derive(Clone, Debug, Serialize)]
pub struct Pressure {
    pub path: PathBuf,
    pub total_bytes: u64,
    pub threshold_bytes: u64,
    pub available_before_bytes: u64,
    pub available_after_bytes: u64,
    /// What evicting every idle copy on the volume, and the stores only they
    /// use, would free.
    pub evictable_bytes: u64,
    pub evicted: usize,
    pub outcome: Relief,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Relief {
    /// Free space is back above the threshold.
    Recovered,
    /// What was evicted should have been enough, but the volume does not show it
    /// yet, as when local snapshots hold the deleted data. The next pass checks
    /// again.
    Pending,
    /// Evicting every idle copy could not bring free space back above the
    /// threshold, so none was evicted: the disk is used by something else, or by
    /// copies in use.
    Unrecoverable,
}

#[derive(Clone, Debug, Serialize)]
pub struct Report {
    pub dry_run: bool,
    pub started_at: Timestamp,
    pub duration_ms: u64,
    pub removed: Vec<RemovedCopy>,
    pub spared: Vec<SparedCopy>,
    pub stores_removed: Vec<StoreChange>,
    pub stores_compacted: Vec<StoreChange>,
    pub stores_kept: Vec<KeptStore>,
    pub disk: Vec<Pressure>,
    /// Copies found on disk that the member did not track yet.
    pub adopted: usize,
    /// The calling members asked whether their copies' workspaces still exist.
    pub owners: Vec<OwnerAsked>,
    /// This member's own caller-side sync state that was removed.
    pub caller_state: Tidied,
    pub problems: Vec<Failure>,
}

/// What one calling member was asked and said.
#[derive(Clone, Debug, Serialize)]
pub struct OwnerAsked {
    pub member: String,
    /// Workspaces asked about.
    pub asked: usize,
    /// Of those, deleted on the calling member.
    pub gone: usize,
    /// Why it could not be asked; its copies are then judged by idleness alone.
    pub error: Option<Failure>,
}

impl Report {
    pub fn new(dry_run: bool, started_at: Timestamp) -> Self {
        Self {
            dry_run,
            started_at,
            duration_ms: 0,
            removed: Vec::new(),
            spared: Vec::new(),
            stores_removed: Vec::new(),
            stores_compacted: Vec::new(),
            stores_kept: Vec::new(),
            disk: Vec::new(),
            adopted: 0,
            owners: Vec::new(),
            caller_state: Tidied::default(),
            problems: Vec::new(),
        }
    }

    pub fn summary(&self) -> PassSummary {
        PassSummary {
            started_at: self.started_at,
            duration_ms: self.duration_ms,
            removed: self.removed.len() + self.stores_removed.len(),
            freed_bytes: self
                .removed
                .iter()
                .filter_map(|r| r.size_bytes)
                .chain(self.stores_removed.iter().filter_map(|s| s.size_bytes))
                .sum(),
            problems: self.problems.len(),
            disk_pressure_unrelieved: self.disk.iter().any(|p| p.outcome == Relief::Unrecoverable),
        }
    }
}
