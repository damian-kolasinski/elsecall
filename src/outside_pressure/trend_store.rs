//! Trend rounds and early warnings in SQLite (`outside-processes.sqlite`), so a
//! restart does not restart a six-hour window and reports can name what
//! warned after it stopped. Rounds are kept for [`trend::KEPT`], warnings for
//! [`RETENTION`] after they end. Processes are kept by name only.
use crate::{
    failure::{Failure, Result},
    outside_pressure::{
        sampling::Owner,
        trend::{self, Round, Seen},
        warning::{Warning, WarningEpisode},
    },
    scheduling::timestamp::Timestamp,
};
use rusqlite::{Connection, params};
use std::{
    collections::HashSet, fs::OpenOptions, os::unix::fs::OpenOptionsExt, path::Path, sync::Mutex,
    time::Duration,
};

/// Warnings that ended longer ago than this are deleted, as load minutes are.
pub const RETENTION: Duration = crate::load_log::store::RETENTION;

pub struct TrendStore {
    connection: Mutex<Connection>,
}

fn failure(error: rusqlite::Error) -> Failure {
    Failure::new(
        "outside_processes_unavailable",
        format!("The outside process history could not be read or written: {error}"),
        "Run elsecall doctor; if outside-processes.sqlite is damaged, stop the service and move it aside.",
    )
}

fn owner_text(owner: Owner) -> &'static str {
    match owner {
        Owner::Root => "root",
        Owner::ThisUser => "this_user",
        Owner::OtherUser => "other_user",
    }
}

fn owner_of(text: &str) -> Option<Owner> {
    match text {
        "root" => Some(Owner::Root),
        "this_user" => Some(Owner::ThisUser),
        "other_user" => Some(Owner::OtherUser),
        _ => None,
    }
}

fn millis(at: Timestamp) -> i64 {
    at.millis() as i64
}

fn timestamp(ms: i64) -> Timestamp {
    Timestamp::from_millis(ms.max(0) as u64)
}

impl TrendStore {
    pub fn open(path: &Path) -> Result<Self> {
        OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .map_err(|e| Failure::io("Open outside process history", e))?;
        let connection = Connection::open(path).map_err(failure)?;
        connection
            .busy_timeout(Duration::from_secs(5))
            .map_err(failure)?;
        connection
            .execute_batch(
                "PRAGMA journal_mode = WAL;
                 PRAGMA synchronous = NORMAL;
                 CREATE TABLE IF NOT EXISTS rounds (at INTEGER PRIMARY KEY) STRICT;
                 CREATE TABLE IF NOT EXISTS seen (
                     at INTEGER NOT NULL,
                     pid INTEGER NOT NULL,
                     started INTEGER NOT NULL,
                     name TEXT NOT NULL,
                     owner TEXT NOT NULL,
                     cpu_cores REAL NOT NULL,
                     memory_bytes INTEGER NOT NULL,
                     PRIMARY KEY (at, pid)
                 ) STRICT;
                 CREATE TABLE IF NOT EXISTS warnings (
                     kind TEXT NOT NULL,
                     pid INTEGER NOT NULL,
                     started INTEGER NOT NULL,
                     since INTEGER NOT NULL,
                     observed_at INTEGER NOT NULL,
                     until INTEGER,
                     warning TEXT NOT NULL,
                     PRIMARY KEY (kind, pid, started, since)
                 ) STRICT;
                 CREATE TABLE IF NOT EXISTS own (
                     pid INTEGER NOT NULL,
                     started INTEGER NOT NULL,
                     PRIMARY KEY (pid, started)
                 ) STRICT;",
            )
            .map_err(failure)?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    fn connection(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.connection.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn record(&self, round: &Round) -> Result<()> {
        let mut connection = self.connection();
        let transaction = connection.transaction().map_err(failure)?;
        transaction
            .execute(
                "INSERT OR IGNORE INTO rounds (at) VALUES (?1)",
                [millis(round.at)],
            )
            .map_err(failure)?;
        for seen in &round.seen {
            transaction
                .execute(
                    "INSERT OR REPLACE INTO seen (at, pid, started, name, owner, cpu_cores, memory_bytes)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        millis(round.at),
                        seen.pid,
                        millis(seen.started),
                        seen.name,
                        owner_text(seen.owner),
                        seen.cpu_cores,
                        seen.memory_bytes.min(i64::MAX as u64) as i64,
                    ],
                )
                .map_err(failure)?;
        }
        transaction.commit().map_err(failure)
    }

    /// Remembers `own`, processes that are Elsecall's own work now, and
    /// returns every remembered process still `alive`; the rest are forgotten.
    pub fn remember_own(
        &self,
        own: &[(u32, Timestamp)],
        alive: &HashSet<(u32, Timestamp)>,
    ) -> Result<HashSet<(u32, Timestamp)>> {
        let mut connection = self.connection();
        let transaction = connection.transaction().map_err(failure)?;
        for (pid, started) in own {
            transaction
                .execute(
                    "INSERT OR IGNORE INTO own (pid, started) VALUES (?1, ?2)",
                    params![pid, millis(*started)],
                )
                .map_err(failure)?;
        }
        let remembered: Vec<(u32, Timestamp)> = transaction
            .prepare("SELECT pid, started FROM own")
            .map_err(failure)?
            .query_map([], |row| {
                Ok((row.get::<_, u32>(0)?, timestamp(row.get::<_, i64>(1)?)))
            })
            .map_err(failure)?
            .collect::<std::result::Result<_, _>>()
            .map_err(failure)?;
        let mut living = HashSet::new();
        for identity in remembered {
            if alive.contains(&identity) {
                living.insert(identity);
            } else {
                transaction
                    .execute(
                        "DELETE FROM own WHERE pid = ?1 AND started = ?2",
                        params![identity.0, millis(identity.1)],
                    )
                    .map_err(failure)?;
            }
        }
        transaction.commit().map_err(failure)?;
        Ok(living)
    }

    /// Rounds from `since` on, oldest first.
    pub fn rounds(&self, since: Timestamp) -> Result<Vec<Round>> {
        let connection = self.connection();
        let mut statement = connection
            .prepare("SELECT at FROM rounds WHERE at >= ?1 ORDER BY at")
            .map_err(failure)?;
        let mut rounds: Vec<Round> = statement
            .query_map([millis(since)], |row| row.get::<_, i64>(0))
            .map_err(failure)?
            .map(|at| {
                at.map(|at| Round {
                    at: timestamp(at),
                    seen: Vec::new(),
                })
            })
            .collect::<std::result::Result<_, _>>()
            .map_err(failure)?;
        let mut statement = connection
            .prepare(
                "SELECT at, pid, started, name, owner, cpu_cores, memory_bytes FROM seen
                 WHERE at >= ?1 ORDER BY at, pid",
            )
            .map_err(failure)?;
        let rows = statement
            .query_map([millis(since)], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, u32>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, f64>(5)?,
                    row.get::<_, i64>(6)?,
                ))
            })
            .map_err(failure)?;
        for row in rows {
            let (at, pid, started, name, owner, cpu_cores, memory_bytes) = row.map_err(failure)?;
            let Some(owner) = owner_of(&owner) else {
                continue;
            };
            if let Ok(index) = rounds.binary_search_by_key(&timestamp(at), |r| r.at) {
                rounds[index].seen.push(Seen {
                    pid,
                    started: timestamp(started),
                    name,
                    owner,
                    cpu_cores,
                    memory_bytes: memory_bytes.max(0) as u64,
                });
            }
        }
        Ok(rounds)
    }

    /// Records a new warning, or the latest observation of one, keyed by its
    /// concern, process and onset.
    pub fn raise(&self, warning: &Warning) -> Result<()> {
        self.connection()
            .execute(
                "INSERT INTO warnings (kind, pid, started, since, observed_at, until, warning)
                 VALUES (?1, ?2, ?3, ?4, ?5, NULL, ?6)
                 ON CONFLICT (kind, pid, started, since)
                 DO UPDATE SET observed_at = ?5, until = NULL, warning = ?6",
                params![
                    warning.concern.kind(),
                    warning.pid,
                    millis(warning.started),
                    millis(warning.since),
                    millis(warning.observed_at),
                    serde_json::to_string(warning).expect("warnings serialize"),
                ],
            )
            .map_err(failure)?;
        Ok(())
    }

    /// Ends a warning at the last time it was seen to hold.
    pub fn end(&self, warning: &Warning) -> Result<()> {
        self.connection()
            .execute(
                "UPDATE warnings SET until = observed_at
                 WHERE kind = ?1 AND pid = ?2 AND started = ?3 AND since = ?4",
                params![
                    warning.concern.kind(),
                    warning.pid,
                    millis(warning.started),
                    millis(warning.since),
                ],
            )
            .map_err(failure)?;
        Ok(())
    }

    /// Warnings that still hold, as last observed.
    pub fn open_warnings(&self) -> Result<Vec<Warning>> {
        Ok(self
            .query(
                "SELECT warning, until FROM warnings WHERE until IS NULL ORDER BY since",
                [],
            )?
            .into_iter()
            .map(|episode| episode.warning)
            .collect())
    }

    /// Warnings that held at some time from `since` to before `until`, oldest
    /// first.
    pub fn between(&self, since: Timestamp, until: Timestamp) -> Result<Vec<WarningEpisode>> {
        self.query(
            "SELECT warning, until FROM warnings
             WHERE since < ?2 AND (until IS NULL OR until >= ?1) ORDER BY since",
            [millis(since), millis(until)],
        )
    }

    fn query(&self, sql: &str, parameters: impl rusqlite::Params) -> Result<Vec<WarningEpisode>> {
        let connection = self.connection();
        let mut statement = connection.prepare(sql).map_err(failure)?;
        let rows = statement
            .query_map(parameters, |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<i64>>(1)?))
            })
            .map_err(failure)?;
        let mut episodes = Vec::new();
        for row in rows {
            let (warning, until) = row.map_err(failure)?;
            // A row this version cannot read is skipped, not fatal.
            if let Ok(warning) = serde_json::from_str(&warning) {
                episodes.push(WarningEpisode {
                    warning,
                    until: until.map(timestamp),
                });
            }
        }
        Ok(episodes)
    }

    /// Deletes rounds no window needs and warnings past retention.
    pub fn prune(&self, now: Timestamp) -> Result<()> {
        let rounds = now.millis().saturating_sub(trend::KEPT.as_millis() as u64) as i64;
        let warnings = now.millis().saturating_sub(RETENTION.as_millis() as u64) as i64;
        let connection = self.connection();
        connection
            .execute_batch(&format!(
                "DELETE FROM rounds WHERE at < {rounds};
                 DELETE FROM seen WHERE at < {rounds};
                 DELETE FROM warnings WHERE until IS NOT NULL AND until < {warnings};"
            ))
            .map_err(failure)?;
        crate::sqlite_retention::reclaim(&connection).map_err(failure)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outside_pressure::warning::Concern;

    fn seen(pid: u32, cpu_cores: f64) -> Seen {
        Seen {
            pid,
            started: Timestamp::from_millis(1_000),
            name: format!("p{pid}"),
            owner: Owner::Root,
            cpu_cores,
            memory_bytes: 3 << 30,
        }
    }

    fn warning(since: u64, observed_at: u64) -> Warning {
        Warning {
            concern: Concern::Spinning { cpu_cores: 1.0 },
            process: "fseventsd".into(),
            pid: 412,
            started: Timestamp::from_millis(1_000),
            owner: Owner::Root,
            since: Timestamp::from_millis(since),
            observed_at: Timestamp::from_millis(observed_at),
        }
    }

    #[test]
    fn keeps_rounds_and_warnings_and_prunes_them_separately() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("outside-processes.sqlite");
        let hour = 3_600_000;
        let now = 40 * 24 * hour;
        {
            let store = TrendStore::open(&path).unwrap();
            store
                .record(&Round {
                    at: Timestamp::from_millis(now - 13 * hour),
                    seen: vec![seen(1, 1.0)],
                })
                .unwrap();
            store
                .record(&Round {
                    at: Timestamp::from_millis(now - hour),
                    seen: vec![],
                })
                .unwrap();
            store
                .record(&Round {
                    at: Timestamp::from_millis(now),
                    seen: vec![seen(2, 0.5), seen(3, 2.0)],
                })
                .unwrap();
            store.raise(&warning(hour, 2 * hour)).unwrap();
            store.end(&warning(hour, 2 * hour)).unwrap();
            store.raise(&warning(now - 7 * hour, now - hour)).unwrap();
            store.raise(&warning(now - 7 * hour, now)).unwrap();
        }
        let store = TrendStore::open(&path).unwrap();
        let rounds = store
            .rounds(Timestamp::from_millis(now - 2 * hour))
            .unwrap();
        assert_eq!(rounds.len(), 2, "an empty round still counts");
        assert_eq!(rounds[0].seen, vec![]);
        assert_eq!(rounds[1].seen, vec![seen(2, 0.5), seen(3, 2.0)]);
        let open = store.open_warnings().unwrap();
        assert_eq!(
            open,
            vec![warning(now - 7 * hour, now)],
            "one row per onset"
        );
        let all = store
            .between(Timestamp::from_millis(0), Timestamp::from_millis(now))
            .unwrap();
        assert_eq!(all[0].until, Some(Timestamp::from_millis(2 * hour)));
        assert_eq!(all[1].until, None);

        store.prune(Timestamp::from_millis(now)).unwrap();
        assert_eq!(store.rounds(Timestamp::from_millis(0)).unwrap().len(), 2);
        assert_eq!(
            store
                .between(Timestamp::from_millis(0), Timestamp::from_millis(now))
                .unwrap()
                .len(),
            1,
            "the warning that ended 40 days ago is gone"
        );
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
