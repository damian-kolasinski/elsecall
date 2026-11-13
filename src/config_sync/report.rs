//! What sync shows: plans, each member's sync state, and what a push changed. Only
//! names, classes, sizes and short keyed fingerprints; never content.
use crate::{
    config_sync::{
        compare::{Change, FileDifference},
        history::{FileRecord, PathRecord},
        manifest::FileKey,
        state::Backup,
        synced_path::SyncedPath,
    },
    failure::Failure,
    roster::MemberName,
    scheduling::timestamp::Timestamp,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// One file that differs, as a plan or status shows it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FileReport {
    pub file: String,
    pub change: Change,
    /// The member's current version is backed up before it is replaced.
    pub backup: bool,
    /// The size and fingerprint the member will hold.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    /// The fingerprint of what the member holds now.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub member_fingerprint: Option<String>,
}

pub fn file_name(path: &SyncedPath, key: &FileKey) -> String {
    path.display_file(key.as_ref().map(|k| k.as_str()))
}

impl FileReport {
    pub fn of(path: &SyncedPath, difference: &FileDifference) -> Self {
        // A member's override values are its own: other members, which hold the
        // fleet key, see their names only.
        let shown = difference.change != Change::Overridden;
        Self {
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

/// This member's sync state, as `sync status` and the fleet view show it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SyncSummary {
    /// `source` on the master, `replica` elsewhere.
    pub role: SyncRole,
    #[serde(default)]
    pub service: crate::service_configuration::Summary,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation: Option<u64>,
    /// Equal on every member holding the same synced content.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manifest_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub issued_by: Option<MemberName>,
    /// A generation's application began and has not finished.
    #[serde(default)]
    pub applying: bool,
    pub paths: Vec<PathState>,
    pub backups: Vec<BackupView>,
    #[serde(default)]
    pub pruned_backups: u64,
    /// On the master: synced paths its own `[sync] paths` does not declare. They
    /// keep their last issued state until declared there again, or disabled.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paused: Vec<SyncedPath>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<Failure>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncRole {
    Source,
    Replica,
}

/// One synced path on this member.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PathState {
    pub path: SyncedPath,
    /// Files and bytes in the synced (mirrored) tree.
    pub files: usize,
    pub bytes: u64,
    /// Where this member's overrides for the path go.
    pub overrides: PathBuf,
    /// Files this member's overrides decide.
    pub overridden: Vec<String>,
    /// How the files here differ from what sync would put here: local edits not
    /// yet backed up, and overrides not yet applied.
    pub differences: Vec<FileReport>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<Failure>,
}

impl SyncSummary {
    pub fn locally_edited(&self) -> usize {
        self.paths
            .iter()
            .flat_map(|p| &p.differences)
            .filter(|f| f.backup)
            .count()
    }
    /// Differences other than local edits: what the next sync on this member
    /// still has to apply.
    pub fn pending(&self) -> usize {
        self.paths
            .iter()
            .flat_map(|p| &p.differences)
            // Overridden files are listed only while their override is not laid.
            .filter(|f| !f.backup)
            .count()
    }
}

/// A member's sync state against its master's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncState {
    /// Nothing is synced.
    Off,
    /// Holds the master's synced content, applied as it should be.
    InSync,
    /// Behind the master, or with a change still to apply here.
    Pending,
    /// In sync, but files here were edited locally; the next sync backs them up
    /// and replaces them.
    LocallyEdited,
    /// This member is the source.
    Source,
}

impl SyncSummary {
    pub fn service_against(&mut self, master: Option<&SyncSummary>) {
        self.service.current = master
            .and_then(|m| m.service.fingerprint.as_ref())
            .map(|wanted| {
                self.service.fingerprint.as_ref() == Some(wanted)
                    && !self.service.pending
                    && self.service.rejection.is_none()
            });
    }

    /// This member's state, given the master's summary.
    pub fn state_against(&self, master: Option<&SyncSummary>) -> SyncState {
        if self.role == SyncRole::Source {
            return SyncState::Source;
        }
        let wanted = master.and_then(|m| m.manifest_id.as_ref());
        if wanted.is_none() && self.manifest_id.is_none() {
            return SyncState::Off;
        }
        if self.applying
            || self.service.pending
            || self.service.rejection.is_some()
            || self.manifest_id.as_ref() != wanted
            || self.pending() > 0
        {
            return SyncState::Pending;
        }
        if self.locally_edited() > 0 {
            return SyncState::LocallyEdited;
        }
        SyncState::InSync
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BackupView {
    pub at: Timestamp,
    pub file: String,
    pub change: Change,
    pub size: u64,
    /// Keyed with this member's own key: comparable between its backups only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    pub location: PathBuf,
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
