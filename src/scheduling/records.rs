//! Durable request records in SQLite.
use crate::{
    failure::{Failure, Result},
    scheduling::{
        estimates,
        record::{Record, State},
        run_id::RunId,
        timestamp::Timestamp,
    },
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::{
    fs::OpenOptions,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::Mutex,
    time::Duration,
};

/// Ended records older than this are deleted when the service starts.
pub const RETENTION: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// Effective scheduler settings since this service started. Service settings
/// are immutable until restart, so these events preserve changes exactly.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SchedulerSettings {
    pub at: Timestamp,
    pub capacity: crate::scheduling::capacity::Capacity,
    pub parameters: crate::scheduling::parameters::Parameters,
}

pub struct Records {
    connection: Mutex<Connection>,
    /// For long reads that must not hold up admission's writes: each opens its
    /// own read-only connection, which WAL lets read alongside them.
    path: PathBuf,
}

fn failure(error: rusqlite::Error) -> Failure {
    Failure::new(
        "records_unavailable",
        format!("The request records could not be read or written: {error}"),
        "Run elsecall doctor; if the records file is damaged, stop the service and move records.sqlite aside.",
    )
}

impl Records {
    pub fn open(path: &Path) -> Result<Self> {
        // Create the file private before SQLite opens it; SQLite gives its WAL and
        // shared-memory files the same mode.
        OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .map_err(|e| Failure::io("Open request records", e))?;
        let connection = Connection::open(path).map_err(failure)?;
        // Wait out a brief lock, as when a stopping instance finishes a write.
        connection
            .busy_timeout(std::time::Duration::from_secs(5))
            .map_err(failure)?;
        connection
            .execute_batch(
                "PRAGMA journal_mode = WAL;
                 PRAGMA synchronous = NORMAL;
                 CREATE TABLE IF NOT EXISTS requests (
                     run_id TEXT PRIMARY KEY,
                     label TEXT,
                     state TEXT NOT NULL,
                     requested_at INTEGER NOT NULL,
                     run_ms INTEGER,
                     completed INTEGER NOT NULL,
                     record TEXT NOT NULL
                 ) STRICT;
                 CREATE TABLE IF NOT EXISTS scheduler_settings (
                     at INTEGER PRIMARY KEY,
                     settings TEXT NOT NULL
                 ) STRICT;
                 CREATE INDEX IF NOT EXISTS requests_by_time ON requests (requested_at);
                 CREATE INDEX IF NOT EXISTS requests_active ON requests (state) WHERE state != 'ended';
                 CREATE INDEX IF NOT EXISTS requests_left_running
                     ON requests (json_extract(record, '$.placed.leftovers.state'))
                     WHERE json_extract(record, '$.placed.leftovers.state') = 'running';
                 CREATE INDEX IF NOT EXISTS requests_by_label ON requests (label, requested_at)
                     WHERE completed = 1;
                 CREATE INDEX IF NOT EXISTS requests_by_rule
                     ON requests (json_extract(record, '$.settings.rule.match'), requested_at)
                     WHERE completed = 1;",
            )
            .map_err(failure)?;
        Ok(Self {
            connection: Mutex::new(connection),
            path: path.to_owned(),
        })
    }

    /// A read-only connection of its own, for a long read.
    fn reader(&self) -> Result<Connection> {
        let connection = Connection::open_with_flags(
            &self.path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(failure)?;
        connection
            .busy_timeout(std::time::Duration::from_secs(5))
            .map_err(failure)?;
        Ok(connection)
    }

    fn connection(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.connection.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Read-only adapter for offline export; never creates, migrates or prunes.
    pub fn open_read_only(path: &Path) -> Result<Self> {
        let connection =
            Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .map_err(failure)?;
        connection
            .busy_timeout(Duration::from_secs(5))
            .map_err(failure)?;
        Ok(Self {
            connection: Mutex::new(connection),
            path: path.to_owned(),
        })
    }

    /// Retained arrivals and carry-in still active at the window's start.
    pub fn overlapping(
        &self,
        since: Timestamp,
        until: Timestamp,
        before: Option<(Timestamp, &str)>,
        limit: u32,
    ) -> Result<Vec<Record>> {
        let connection = self.reader()?;
        let mut statement=connection.prepare("SELECT record FROM requests
            WHERE requested_at < ?2
              AND (requested_at >= ?1 OR state != 'ended' OR json_extract(record, '$.ended_at') >= ?5
                 OR json_extract(record, '$.placed.leftovers.state') = 'running'
                 OR (json_extract(record, '$.placed.leftovers.state') = 'ended'
                     AND (json_extract(record, '$.placed.leftovers.duration_ms') IS NULL
                          OR unixepoch(json_extract(record, '$.placed.leftovers.since'), 'subsec') * 1000
                             + json_extract(record, '$.placed.leftovers.duration_ms') >= ?1)))
              AND (requested_at < ?3 OR (requested_at = ?3 AND run_id < ?4))
            ORDER BY requested_at DESC, run_id DESC LIMIT ?6").map_err(failure)?;
        let (at, id) = before.unwrap_or((until, ""));
        statement
            .query_map(
                params![
                    since.millis() as i64,
                    until.millis() as i64,
                    at.millis() as i64,
                    id,
                    since.to_string(),
                    limit
                ],
                |row| Self::decode(row.get(0)?),
            )
            .map_err(failure)?
            .collect::<rusqlite::Result<_>>()
            .map_err(failure)
    }

    pub fn save_scheduler_settings(&self, settings: &SchedulerSettings) -> Result<()> {
        self.connection()
            .execute(
                "INSERT OR REPLACE INTO scheduler_settings (at, settings) VALUES (?1, ?2)",
                params![
                    settings.at.millis() as i64,
                    serde_json::to_string(settings).expect("settings serialize")
                ],
            )
            .map_err(failure)?;
        Ok(())
    }

    /// Baseline at the start, followed by each retained change before the end.
    /// Old databases have no settings table; missing evidence stays empty.
    pub fn scheduler_settings(
        &self,
        since: Timestamp,
        until: Timestamp,
    ) -> Result<Vec<SchedulerSettings>> {
        let connection = self.reader()?;
        let present: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'scheduler_settings')",
            [], |r| r.get(0),
        ).map_err(failure)?;
        if !present {
            return Ok(Vec::new());
        }
        let mut statement = connection
            .prepare(
                "SELECT settings FROM scheduler_settings WHERE at < ?2 AND
             (at >= ?1 OR at = (SELECT max(at) FROM scheduler_settings WHERE at < ?1))
             ORDER BY at",
            )
            .map_err(failure)?;
        statement
            .query_map(params![since.millis() as i64, until.millis() as i64], |r| {
                let text: String = r.get(0)?;
                serde_json::from_str(&text).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })
            })
            .map_err(failure)?
            .collect::<rusqlite::Result<_>>()
            .map_err(failure)
    }

    pub fn save(&self, record: &Record) -> Result<()> {
        let state = match record.state {
            State::Queued => "queued",
            State::Running => "running",
            State::Ended => "ended",
        };
        let completed =
            record.outcome.as_ref().is_some_and(|o| o.completed()) && record.run_ms.is_some();
        self.connection()
            .execute(
                "INSERT OR REPLACE INTO requests
                     (run_id, label, state, requested_at, run_ms, completed, record)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    record.run_id.as_str(),
                    record.label,
                    state,
                    record.requested_at.millis() as i64,
                    record.run_ms.map(|ms| ms as i64),
                    completed,
                    serde_json::to_string(record).expect("records serialize"),
                ],
            )
            .map_err(failure)?;
        Ok(())
    }

    fn decode(json: String) -> rusqlite::Result<Record> {
        serde_json::from_str(&json).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, e.into())
        })
    }

    pub fn get(&self, id: &RunId) -> Result<Option<Record>> {
        self.connection()
            .query_row(
                "SELECT record FROM requests WHERE run_id = ?1",
                [id.as_str()],
                |row| row.get(0),
            )
            .optional()
            .map_err(failure)?
            .map(|json| Self::decode(json).map_err(failure))
            .transpose()
    }

    fn query(&self, sql: &str, params: impl rusqlite::Params) -> Result<Vec<Record>> {
        let connection = self.connection();
        let mut statement = connection.prepare(sql).map_err(failure)?;
        statement
            .query_map(params, |row| Self::decode(row.get(0)?))
            .map_err(failure)?
            .collect::<rusqlite::Result<_>>()
            .map_err(failure)
    }

    /// The most recent requests, newest first.
    pub fn recent(&self, limit: u32) -> Result<Vec<Record>> {
        self.query(
            "SELECT record FROM requests ORDER BY requested_at DESC, rowid DESC LIMIT ?1",
            [limit],
        )
    }

    /// Requests that had not ended when the service last stopped.
    pub fn unfinished(&self) -> Result<Vec<Record>> {
        self.query(
            "SELECT record FROM requests WHERE state != 'ended' ORDER BY requested_at",
            [],
        )
    }

    /// Placed runs hosted here whose leftovers were still running when the
    /// service last stopped, ended or not.
    pub fn running_leftovers(&self) -> Result<Vec<Record>> {
        self.query(
            "SELECT record FROM requests
             WHERE json_extract(record, '$.placed.leftovers.state') = 'running'
             ORDER BY requested_at",
            [],
        )
    }

    /// Current production work and all production arrivals after a fixed epoch.
    /// Includes short work that starts and finishes between rehearsal polls.
    pub fn production_activity(&self, since: Timestamp) -> Result<(usize, usize)> {
        let connection = self.connection();
        // Indexed active bindings and arrivals avoid reparsing every retained
        // decision on each safety poll of a long-lived production service.
        let count = |filter: &str, arguments: &[&dyn rusqlite::ToSql]| -> Result<usize> {
            connection.query_row(&format!("SELECT COUNT(*) FROM requests WHERE {filter} AND (label IS NULL OR substr(label,1,10) != 'rehearsal:') AND COALESCE(json_extract(record,'$.synthetic'),0) != 1 AND COALESCE(json_extract(record,'$.internal'),0) != 1"),arguments,|row|row.get::<_,i64>(0)).map(|n|n as usize).map_err(failure)
        };
        let active = count("state != 'ended'", &[])?
            + count(
                "state = 'ended' AND json_extract(record, '$.placed.leftovers.state') = 'running'",
                &[],
            )?;
        let arrivals = count("requested_at >= ?1", &[&(since.millis() as i64)])?;
        Ok((active, arrivals))
    }

    /// Run times of the label's latest completed requests.
    pub fn run_times(&self, label: &str) -> Result<Vec<Duration>> {
        if crate::rehearsal::synthetic(Some(label)) {
            return Ok(Vec::new());
        }
        let connection = self.connection();
        let mut statement = connection
            .prepare(
                "SELECT run_ms FROM requests WHERE label = ?1 AND completed = 1
                 ORDER BY requested_at DESC LIMIT ?2",
            )
            .map_err(failure)?;
        statement
            .query_map(params![label, estimates::HISTORY as i64], |row| {
                Ok(Duration::from_millis(row.get::<_, i64>(0)? as u64))
            })
            .map_err(failure)?
            .collect::<rusqlite::Result<_>>()
            .map_err(failure)
    }

    /// The calling side of placed runs since `since`, newest first.
    pub fn placed_since(&self, since: Timestamp, limit: u32) -> Result<Vec<Record>> {
        self.query(
            "SELECT record FROM requests WHERE requested_at >= ?1
                 AND json_extract(record, '$.placed.side') = 'caller'
                 AND (label IS NULL OR substr(label, 1, 10) != 'rehearsal:')
                 AND COALESCE(json_extract(record, '$.synthetic'), 0) != 1
             ORDER BY requested_at DESC LIMIT ?2",
            params![since.millis() as i64, limit],
        )
    }

    /// The latest `limit` placeable requests since `since`, oldest first. Read
    /// on a connection of its own, so admission writes on meanwhile.
    pub fn placeable_since(&self, since: Timestamp, limit: u32) -> Result<Vec<Record>> {
        let connection = self.reader()?;
        let mut statement = connection
            .prepare(
                "SELECT record FROM requests WHERE requested_at >= ?1
                     AND json_extract(record, '$.placement') = 'placeable'
                 ORDER BY requested_at DESC, rowid DESC LIMIT ?2",
            )
            .map_err(failure)?;
        let mut records = statement
            .query_map(params![since.millis() as i64, limit], |row| {
                Self::decode(row.get(0)?)
            })
            .map_err(failure)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(failure)?;
        records.reverse();
        Ok(records)
    }

    /// The label's latest completed requests since `since`, newest first,
    /// without their decisions (see [`Self::learning_read`]).
    pub fn history(&self, label: &str, since: Timestamp, limit: u32) -> Result<Vec<Record>> {
        self.learning_read(
            "label = ?1 AND completed = 1 AND requested_at >= ?2",
            params![label, since.millis() as i64, limit],
        )
    }

    /// The latest completed requests whose label matched the label rule
    /// `pattern` since `since`, other than `label`'s and those that took less
    /// than `shortest`, newest first, without their decisions. Indexed by the
    /// rule, so a rare family is found without reading every recent record.
    pub fn family_history(
        &self,
        pattern: &str,
        label: &str,
        shortest: Duration,
        since: Timestamp,
        limit: u32,
    ) -> Result<Vec<Record>> {
        self.learning_read(
            "json_extract(record, '$.settings.rule.match') = ?1 AND completed = 1
                 AND requested_at >= ?2 AND label IS NOT ?3 AND run_ms >= ?4",
            params![
                pattern,
                since.millis() as i64,
                label,
                shortest.as_millis() as i64,
                limit
            ],
        )
    }

    /// The latest labelled completed requests since `since`, newest first,
    /// without their decisions.
    pub fn labelled_since(&self, since: Timestamp, limit: u32) -> Result<Vec<Record>> {
        self.learning_read(
            "label IS NOT NULL AND completed = 1 AND requested_at >= ?1",
            params![since.millis() as i64, limit],
        )
    }

    /// Records for learning, newest first, where `filter` selects them and the
    /// last of `params` is the limit. Learning never reads a decision, the bulk
    /// of a placed run's record, so SQLite drops it before decoding; the read
    /// uses a connection of its own, so admission writes on meanwhile.
    fn learning_read(&self, filter: &str, params: &[&dyn rusqlite::ToSql]) -> Result<Vec<Record>> {
        let connection = self.reader()?;
        let mut statement = connection
            .prepare(&format!(
                "SELECT json_set(record, '$.decision', NULL) FROM requests WHERE {filter}
                 AND (label IS NULL OR substr(label, 1, 10) != 'rehearsal:')
                 AND COALESCE(json_extract(record, '$.synthetic'), 0) != 1
                 ORDER BY requested_at DESC, rowid DESC LIMIT ?{}",
                params.len()
            ))
            .map_err(failure)?;
        statement
            .query_map(params, |row| Self::decode(row.get(0)?))
            .map_err(failure)?
            .collect::<rusqlite::Result<_>>()
            .map_err(failure)
    }

    /// Requests made from `since` to before `until`, newest first, starting below
    /// `before` (a request time and run ID) when given.
    pub fn between(
        &self,
        since: Timestamp,
        until: Timestamp,
        before: Option<(Timestamp, &str)>,
        limit: u32,
    ) -> Result<Vec<Record>> {
        let (before_at, before_id) = before.map_or((until.millis() as i64, ""), |(at, id)| {
            (at.millis() as i64, id)
        });
        self.query(
            "SELECT record FROM requests WHERE requested_at >= ?1 AND requested_at < ?2
                 AND (requested_at < ?3 OR (requested_at = ?3 AND run_id < ?4))
             ORDER BY requested_at DESC, run_id DESC LIMIT ?5",
            params![
                since.millis() as i64,
                until.millis() as i64,
                before_at,
                before_id,
                limit
            ],
        )
    }

    pub fn prune(&self, now: Timestamp) -> Result<()> {
        let cutoff = now.millis().saturating_sub(RETENTION.as_millis() as u64);
        let connection = self.connection();
        connection
            .execute(
                "DELETE FROM requests WHERE state = 'ended' AND requested_at < ?1",
                [cutoff as i64],
            )
            .map_err(failure)?;
        // Keep the last pre-retention event as the baseline for a service that
        // has run longer than retention.
        connection
            .execute(
                "DELETE FROM scheduler_settings WHERE at < ?1 AND at <
             (SELECT max(at) FROM scheduler_settings WHERE at < ?1)",
                [cutoff as i64],
            )
            .map_err(failure)?;
        crate::sqlite_retention::reclaim(&connection).map_err(failure)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn settings_history_preserves_baseline_changes_and_retention() {
        use crate::scheduling::{
            capacity::{Capacity, Cores},
            parameters::Parameters,
        };
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.sqlite");
        let records = Records::open(&path).unwrap();
        let day = 86_400_000;
        for n in [1, 2, 15, 39] {
            records
                .save_scheduler_settings(&SchedulerSettings {
                    at: Timestamp::from_millis(n * day),
                    capacity: Capacity {
                        weighted_cores: Cores::from_millis(4000),
                        reserved: Cores::ONE,
                        capacity: Cores::from_millis(3000),
                    },
                    parameters: Parameters::default(),
                })
                .unwrap();
        }
        let history = || {
            records
                .scheduler_settings(
                    Timestamp::from_millis(11 * day),
                    Timestamp::from_millis(40 * day),
                )
                .unwrap()
        };
        assert_eq!(
            history()
                .iter()
                .map(|s| s.at.millis() / day)
                .collect::<Vec<_>>(),
            [2, 15, 39]
        );
        records.prune(Timestamp::from_millis(40 * day)).unwrap();
        assert_eq!(
            history()
                .iter()
                .map(|s| s.at.millis() / day)
                .collect::<Vec<_>>(),
            [2, 15, 39]
        );
        let read_only = Records::open_read_only(&path).unwrap();
        assert_eq!(
            history(),
            read_only
                .scheduler_settings(
                    Timestamp::from_millis(11 * day),
                    Timestamp::from_millis(40 * day)
                )
                .unwrap()
        );
        // A pre-H2 database is read without adding a schema or inventing defaults.
        let legacy_path = temp.path().join("legacy.sqlite");
        let connection = Connection::open(&legacy_path).unwrap();
        connection
            .execute_batch("CREATE TABLE legacy (id INTEGER);")
            .unwrap();
        let legacy = Records::open_read_only(&legacy_path).unwrap();
        assert!(
            legacy
                .scheduler_settings(Timestamp::from_millis(0), Timestamp::from_millis(day))
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE name = 'scheduler_settings'",
                    [],
                    |r| r.get::<_, u32>(0)
                )
                .unwrap(),
            0
        );
    }
    use crate::scheduling::{
        capacity::Cores,
        record::Outcome,
        request::{Kind, Placement, Urgency},
    };

    fn record(
        label: &str,
        requested: u64,
        run_ms: Option<u64>,
        outcome: Option<Outcome>,
    ) -> Record {
        Record {
            plan_observation: None,
            workload: None,
            run_id: RunId::generate(),
            kind: Kind::Run,
            synthetic: false,
            internal: false,
            peer_io: None,
            label: Some(label.into()),
            placement: Placement::Pinned,
            urgency: Urgency::Interactive {
                hold_limit_ms: None,
            },
            size: Cores::ONE,
            size_basis: None,
            calling_member: "m".into(),
            workspace: None,
            workspace_recorded: true,
            member: None,
            placement_note: None,
            nested_in: None,
            shares_capacity_of: None,
            state: if outcome.is_some() {
                State::Ended
            } else {
                State::Running
            },
            outcome,
            requested_at: Timestamp::from_millis(requested),
            started_at: None,
            ended_at: None,
            waited_ms: None,
            run_ms,
            run_phases: None,
            cpu_ms: None,
            hold_limit: None,
            decision: None,
            warnings: Vec::new(),
            requested_by: crate::scheduling::processes::ProcessIdentity {
                pid: 1,
                started_us: 1,
            },
            processes: Vec::new(),
            placed: None,
            settings: None,
            reacquired: 0,
            config_sync: None,
            pre_run_history: Some(Default::default()),
            fallbacks: Vec::new(),
        }
    }

    #[test]
    fn stores_lists_estimates_and_prunes() {
        let dir = tempfile::tempdir().unwrap();
        let records = Records::open(&dir.path().join("records.sqlite")).unwrap();
        let day = 24 * 60 * 60 * 1000;
        let now = Timestamp::from_millis(100 * day);
        let exited = Some(Outcome::Exited { exit_code: 1 });
        let old = record("build", 10 * day, Some(5), exited.clone());
        let fast = record("build", 99 * day, Some(40_000), exited.clone());
        let interrupted = record(
            "build",
            99 * day + 1,
            Some(1),
            Some(Outcome::Signaled { signal: 2 }),
        );
        let running = record("build", 99 * day + 2, None, None);
        for r in [&old, &fast, &interrupted, &running] {
            records.save(r).unwrap();
        }
        assert_eq!(records.get(&fast.run_id).unwrap().as_ref(), Some(&fast));
        assert_eq!(
            records.recent(2).unwrap(),
            vec![running.clone(), interrupted.clone()]
        );
        assert_eq!(records.unfinished().unwrap(), vec![running.clone()]);
        assert_eq!(
            records
                .history("build", Timestamp::from_millis(99 * day), 10)
                .unwrap(),
            vec![fast.clone()],
            "completed runs of the label since the cutoff"
        );
        assert_eq!(
            records.run_times("build").unwrap(),
            vec![Duration::from_millis(40_000), Duration::from_millis(5)],
            "only completed runs count, newest first"
        );
        records.prune(now).unwrap();
        assert_eq!(records.get(&old.run_id).unwrap(), None);
        assert_eq!(records.recent(10).unwrap().len(), 3);
    }

    #[test]
    fn learning_reads_families_and_every_label_without_decisions() {
        use crate::scheduling::{
            record::{Decision, Verdict},
            settings::{AppliedRule, Settings, Source},
        };
        let dir = tempfile::tempdir().unwrap();
        let records = Records::open(&dir.path().join("records.sqlite")).unwrap();
        let exited = Some(Outcome::Exited { exit_code: 0 });
        let in_family = |label: &str, requested, pattern: &str| {
            let mut r = record(label, requested, Some(1000), exited.clone());
            r.settings = Some(Settings {
                rule: Some(AppliedRule {
                    number: 1,
                    pattern: pattern.into(),
                }),
                size: Source::Rule,
                urgency: Source::Default,
                hold_limit: Some(Source::Default),
                deadline: None,
                at_deadline: None,
                placement: Source::Default,
            });
            r
        };
        let mut a = in_family("test a", 10, "test *");
        a.decision = Some(Decision {
            decided_at: Timestamp::from_millis(10),
            verdict: Verdict::Started,
            reason: "x".repeat(10_000),
            candidates: Vec::new(),
            unreachable: Vec::new(),
            parameters: None,
            scoring: None,
            arrival: None,
        });
        let b = in_family("test b", 20, "test *");
        let other = in_family("build", 30, "build *");
        let unruled = record("lint", 40, Some(1000), exited.clone());
        let unfinished = in_family("test c", 50, "test *");
        let unfinished = Record {
            outcome: None,
            state: State::Running,
            ..unfinished
        };
        for r in [&a, &b, &other, &unruled, &unfinished] {
            records.save(r).unwrap();
        }
        let lean = |r: &Record| Record {
            decision: None,
            ..r.clone()
        };
        let since = Timestamp::from_millis(0);
        let none = Duration::ZERO;
        assert_eq!(
            records
                .family_history("test *", "test c", none, since, 10)
                .unwrap(),
            vec![b.clone(), lean(&a)],
            "completed runs of the rule, newest first, decisions left out"
        );
        assert_eq!(
            records
                .family_history("test *", "test c", none, since, 1)
                .unwrap(),
            vec![b.clone()]
        );
        assert_eq!(
            records
                .family_history("test *", "test b", none, since, 10)
                .unwrap(),
            vec![lean(&a)],
            "the label's own runs are not its family's"
        );
        assert!(
            records
                .family_history("test *", "x", Duration::from_secs(2), since, 10)
                .unwrap()
                .is_empty(),
            "nor are runs shorter than asked"
        );
        // The rule's index finds a family without scanning every recent record.
        let plan: Vec<String> = Connection::open(dir.path().join("records.sqlite"))
            .unwrap()
            .prepare(
                "EXPLAIN QUERY PLAN SELECT record FROM requests
                 WHERE json_extract(record, '$.settings.rule.match') = ?1 AND completed = 1
                     AND requested_at >= ?2 AND label IS NOT ?3 AND run_ms >= ?4",
            )
            .unwrap()
            .query_map(params!["test *", 0, "x", 0], |row| row.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(
            plan.iter().any(|step| step.contains("requests_by_rule")),
            "{plan:?}"
        );
        assert_eq!(
            records.labelled_since(since, 10).unwrap(),
            vec![unruled, other, b, lean(&a)]
        );
        assert_eq!(
            records
                .labelled_since(Timestamp::from_millis(25), 10)
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn placeable_requests_read_oldest_first_from_the_latest() {
        let dir = tempfile::tempdir().unwrap();
        let records = Records::open(&dir.path().join("records.sqlite")).unwrap();
        let placeable = |requested| {
            let mut r = record("work", requested, Some(1), None);
            r.placement = Placement::Placeable;
            r
        };
        let (early, middle, late) = (placeable(10), placeable(20), placeable(30));
        let pinned = record("work", 25, Some(1), None);
        for r in [&late, &pinned, &early, &middle] {
            records.save(r).unwrap();
        }
        let since = |millis| Timestamp::from_millis(millis);
        assert_eq!(
            records.placeable_since(since(0), 10).unwrap(),
            vec![early.clone(), middle.clone(), late.clone()]
        );
        assert_eq!(
            records.placeable_since(since(0), 2).unwrap(),
            vec![middle.clone(), late.clone()],
            "the latest within the bound"
        );
        assert_eq!(
            records.placeable_since(since(15), 10).unwrap(),
            vec![middle, late]
        );
    }

    #[test]
    fn a_period_is_read_page_by_page_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let records = Records::open(&dir.path().join("records.sqlite")).unwrap();
        // Two requests share a millisecond; one is before the period, one after.
        let saved: Vec<Record> = [5, 10, 20, 20, 30, 40]
            .into_iter()
            .map(|at| record("build", at, None, None))
            .collect();
        for r in &saved {
            records.save(r).unwrap();
        }
        let (since, until) = (Timestamp::from_millis(10), Timestamp::from_millis(40));
        let mut read = Vec::new();
        let mut before = None;
        loop {
            let page = records
                .between(
                    since,
                    until,
                    before
                        .as_ref()
                        .map(|(at, id): &(Timestamp, String)| (*at, id.as_str())),
                    2,
                )
                .unwrap();
            let Some(last) = page.last() else { break };
            before = Some((last.requested_at, last.run_id.to_string()));
            read.extend(page.iter().map(|r| r.requested_at.millis()));
        }
        assert_eq!(read, [30, 20, 20, 10], "every request of the period, once");
    }
    #[test]
    fn synthetic_rows_cannot_displace_bounded_production_learning() {
        use crate::reporting::fixtures::{Run, placed};
        let temp = tempfile::tempdir().unwrap();
        let records = Records::open(&temp.path().join("records.sqlite")).unwrap();
        let production = placed(Run {
            n: 1,
            member: "devbox",
            ..Default::default()
        });
        records.save(&production).unwrap();
        for n in 2..12 {
            let mut synthetic = placed(Run {
                n,
                member: "devbox",
                label: "rehearsal:build",
                ..Default::default()
            });
            synthetic.synthetic = true;
            records.save(&synthetic).unwrap();
        }
        let since = Timestamp::from_millis(0);
        let lean = Record {
            decision: None,
            ..production.clone()
        };
        assert_eq!(
            records.labelled_since(since, 1).unwrap(),
            std::slice::from_ref(&lean)
        );
        assert_eq!(records.history("build", since, 1).unwrap(), [lean]);
        assert_eq!(records.placed_since(since, 1).unwrap(), [production]);
        assert!(records.run_times("rehearsal:build").unwrap().is_empty());
    }
}
