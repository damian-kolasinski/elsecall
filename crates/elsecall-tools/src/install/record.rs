//! The latest apply on this member: each step and how far it got, kept in
//! `tools-apply.json` in the data directory, owner-only.
//!
//! A step is queued, running, installed, skipped (its manager does not install
//! yet) or failed. A failure is the part of the step that failed and a code,
//! never what a command printed. An apply that was running when the service
//! stopped is read back with its unfinished steps failed as `apply_interrupted`:
//! running work does not survive a restart.
use crate::install::request::{HeldStep, InstallStep, WireStep};
use elsecall_contract::scheduling_model::timestamp::Timestamp;
use elsecall_fleet::provisioning_model::{ApplyState, FailedStep, ProvisioningSummary};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub(crate) const FILE: &str = "tools-apply.json";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Record {
    pub(crate) started_at: Timestamp,
    pub(crate) finished_at: Option<Timestamp>,
    pub(crate) entries: Vec<Entry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Entry {
    pub(crate) step: InstallStep,
    pub(crate) state: State,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum State {
    Queued,
    Running {
        since: Timestamp,
    },
    Installed {
        at: Timestamp,
    },
    /// Its manager does not install yet; nothing ran.
    Skipped {
        reason: &'static str,
    },
    Failed(Failure),
}

/// The part of a step that failed, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Failure {
    pub(crate) stage: String,
    pub(crate) code: String,
    pub(crate) at: Timestamp,
}

impl Record {
    /// A new apply: the held steps first, failed again with their own failure,
    /// then the steps to run, queued.
    pub(crate) fn new(steps: Vec<InstallStep>, held: Vec<HeldStep>) -> Self {
        Self {
            started_at: Timestamp::now(),
            finished_at: None,
            entries: held
                .into_iter()
                .map(|held| Entry {
                    step: held.step,
                    state: State::Failed(Failure {
                        stage: held.stage,
                        code: held.code,
                        at: held.failed_at,
                    }),
                })
                .chain(steps.into_iter().map(|step| Entry {
                    step,
                    state: State::Queued,
                }))
                .collect(),
        }
    }

    pub(crate) fn running(&self) -> bool {
        self.finished_at.is_none()
    }

    pub(crate) fn summary(&self) -> ProvisioningSummary {
        ProvisioningSummary {
            state: if self.running() {
                ApplyState::Running
            } else {
                ApplyState::Finished
            },
            started_at: self.started_at,
            finished_at: self.finished_at,
            steps: self.entries.len(),
            installed: self
                .entries
                .iter()
                .filter(|entry| matches!(entry.state, State::Installed { .. }))
                .count(),
            failed: self
                .entries
                .iter()
                .filter_map(|entry| match &entry.state {
                    State::Failed(failure) => Some(FailedStep {
                        manager: entry.step.manager.as_str().to_owned(),
                        tool: entry.step.tool.as_str().to_owned(),
                        version: entry.step.version.as_str().to_owned(),
                        stage: failure.stage.clone(),
                        code: failure.code.clone(),
                        failed_at: failure.at,
                    }),
                    _ => None,
                })
                .collect(),
        }
    }

    /// As read back after a restart: whatever had not finished was cut short.
    fn interrupted(mut self) -> Self {
        if self.running() {
            let now = Timestamp::now();
            for entry in &mut self.entries {
                if matches!(entry.state, State::Queued | State::Running { .. }) {
                    entry.state = State::Failed(Failure {
                        stage: "apply".into(),
                        code: "apply_interrupted".into(),
                        at: now,
                    });
                }
            }
            self.finished_at = Some(now);
        }
        self
    }
}

pub(crate) fn path(data: &Path) -> PathBuf {
    data.join(FILE)
}

/// The record kept in `data`; none when there is none or it cannot be read.
pub(crate) fn load(data: &Path) -> Option<Record> {
    let bytes = std::fs::read(path(data)).ok()?;
    let wire: WireRecord = serde_json::from_slice(&bytes).ok()?;
    wire.accept().map(Record::interrupted)
}

pub(crate) fn save(data: &Path, record: &Record) -> elsecall_contract::failure::Result<()> {
    let bytes = serde_json::to_vec(&WireRecord::of(record)).expect("a record serializes");
    elsecall_host::private_files::write(&path(data), &bytes, 0o600)
}

/// The record as it is kept, sent to the master and printed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct WireRecord {
    pub(crate) started_at: Timestamp,
    pub(crate) finished_at: Option<Timestamp>,
    pub(crate) steps: Vec<WireEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct WireEntry {
    pub(crate) manager: crate::tool_inventory::Manager,
    pub(crate) tool: String,
    pub(crate) version: String,
    #[serde(default)]
    pub(crate) options: std::collections::BTreeMap<String, String>,
    #[serde(flatten)]
    pub(crate) state: WireState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum WireState {
    Queued,
    Running {
        since: Timestamp,
    },
    Installed {
        at: Timestamp,
    },
    Skipped {
        reason: String,
    },
    Failed {
        stage: String,
        code: String,
        at: Timestamp,
    },
}

impl WireRecord {
    pub(crate) fn of(record: &Record) -> Self {
        Self {
            started_at: record.started_at,
            finished_at: record.finished_at,
            steps: record
                .entries
                .iter()
                .map(|entry| {
                    let step = WireStep::of(&entry.step);
                    WireEntry {
                        manager: step.manager,
                        tool: step.tool,
                        version: step.version,
                        options: step.options,
                        state: match &entry.state {
                            State::Queued => WireState::Queued,
                            State::Running { since } => WireState::Running { since: *since },
                            State::Installed { at } => WireState::Installed { at: *at },
                            State::Skipped { reason } => WireState::Skipped {
                                reason: (*reason).to_owned(),
                            },
                            State::Failed(failure) => WireState::Failed {
                                stage: failure.stage.clone(),
                                code: failure.code.clone(),
                                at: failure.at,
                            },
                        },
                    }
                })
                .collect(),
        }
    }

    /// The record, when every step follows its grammar; a kept file is ours,
    /// but a hand-edited one must not run anything it names.
    pub(crate) fn accept(self) -> Option<Record> {
        let entries = self
            .steps
            .into_iter()
            .map(|entry| {
                let step = WireStep {
                    manager: entry.manager,
                    tool: entry.tool,
                    version: entry.version,
                    options: entry.options,
                }
                .accept()
                .ok()?;
                let state = match entry.state {
                    WireState::Queued => State::Queued,
                    WireState::Running { since } => State::Running { since },
                    WireState::Installed { at } => State::Installed { at },
                    WireState::Skipped { .. } => State::Skipped {
                        reason: "not_supported",
                    },
                    WireState::Failed { stage, code, at } => {
                        State::Failed(Failure { stage, code, at })
                    }
                };
                Some(Entry { step, state })
            })
            .collect::<Option<Vec<_>>>()?;
        Some(Record {
            started_at: self.started_at,
            finished_at: self.finished_at,
            entries,
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::install::{record::*, request::ApplyRequest};
    use serde_json::json;

    fn record() -> Record {
        let steps = serde_json::from_value::<ApplyRequest>(json!({"steps": [
            {"manager": "mise", "tool": "node", "version": "22.19.0"},
            {"manager": "mise", "tool": "bun", "version": "1.3.14"},
            {"manager": "xcodes", "tool": "xcode", "version": "26.2"},
        ]}))
        .unwrap()
        .accept()
        .unwrap()
        .0;
        let mut record = Record::new(steps, Vec::new());
        record.entries[0].state = State::Installed {
            at: Timestamp::now(),
        };
        record.entries[1].state = State::Running {
            since: Timestamp::now(),
        };
        record
    }

    #[test]
    fn a_kept_record_reads_back_and_a_running_one_was_interrupted() {
        let data = tempfile::tempdir().unwrap();
        assert!(load(data.path()).is_none());
        let record = record();
        save(data.path(), &record).unwrap();
        let mode = std::os::unix::fs::PermissionsExt::mode(
            &std::fs::metadata(path(data.path())).unwrap().permissions(),
        );
        assert_eq!(mode & 0o777, 0o600);
        let read = load(data.path()).unwrap();
        assert!(!read.running());
        assert_eq!(read.entries[0].state, record.entries[0].state);
        for entry in &read.entries[1..] {
            assert!(
                matches!(&entry.state, State::Failed(failure) if failure.code == "apply_interrupted"),
                "{entry:?}"
            );
        }
        let summary = read.summary();
        assert_eq!(
            (summary.steps, summary.installed, summary.failed.len()),
            (3, 1, 2)
        );
        assert_eq!(summary.failed[0].subject(), "mise:bun@1.3.14");

        std::fs::write(
            path(data.path()),
            serde_json::to_vec(&json!({"started_at": "2026-11-20T10:00:00.000Z",
                "finished_at": null,
                "steps": [{"manager": "mise", "tool": "-x", "version": "1", "state": "queued"}]}))
            .unwrap(),
        )
        .unwrap();
        assert!(
            load(data.path()).is_none(),
            "a step outside its grammar is no record"
        );
    }
}
