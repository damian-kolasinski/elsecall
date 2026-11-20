//! The workspaces this member's runs started in: each repository root a run was
//! admitted from, and when the last one was.
//!
//! Admission records the root of every run made in a working tree here, before
//! it is placed, so local and placed runs count alike. A root stays for
//! [`KEPT_FOR`] after its last run, as run records do, and the
//! [`MAXIMUM_KEPT`] most recent are kept. The file is
//! `run-workspaces.json` in the data directory, owner-only. A root is written
//! again at most once per [`REFRESH_AFTER`], so a busy workspace does not
//! rewrite the file on every run. An unreadable file is started afresh: it only
//! remembers where runs started, and the next runs fill it again.
use elsecall_contract::scheduling_model::timestamp::Timestamp;
use elsecall_host::private_files;
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::Mutex,
    time::Duration,
};

/// How long a workspace stays after its last run: run records' retention.
pub const KEPT_FOR: Duration = elsecall_host::sqlite_retention::RETENTION;
/// How many roots are kept, the most recent first.
pub const MAXIMUM_KEPT: usize = 256;
/// A root already recorded is written again only when its last run is older.
const REFRESH_AFTER: Duration = Duration::from_secs(60 * 60);
const FILE: &str = "run-workspaces.json";
const MAXIMUM_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunWorkspace {
    pub root: PathBuf,
    pub last_run_at: Timestamp,
}

#[derive(Default, Serialize, Deserialize)]
struct Document {
    workspaces: Vec<RunWorkspace>,
}

pub struct RunWorkspaces {
    path: PathBuf,
    loaded: Mutex<Option<Vec<RunWorkspace>>>,
}

impl RunWorkspaces {
    pub fn open(data: &Path) -> Self {
        Self {
            path: data.join(FILE),
            loaded: Mutex::new(None),
        }
    }

    /// Records a run admitted from `root`, an absolute working tree root.
    pub fn record(&self, root: &Path, now: Timestamp) -> std::io::Result<()> {
        let mut loaded = self
            .loaded
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let workspaces = loaded.get_or_insert_with(|| self.read());
        if let Some(existing) = workspaces.iter_mut().find(|entry| entry.root == root) {
            if now.since(existing.last_run_at) < REFRESH_AFTER {
                return Ok(());
            }
            existing.last_run_at = now;
        } else {
            workspaces.push(RunWorkspace {
                root: root.to_owned(),
                last_run_at: now,
            });
        }
        tidy(workspaces, now);
        let bytes = serde_json::to_vec(&Document {
            workspaces: workspaces.clone(),
        })
        .expect("workspaces serialize");
        private_files::replace(&self.path, &bytes)
    }

    /// The roots with a run in the last [`KEPT_FOR`], the most recent first.
    pub fn recent(&self, now: Timestamp) -> Vec<RunWorkspace> {
        let mut loaded = self
            .loaded
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let workspaces = loaded.get_or_insert_with(|| self.read());
        let mut recent = workspaces.clone();
        tidy(&mut recent, now);
        recent
    }

    fn read(&self) -> Vec<RunWorkspace> {
        let Ok(file) = std::fs::File::open(&self.path) else {
            return Vec::new();
        };
        let mut bytes = Vec::new();
        use std::io::Read;
        if file.take(MAXIMUM_BYTES).read_to_end(&mut bytes).is_err() {
            return Vec::new();
        }
        serde_json::from_slice::<Document>(&bytes)
            .map(|document| {
                document
                    .workspaces
                    .into_iter()
                    .filter(|entry| entry.root.is_absolute())
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// Newest first, without expired entries, at most [`MAXIMUM_KEPT`].
fn tidy(workspaces: &mut Vec<RunWorkspace>, now: Timestamp) {
    workspaces.retain(|entry| now.since(entry.last_run_at) <= KEPT_FOR);
    workspaces.sort_by(|left, right| {
        right
            .last_run_at
            .cmp(&left.last_run_at)
            .then_with(|| left.root.cmp(&right.root))
    });
    workspaces.truncate(MAXIMUM_KEPT);
}

#[cfg(test)]
mod tests {
    use crate::run_workspaces::*;

    const HOUR: u64 = 60 * 60 * 1000;

    #[test]
    fn roots_are_kept_newest_first_and_expire_with_run_records() {
        let data = tempfile::tempdir().unwrap();
        let registry = RunWorkspaces::open(data.path());
        let start = Timestamp::from_millis(1_000 * HOUR);
        registry
            .record(Path::new("/Users/me/acme-ios"), start)
            .unwrap();
        registry
            .record(
                Path::new("/Users/me/elsecall"),
                start.after(Duration::from_secs(60)),
            )
            .unwrap();
        let reopened = RunWorkspaces::open(data.path());
        let roots: Vec<_> = reopened
            .recent(start.after(Duration::from_secs(120)))
            .into_iter()
            .map(|entry| entry.root)
            .collect();
        assert_eq!(
            roots,
            [
                PathBuf::from("/Users/me/elsecall"),
                PathBuf::from("/Users/me/acme-ios")
            ]
        );
        let later = start.after(KEPT_FOR + Duration::from_secs(30));
        let left: Vec<_> = reopened
            .recent(later)
            .into_iter()
            .map(|entry| entry.root)
            .collect();
        assert_eq!(left, [PathBuf::from("/Users/me/elsecall")]);
    }

    #[test]
    fn a_busy_root_is_rewritten_at_most_hourly() {
        let data = tempfile::tempdir().unwrap();
        let registry = RunWorkspaces::open(data.path());
        let start = Timestamp::from_millis(1_000 * HOUR);
        let root = Path::new("/Users/me/acme-ios");
        registry.record(root, start).unwrap();
        let file = data.path().join(FILE);
        let first = std::fs::read(&file).unwrap();
        registry
            .record(root, start.after(Duration::from_secs(600)))
            .unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), first);
        let hour_later = start.after(Duration::from_secs(3_601));
        registry.record(root, hour_later).unwrap();
        assert_eq!(registry.recent(hour_later)[0].last_run_at, hour_later);
        let mode = std::os::unix::fs::PermissionsExt::mode(
            &std::fs::metadata(&file).unwrap().permissions(),
        );
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn at_most_the_newest_roots_are_kept_and_a_broken_file_starts_afresh() {
        let data = tempfile::tempdir().unwrap();
        std::fs::write(data.path().join(FILE), b"not json").unwrap();
        let registry = RunWorkspaces::open(data.path());
        let start = Timestamp::from_millis(1_000 * HOUR);
        assert!(registry.recent(start).is_empty());
        for index in 0..(MAXIMUM_KEPT + 3) {
            let at = start.after(Duration::from_secs(index as u64));
            registry
                .record(Path::new(&format!("/Users/me/w{index}")), at)
                .unwrap();
        }
        let recent = registry.recent(start.after(Duration::from_secs(1_000)));
        assert_eq!(recent.len(), MAXIMUM_KEPT);
        assert_eq!(
            recent[0].root,
            PathBuf::from(format!("/Users/me/w{}", MAXIMUM_KEPT + 2))
        );
    }
}
