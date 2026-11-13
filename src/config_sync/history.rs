//! Records of what synced: each issued generation, each push and each application,
//! with paths, file names, sizes, short fingerprints and the outcome. Never
//! content. Kept owner-only as JSON lines, the latest `KEPT` of them.
use crate::{
    config_sync::{compare::Change, synced_path::SyncedPath},
    failure::{Failure, Result},
    private_files,
    roster::MemberName,
    scheduling::timestamp::Timestamp,
};
use serde::{Deserialize, Serialize};
use std::{
    io::{BufRead, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::Mutex,
};

const KEPT: usize = 500;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Event {
    /// The master issued a generation from its files.
    Issued,
    /// The master pushed a generation to a member.
    Pushed,
    /// A push to a member failed; the member keeps its previous state.
    PushFailed,
    /// This member applied a generation, or its overrides changed.
    Applied,
    /// Applying failed; the previous state stays.
    ApplyFailed,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FileRecord {
    pub file: String,
    pub change: Change,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// The short fingerprint of the content now there, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub backed_up: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PathRecord {
    pub path: SyncedPath,
    pub files: Vec<FileRecord>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Record {
    pub at: Timestamp,
    pub event: Event,
    pub generation: u64,
    pub manifest_id: String,
    /// The member pushed to, for push events.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub member: Option<MemberName>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<PathRecord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub struct History {
    path: PathBuf,
    lock: Mutex<()>,
}

impl History {
    pub fn new(path: &Path) -> Self {
        Self {
            path: path.to_owned(),
            lock: Mutex::new(()),
        }
    }

    pub fn append(&self, record: &Record) -> Result<()> {
        let _guard = self.lock.lock().expect("history lock");
        let mut line = serde_json::to_vec(record).expect("records serialize");
        line.push(b'\n');
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&self.path)
            .map_err(|e| Failure::io("Record a sync", e))?;
        file.write_all(&line)
            .map_err(|e| Failure::io("Record a sync", e))?;
        drop(file);
        let lines = self.lines()?;
        if lines.len() > 2 * KEPT {
            let kept = lines[lines.len() - KEPT..].join("\n") + "\n";
            private_files::replace(&self.path, kept.as_bytes())
                .map_err(|e| Failure::io("Trim sync records", e))?;
        }
        Ok(())
    }

    fn lines(&self) -> Result<Vec<String>> {
        match std::fs::File::open(&self.path) {
            Ok(file) => Ok(std::io::BufReader::new(file)
                .lines()
                .map_while(|l| l.ok())
                .collect()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(Failure::io("Read sync records", e)),
        }
    }

    /// The latest `limit` records, newest first.
    pub fn latest(&self, limit: usize) -> Result<Vec<Record>> {
        let _guard = self.lock.lock().expect("history lock");
        Ok(self
            .lines()?
            .iter()
            .rev()
            .filter_map(|line| serde_json::from_str(line).ok())
            .take(limit)
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_the_latest_records() {
        let dir = tempfile::tempdir().unwrap();
        let history = History::new(&dir.path().join("history.jsonl"));
        for generation in 0..(2 * KEPT as u64 + 1) {
            history
                .append(&Record {
                    at: Timestamp::from_millis(generation),
                    event: Event::Applied,
                    generation,
                    manifest_id: "0123456789abcdef".into(),
                    member: None,
                    paths: Vec::new(),
                    error: None,
                })
                .unwrap();
        }
        let latest = history.latest(3).unwrap();
        assert_eq!(latest[0].generation, 2 * KEPT as u64);
        assert_eq!(latest.len(), 3);
        assert_eq!(history.latest(usize::MAX).unwrap().len(), KEPT);
    }
}
