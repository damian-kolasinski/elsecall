//! A member's record of its latest upgrade, in `upgrade/` in Elsecall's owner-only
//! state: `state.json`, the staged build (`incoming`), the executable it replaces
//! (`previous`) and the helper's lock. The service, the helper that installs the
//! build and the service that starts afterwards all read and write it, so the
//! master learns the outcome across the restart. Its format is frozen with the
//! upgrade protocol.
use crate::{
    build_identity::{Build, BuildId, Digest},
    failure::{Failure, Result},
    fleet_upgrade::{member::WorkInFlight, protocol::UpgradeId},
    private_files,
    roster::MemberName,
    scheduling::timestamp::Timestamp,
};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Phase {
    /// The build is staged; the member waits for its runs and leases to end.
    Draining {
        wait_seconds: u64,
        in_flight: Vec<WorkInFlight>,
    },
    /// The helper is installing the build and restarting the service.
    Installing,
    /// The new build runs and is ready.
    Upgraded { running: Build },
    /// The new build did not become ready; the previous one runs again.
    RolledBack { running: Build, error: Failure },
    /// Neither the new nor the previous build became ready.
    RollbackFailed { error: Failure },
    /// Work was still running when the wait ended; nothing changed.
    Deferred { in_flight: Vec<WorkInFlight> },
    /// The attempt ended before installing anything, or its helper vanished.
    Interrupted { error: Failure },
}
impl Phase {
    pub fn terminal(&self) -> bool {
        !matches!(self, Phase::Draining { .. } | Phase::Installing)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UpgradeState {
    pub id: UpgradeId,
    pub master: MemberName,
    /// What the member ran when the upgrade arrived.
    pub from: Build,
    /// The build being installed.
    pub to: BuildId,
    /// Its full digest, which the helper checks the staged build against.
    pub digest: Digest,
    #[serde(flatten)]
    pub phase: Phase,
    pub updated_at: Timestamp,
}

pub struct Journal {
    directory: PathBuf,
}
impl Journal {
    pub fn new(data: &Path) -> Self {
        Self {
            directory: data.join("upgrade"),
        }
    }
    pub fn directory(&self) -> &Path {
        &self.directory
    }
    fn state_path(&self) -> PathBuf {
        self.directory.join("state.json")
    }
    pub fn incoming(&self) -> PathBuf {
        self.directory.join("incoming")
    }
    pub fn previous(&self) -> PathBuf {
        self.directory.join("previous")
    }

    /// The latest upgrade, if this member ever had one.
    pub fn read(&self) -> Result<Option<UpgradeState>> {
        match std::fs::read(self.state_path()) {
            Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|_| {
                Failure::new(
                    "invalid_upgrade_state",
                    "This member's upgrade record is not readable.",
                    "Move upgrade/state.json out of the Elsecall state directory; the next upgrade starts a new one.",
                )
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(Failure::io("Read upgrade record", e)),
        }
    }

    /// Records the state atomically and durably, so a power loss leaves either the
    /// previous record or this one.
    pub fn record(&self, state: &UpgradeState) -> Result<()> {
        private_files::directory(&self.directory)?;
        let bytes = serde_json::to_vec_pretty(state).expect("upgrade state serializes");
        private_files::write(&self.state_path(), &bytes, 0o600)
    }

    /// Moves an unreadable record aside, so it cannot disable upgrades; it is kept
    /// as `state.json.unreadable` for diagnosis.
    pub fn set_aside_unreadable(&self) {
        if matches!(self.read(), Err(ref e) if e.code == "invalid_upgrade_state") {
            let aside = self.directory.join("state.json.unreadable");
            if std::fs::rename(self.state_path(), aside).is_ok() {
                crate::service_log!("{{\"event\":\"upgrade_record_set_aside\"}}");
            }
        }
    }

    /// Moves the latest upgrade to `phase`.
    pub fn advance(&self, state: &mut UpgradeState, phase: Phase) -> Result<()> {
        state.phase = phase;
        state.updated_at = Timestamp::now();
        self.record(state)
    }

    /// Writes an executable owner-only, replacing any earlier one atomically.
    pub fn stage(&self, path: &Path, bytes: &[u8]) -> Result<()> {
        private_files::directory(&self.directory)?;
        private_files::write(path, bytes, 0o700)
    }

    /// Removes the staged and previous builds, once nothing needs them.
    pub fn discard_builds(&self) {
        let _ = private_files::remove_file(&self.incoming());
        let _ = private_files::remove_file(&self.previous());
    }

    /// The lock the helper holds while it installs; `None` while a helper runs.
    pub fn helper_lock(&self) -> Result<Option<File>> {
        private_files::directory(&self.directory)?;
        private_files::try_lock(&self.directory.join("helper.lock"))
    }

    /// Removes the upgrade record and its files, as uninstalling does. Unknown files
    /// are left, and so is the directory then.
    pub fn remove(&self) -> Result<()> {
        for file in [
            self.state_path(),
            self.directory.join("state.json.unreadable"),
            self.incoming(),
            self.previous(),
            self.directory.join("helper.lock"),
        ] {
            private_files::remove_file(&file)?;
        }
        match std::fs::remove_dir(&self.directory) {
            Err(e)
                if e.kind() != std::io::ErrorKind::NotFound
                    && e.kind() != std::io::ErrorKind::DirectoryNotEmpty =>
            {
                Err(Failure::io("Remove upgrade state", e))
            }
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn states_round_trip_and_only_waiting_and_installing_are_open() {
        let home = tempfile::tempdir().unwrap();
        let data = home.path().join("data");
        private_files::directory(&data).unwrap();
        let journal = Journal::new(&data);
        assert!(journal.read().unwrap().is_none());
        let build: BuildId = "0123456789abcdef".parse().unwrap();
        let mut state = UpgradeState {
            id: UpgradeId::generate().unwrap(),
            master: "alpha".parse().unwrap(),
            from: Build {
                version: "0.1.0".into(),
                build: build.clone(),
            },
            to: Digest::of(b"next").build_id(),
            digest: Digest::of(b"next"),
            phase: Phase::Installing,
            updated_at: Timestamp::now(),
        };
        journal.record(&state).unwrap();
        let read = journal.read().unwrap().unwrap();
        assert_eq!(read.id, state.id);
        assert_eq!(read.from, state.from);
        assert!(matches!(read.phase, Phase::Installing));
        assert!(!state.phase.terminal());
        let error = Failure::new("startup_failed", "m", "h");
        let running = state.from.clone();
        journal
            .advance(&mut state, Phase::RolledBack { running, error })
            .unwrap();
        let read = journal.read().unwrap().unwrap();
        assert!(read.phase.terminal());
        let json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(journal.state_path()).unwrap()).unwrap();
        assert_eq!(json["state"], "rolled_back");
        assert_eq!(json["error"]["code"], "startup_failed");
        journal.remove().unwrap();
        assert!(!journal.directory().exists());
    }

    #[test]
    fn an_unreadable_record_is_set_aside_instead_of_disabling_upgrades() {
        let home = tempfile::tempdir().unwrap();
        let data = home.path().join("data");
        private_files::directory(&data).unwrap();
        let journal = Journal::new(&data);
        private_files::directory(journal.directory()).unwrap();
        std::fs::write(journal.state_path(), b"{\"truncated").unwrap();
        assert_eq!(journal.read().unwrap_err().code, "invalid_upgrade_state");
        journal.set_aside_unreadable();
        assert!(journal.read().unwrap().is_none());
        assert!(journal.directory().join("state.json.unreadable").exists());
    }
}
