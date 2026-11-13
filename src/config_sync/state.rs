//! This member's sync state, kept owner-only in Elsecall's state directory: the
//! fingerprint key, the manifest it holds (issued, if it is master; applied
//! otherwise), a manifest being applied, what it last wrote at each synced path,
//! and the backups of local edits. It holds fingerprints and names, never content.
use crate::{
    config_sync::{
        compare::Change,
        fingerprint::{Fingerprint, SyncKey},
        manifest::{Manifest, Tree},
        synced_path::SyncedPath,
    },
    failure::{Failure, Result},
    private_files,
    scheduling::timestamp::Timestamp,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    io::Read,
    path::{Path, PathBuf},
};

const LIMIT: u64 = 16 << 20;
/// Backups kept of each file; older ones are removed and counted.
pub const BACKUPS_PER_FILE: usize = 20;
/// Global oldest-first cap, including filenames no longer in the synced set.
pub const BACKUPS_TOTAL: usize = 1000;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct State {
    #[serde(default)]
    pub service_rejection: Option<Failure>,
    #[serde(default)]
    pub service_member: Option<crate::roster::MemberName>,
    #[serde(default)]
    pub service_source_master: bool,
    pub key: Option<SyncKey>,
    pub manifest: Option<Manifest>,
    /// A manifest whose application had begun; finished at the next chance.
    pub applying: Option<Manifest>,
    /// The tree sync last wrote at each path, overrides included.
    pub written: BTreeMap<SyncedPath, Tree>,
    pub backups: Vec<Backup>,
    /// Backups removed because a file had more than `BACKUPS_PER_FILE`.
    #[serde(default)]
    pub pruned_backups: u64,
    /// This member issued the manifest it holds, as master. Its files may then
    /// hold changes no other member received, so the first push it applies from
    /// another master backs up everything it replaces.
    #[serde(default)]
    pub issued_here: bool,
    /// The last role this member reconciled its files as. A member that becomes
    /// master first restores its replica, so its own drift never becomes truth.
    #[serde(default)]
    pub acting_as_source: bool,
    /// A key only this member holds, for fingerprints of its own content (backups),
    /// which other members, holding the fleet key, must not be able to test.
    #[serde(default)]
    pub local_key: Option<SyncKey>,
}

/// A local version of a file kept before sync replaced or removed it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Backup {
    pub at: Timestamp,
    pub path: SyncedPath,
    /// The file, as `~/…`.
    pub file: String,
    pub change: Change,
    pub size: u64,
    /// Under this member's local key; none for a whole tree kept because it held
    /// entries sync cannot carry.
    pub fingerprint: Option<Fingerprint>,
    /// Where the backup is, owner-only, in Elsecall's state.
    pub location: PathBuf,
}

fn invalid(path: &Path) -> Failure {
    Failure::new(
        "invalid_sync_state",
        "This member's sync state could not be read.",
        format!(
            "Move {} aside and run elsecall service restart; the master pushes the synced set again.",
            path.display()
        ),
    )
}

impl State {
    pub fn load(path: &Path) -> Result<Self> {
        let file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(Failure::io("Read sync state", e)),
        };
        let mut text = Vec::new();
        file.take(LIMIT + 1)
            .read_to_end(&mut text)
            .map_err(|e| Failure::io("Read sync state", e))?;
        if text.len() as u64 > LIMIT {
            return Err(invalid(path));
        }
        serde_json::from_slice(&text).map_err(|_| invalid(path))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let bytes = serde_json::to_vec(self).expect("sync state serializes");
        private_files::replace(path, &bytes).map_err(|e| Failure::io("Save sync state", e))
    }

    /// Records a backup, keeping at most `BACKUPS_PER_FILE` of each file. Returns
    /// the backups to delete.
    pub fn add_backup(&mut self, backup: Backup) -> Vec<Backup> {
        self.add_backup_with_limit(backup, BACKUPS_TOTAL)
    }

    pub fn add_backup_with_limit(&mut self, backup: Backup, limit: usize) -> Vec<Backup> {
        self.backups.push(backup);
        self.trim_backups(limit)
    }

    /// Oldest first, globally and per filename. Also applies after lowering a cap.
    pub fn trim_backups(&mut self, limit: usize) -> Vec<Backup> {
        self.backups.sort_by_key(|b| b.at);
        let mut counts = BTreeMap::new();
        let mut kept = Vec::new();
        let mut removed = Vec::new();
        for backup in std::mem::take(&mut self.backups).into_iter().rev() {
            let count = counts.entry(backup.file.clone()).or_insert(0);
            if kept.len() >= limit || *count >= BACKUPS_PER_FILE {
                removed.push(backup);
            } else {
                *count += 1;
                kept.push(backup);
            }
        }
        kept.reverse();
        self.backups = kept;
        self.pruned_backups += removed.len() as u64;
        removed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn global_cap_expires_disappeared_filenames_oldest_first() {
        let mut state = State::default();
        for n in 0..10 {
            let removed = state.add_backup_with_limit(
                Backup {
                    at: Timestamp::from_millis(n),
                    path: SyncedPath::parse("~/.config/demo").unwrap(),
                    file: format!("~/.config/demo/{n}"),
                    change: Change::Removed,
                    size: 1,
                    fingerprint: None,
                    location: PathBuf::from(format!("/backups/{n}")),
                },
                3,
            );
            assert_eq!(removed.len(), usize::from(n >= 3));
        }
        assert_eq!(
            state
                .backups
                .iter()
                .map(|b| b.at.millis())
                .collect::<Vec<_>>(),
            [7, 8, 9]
        );
        assert_eq!(state.trim_backups(1).len(), 2);
        assert_eq!(state.backups[0].at.millis(), 9);
    }

    #[test]
    fn round_trips_and_bounds_backups() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut state = State::load(&path).unwrap();
        assert!(state.manifest.is_none());
        state.key = SyncKey::generate();
        let key = state.key.clone().unwrap();
        let synced = SyncedPath::parse("~/.config/demo").unwrap();
        for n in 0..(BACKUPS_PER_FILE + 2) {
            let removed = state.add_backup(Backup {
                at: Timestamp::from_millis(n as u64),
                path: synced.clone(),
                file: "~/.config/demo/a".into(),
                change: Change::LocallyEdited,
                size: 1,
                fingerprint: Some(key.fingerprint(&[n as u8])),
                location: dir.path().join(n.to_string()),
            });
            assert_eq!(removed.len(), usize::from(n >= BACKUPS_PER_FILE));
        }
        assert_eq!(state.backups.len(), BACKUPS_PER_FILE);
        assert_eq!(state.pruned_backups, 2);
        assert_eq!(state.backups[0].at, Timestamp::from_millis(2));
        state.save(&path).unwrap();
        let back = State::load(&path).unwrap();
        assert_eq!(back.key, state.key);
        assert_eq!(back.backups, state.backups);
        std::fs::write(&path, "{").unwrap();
        assert_eq!(State::load(&path).unwrap_err().code, "invalid_sync_state");
    }
}
