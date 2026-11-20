//! What sync shows: plans, each member's sync state, and what a push changed. Only
//! names, classes, sizes and short keyed fingerprints; never content.
use crate::config_sync::{
    compare::{Change, FileDifference},
    history::{FileRecord, PathRecord},
    manifest::FileKey,
    state::Backup,
};
use elsecall_contract::failure::Failure;
use elsecall_fleet::config_sync_model::synced_path::SyncedPath;
use serde::{Deserialize, Serialize};

pub use elsecall_fleet::config_sync_model::status::{
    BackupView, FileReport, PathState, SyncRole, SyncState, SyncSummary,
};

pub fn file_name(path: &SyncedPath, key: &FileKey) -> String {
    path.display_file(key.as_ref().map(|k| k.as_str()))
}

/// How `difference` shows in a plan or status.
pub fn file_report(path: &SyncedPath, difference: &FileDifference) -> FileReport {
    {
        // A member's override values are its own: other members, which hold the
        // fleet key, see their names only.
        let shown = difference.change != Change::Overridden;
        FileReport {
            file: file_name(path, &difference.key),
            change: difference.change,
            backup: difference.backup,
            size: difference.desired.filter(|_| shown).map(|f| f.size),
            fingerprint: difference
                .desired
                .filter(|_| shown)
                .map(|f| f.fingerprint.short()),
            member_fingerprint: difference
                .live
                .filter(|_| shown)
                .map(|f| f.fingerprint.short()),
        }
    }
}

/// How one synced path differs on one member.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PathReport {
    pub path: SyncedPath,
    pub files: Vec<FileReport>,
    /// Files already as they will be.
    pub unchanged: usize,
    /// Entries Elsecall does not carry (links, special files), left as they are
    /// on the master and removed on members.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<Failure>,
}

impl From<&Backup> for BackupView {
    fn from(backup: &Backup) -> Self {
        Self {
            at: backup.at,
            file: backup.file.clone(),
            change: backup.change,
            size: backup.size,
            fingerprint: backup.fingerprint.map(|f| f.short()),
            location: backup.location.clone(),
        }
    }
}

/// What applying a generation changed on one member.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Applied {
    pub generation: u64,
    pub manifest_id: String,
    pub paths: Vec<PathRecord>,
}

impl Applied {
    pub fn backups(&self) -> usize {
        self.paths
            .iter()
            .flat_map(|p| &p.files)
            .filter(|f| f.backed_up)
            .count()
    }
}

pub fn record_of(path: &SyncedPath, differences: &[FileDifference]) -> PathRecord {
    PathRecord {
        path: path.clone(),
        files: differences
            .iter()
            .map(|d| FileRecord {
                file: file_name(path, &d.key),
                change: d.change,
                size: d.desired.map(|f| f.size),
                fingerprint: d.desired.map(|f| f.fingerprint.short()),
                backed_up: d.backup,
            })
            .collect(),
    }
}
