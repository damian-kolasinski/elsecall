//! Configuration sync before a placed run: the master makes the member it places
//! a run on hold the current generation of every synced path, so a run never
//! starts on stale configuration. When that fails, a placeable run is placed
//! again without that member (the caller needs no sync, so it always remains);
//! a run on a required member, or one no member is left for, fails with
//! `config_sync_failed`.
//!
//! This is placement's port; the adapter over configuration sync lives outside
//! scheduling. Records keep paths, counts and sizes only, never values.
use crate::{
    failure::{Failure, Result},
    scheduling::timestamp::Timestamp,
};
use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};

/// The failure code of a run whose member could not be brought up to date.
pub const CODE: &str = "config_sync_failed";

/// Brings a member's synced configuration up to date with the master's files.
pub trait PreRunSync: Send + Sync {
    /// Makes `member` hold the master's current synced configuration: one cheap
    /// check when it already does, a push of what differs otherwise.
    fn bring_up_to_date(&self, member: &str) -> BoxFuture<'static, Result<Synced>>;

    /// Request context for adapters whose evidence is scoped to an individual
    /// scheduling attempt. Existing providers share configuration per member.
    fn bring_up_to_date_for(
        &self,
        member: &str,
        _run_id: &crate::scheduling::run_id::RunId,
    ) -> BoxFuture<'static, Result<Synced>> {
        self.bring_up_to_date(member)
    }
}

/// Where the member stands once the sync succeeded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Synced {
    /// The fleet syncs nothing.
    NothingSynced,
    /// The member already held the current generation.
    Current {
        generation: u64,
        /// The synced paths the member now holds as the master issued them.
        paths: Vec<String>,
    },
    /// The master pushed the current generation first.
    Pushed {
        generation: u64,
        paths: Vec<String>,
        pushed: Vec<PushedPath>,
    },
}

impl Synced {
    /// Paths the member holds as the master's current files, so consistency
    /// checks of them are current by construction.
    pub fn current_paths(&self) -> &[String] {
        match self {
            Synced::NothingSynced => &[],
            Synced::Current { paths, .. } | Synced::Pushed { paths, .. } => paths,
        }
    }
}

/// What a push changed below one synced path: counts and sizes, never values.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PushedPath {
    pub path: String,
    /// Files added, changed, removed or with changed permissions.
    pub files: usize,
    /// Bytes of the files the member received.
    pub bytes: u64,
}

/// The pre-run sync as a run's record keeps it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreRunSyncRecord {
    pub member: String,
    /// From asking the member to its answer, push included; part of the run's
    /// wait for its start.
    pub duration_ms: u64,
    #[serde(flatten)]
    pub outcome: Recorded,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Recorded {
    NothingSynced,
    Current {
        generation: u64,
        paths: Vec<String>,
    },
    Pushed {
        generation: u64,
        paths: Vec<String>,
        pushed: Vec<PushedPath>,
    },
    /// The run failed with `config_sync_failed`; `cause` is the underlying code.
    Failed {
        cause: String,
    },
}

impl From<Synced> for Recorded {
    fn from(synced: Synced) -> Self {
        match synced {
            Synced::NothingSynced => Recorded::NothingSynced,
            Synced::Current { generation, paths } => Recorded::Current { generation, paths },
            Synced::Pushed {
                generation,
                paths,
                pushed,
            } => Recorded::Pushed {
                generation,
                paths,
                pushed,
            },
        }
    }
}

/// Bounded, privacy-safe history of pre-run I/O, including successful retries.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attempt {
    pub member: String,
    pub started_at: Timestamp,
    pub duration_ms: u64,
    pub cause: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct History {
    pub attempts: Vec<Attempt>,
    pub complete: bool,
    pub truncated: bool,
}
impl Default for History {
    fn default() -> Self {
        Self {
            attempts: Vec::new(),
            complete: true,
            truncated: false,
        }
    }
}
impl History {
    pub const MAXIMUM_ATTEMPTS: usize = 4096;
    pub fn push(&mut self, attempt: Attempt) {
        if self.attempts.len() < Self::MAXIMUM_ATTEMPTS {
            self.attempts.push(attempt);
        } else {
            self.truncated = true;
            self.complete = false;
        }
    }
}

/// A failure's code as Elsecall may repeat it. Codes can come from the member,
/// so anything but a short snake_case identifier is not repeated.
pub(crate) fn cause_code(cause: &Failure) -> String {
    let code = cause.code.as_str();
    let plain = !code.is_empty()
        && code.len() <= 48
        && code.starts_with(|c: char| c.is_ascii_lowercase())
        && code
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if plain {
        code.to_owned()
    } else {
        "member_error".to_owned()
    }
}

fn hint(member: &str) -> String {
    format!(
        "Run `elsecall sync push --member {member}` to see why and retry the sync, fix what it reports, then rerun the same command."
    )
}

/// A sync failure that fails the run with `config_sync_failed`.
#[derive(Debug)]
pub struct SyncFailure {
    pub failure: Failure,
    /// The underlying code, as the record keeps it.
    pub cause: String,
}

/// What a failed sync means for the run.
#[derive(Debug)]
pub enum Failed {
    /// The member could not be reached: placement treats it as an offer that
    /// could not reach the member.
    Unavailable(Failure),
    /// This member is no longer master, so it places nothing; the member is
    /// fine. The run fails with `member_unavailable`, as an offer refused for
    /// that reason did before.
    NotMaster(Failure),
    Sync(SyncFailure),
}

fn sync_failure(member: &str, what: String, cause: String) -> SyncFailure {
    let failure = Failure::new(CODE, what, hint(member))
        .with_details(serde_json::json!({"member": member, "cause": cause}));
    SyncFailure { failure, cause }
}

/// What `cause`, the error of bringing `member` up to date, means for the run.
/// The cause's own message is never repeated, since it may come from the member.
pub fn failed(member: &str, cause: &Failure) -> Failed {
    match cause.code.as_str() {
        "member_unavailable" | "member_rejected" | "member_unverified" => {
            Failed::Unavailable(Failure::new(
                "member_unavailable",
                format!(
                    "{member} could not be reached to bring its synced configuration up to date."
                ),
                format!(
                    "Check that {member} is on, connected to Tailscale and running Elsecall (elsecall doctor there)."
                ),
            ))
        }
        "not_master" => Failed::NotMaster(Failure::new(
            "member_unavailable",
            format!(
                "This member is no longer the fleet's master, so it cannot bring {member} up to date or place runs there."
            ),
            "Run elsecall fleet to see the master, then rerun the same command.",
        )),
        _ => {
            let code = cause_code(cause);
            Failed::Sync(sync_failure(
                member,
                format!(
                    "The synced configuration on {member} could not be brought up to date before the run ({code}), so the command did not run there."
                ),
                code,
            ))
        }
    }
}

/// The run fails: the sync to `member` was still running when the hold limit
/// passed.
pub fn timed_out(member: &str, limit_seconds: u64) -> SyncFailure {
    sync_failure(
        member,
        format!(
            "The synced configuration on {member} was still being brought up to date when the {limit_seconds} s hold limit passed, so the command did not run there."
        ),
        "hold_limit_passed".into(),
    )
}

/// A sync that succeeded before an offer, and how long it took.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Done {
    pub member: String,
    pub duration_ms: u64,
    pub synced: Synced,
}

impl Done {
    pub fn record(&self) -> PreRunSyncRecord {
        PreRunSyncRecord {
            member: self.member.clone(),
            duration_ms: self.duration_ms,
            outcome: self.synced.clone().into(),
        }
    }
}

/// A placeable request whose pre-run sync failed on the member placement chose,
/// and where placement sent it instead. That member is left out of the request's
/// placement from then on.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fallback {
    /// The failed sync: the member, how long it took, and its cause code.
    pub failed_sync: PreRunSyncRecord,
    pub decided_at: Timestamp,
    /// Where placement sent the request instead: the caller or another member;
    /// `None` while no remaining member could start it in time, and it waited
    /// for one within its hold limit.
    pub moved_to: Option<String>,
    pub reason: String,
}

impl PreRunSyncRecord {
    pub fn failed(member: &str, duration_ms: u64, cause: String) -> Self {
        Self {
            member: member.to_owned(),
            duration_ms,
            outcome: Recorded::Failed { cause },
        }
    }
}

#[cfg(test)]
mod history_tests {
    use super::*;
    #[test]
    fn attempt_history_marks_its_bound_instead_of_claiming_completeness() {
        let mut history = History::default();
        let attempt = Attempt {
            member: "devbox".into(),
            started_at: Timestamp::from_millis(1_000),
            duration_ms: 500,
            cause: None,
        };
        for _ in 0..History::MAXIMUM_ATTEMPTS {
            history.push(attempt.clone());
        }
        assert!(history.complete && !history.truncated);
        history.push(attempt);
        assert_eq!(history.attempts.len(), History::MAXIMUM_ATTEMPTS);
        assert!(!history.complete && history.truncated);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_plain_codes_are_repeated() {
        let cause = |code: &str| Failure::new(code, "secret=hunter2", "x");
        let sync = |code: &str| match failed("bravo", &cause(code)) {
            Failed::Sync(sync) => sync,
            Failed::Unavailable(failure) | Failed::NotMaster(failure) => panic!("{failure:?}"),
        };
        assert_eq!(sync("unsafe_sync_path").cause, "unsafe_sync_path");
        for odd in [
            "",
            "Bad",
            "has space",
            "x\ny",
            "a".repeat(49).as_str(),
            "1abc",
        ] {
            assert_eq!(sync(odd).cause, "member_error", "{odd:?}");
        }
        let failure = sync("io_error").failure;
        assert_eq!(failure.code, CODE);
        assert!(!failure.message.contains("hunter2"));
        assert!(failure.hint.contains("sync push --member bravo"));
        assert_eq!(failure.details.unwrap()["cause"], "io_error");
        // Unreachable members and a lost master role keep their old meaning.
        for code in ["member_unavailable", "member_rejected", "not_master"] {
            match failed("bravo", &cause(code)) {
                Failed::Unavailable(failure) | Failed::NotMaster(failure) => {
                    assert_eq!(failure.code, "member_unavailable");
                    assert!(!failure.message.contains("hunter2"));
                }
                Failed::Sync(sync) => panic!("{code}: {sync:?}"),
            }
        }
    }

    #[test]
    fn records_flatten_their_outcome() {
        let record = PreRunSyncRecord {
            member: "bravo".into(),
            duration_ms: 42,
            outcome: Recorded::Pushed {
                generation: 3,
                paths: vec!["~/.config/demo".into()],
                pushed: vec![PushedPath {
                    path: "~/.config/demo".into(),
                    files: 1,
                    bytes: 12,
                }],
            },
        };
        let value = serde_json::to_value(&record).unwrap();
        assert_eq!(value["outcome"], "pushed");
        assert_eq!(value["generation"], 3);
        assert_eq!(value["pushed"][0]["bytes"], 12);
        let back: PreRunSyncRecord = serde_json::from_value(value).unwrap();
        assert_eq!(back, record);
        let failed = PreRunSyncRecord {
            member: "bravo".into(),
            duration_ms: 5,
            outcome: Recorded::Failed {
                cause: "member_unavailable".into(),
            },
        };
        assert_eq!(
            PreRunSyncRecord::failed("bravo", 5, "member_unavailable".into()),
            failed
        );
        let value = serde_json::to_value(&failed).unwrap();
        assert_eq!(value["outcome"], "failed");
        assert_eq!(
            serde_json::from_value::<PreRunSyncRecord>(value).unwrap(),
            failed
        );
    }
}
