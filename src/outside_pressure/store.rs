//! Blocked episodes in SQLite, one row per episode, kept for [`RETENTION`], so
//! reports can name what held a member closed after it cleared.
use crate::{
    failure::{Failure, Result},
    outside_pressure::blocked::Blocked,
    scheduling::timestamp::Timestamp,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::{fs::OpenOptions, os::unix::fs::OpenOptionsExt, path::Path, sync::Mutex, time::Duration};

/// Episodes that ended longer ago than this are deleted, as load minutes are.
pub const RETENTION: Duration = crate::load_log::store::RETENTION;

/// One blocked episode as last attributed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Episode {
    #[serde(flatten)]
    pub blocked: Blocked,
    /// When the gate last stayed closed; `None` while the episode lasts.
    pub until: Option<Timestamp>,
}

pub struct EpisodeStore {
    connection: Mutex<Connection>,
}

fn failure(error: rusqlite::Error) -> Failure {
    Failure::new(
        "outside_pressure_unavailable",
        format!("The outside pressure history could not be read or written: {error}"),
        "Run elsecall doctor; if outside-pressure.sqlite is damaged, stop the service and move it aside.",
    )
}

impl EpisodeStore {
    /// Opens the history. Episodes a stopped service left open end at their last
    /// attribution, the last moment they were known to last; the latest of them
    /// is returned, so a block that still lasts can keep its onset.
    pub fn open(path: &Path) -> Result<(Self, Option<Blocked>)> {
        OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .map_err(|e| Failure::io("Open outside pressure history", e))?;
        let connection = Connection::open(path).map_err(failure)?;
        connection
            .busy_timeout(Duration::from_secs(5))
            .map_err(failure)?;
        connection
            .execute_batch(
                "PRAGMA journal_mode = WAL;
                 PRAGMA synchronous = NORMAL;
                 CREATE TABLE IF NOT EXISTS episodes (
                     since INTEGER PRIMARY KEY,
                     attributed_at INTEGER NOT NULL,
                     until INTEGER,
                     blocked TEXT NOT NULL
                 ) STRICT;",
            )
            .map_err(failure)?;
        let interrupted = connection
            .query_row(
                "SELECT blocked FROM episodes WHERE until IS NULL ORDER BY since DESC LIMIT 1",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(failure)?
            .and_then(|blocked| serde_json::from_str(&blocked).ok());
        connection
            .execute(
                "UPDATE episodes SET until = attributed_at WHERE until IS NULL",
                [],
            )
            .map_err(failure)?;
        Ok((
            Self {
                connection: Mutex::new(connection),
            },
            interrupted,
        ))
    }

    fn connection(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.connection.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Records a new or newly attributed episode, keyed by its onset.
    pub fn record(&self, blocked: &Blocked) -> Result<()> {
        self.connection()
            .execute(
                "INSERT INTO episodes (since, attributed_at, until, blocked) VALUES (?1, ?2, NULL, ?3)
                 ON CONFLICT (since) DO UPDATE SET attributed_at = ?2, until = NULL, blocked = ?3",
                params![
                    blocked.since.millis() as i64,
                    blocked.attributed_at.millis() as i64,
                    serde_json::to_string(blocked).expect("blocked states serialize")
                ],
            )
            .map_err(failure)?;
        Ok(())
    }

    pub fn end(&self, since: Timestamp, until: Timestamp) -> Result<()> {
        self.connection()
            .execute(
                "UPDATE episodes SET until = ?2 WHERE since = ?1",
                params![since.millis() as i64, until.millis() as i64],
            )
            .map_err(failure)?;
        Ok(())
    }

    /// Episodes that overlap `since` to before `until`, oldest first.
    pub fn between(&self, since: Timestamp, until: Timestamp) -> Result<Vec<Episode>> {
        let connection = self.connection();
        let mut statement = connection
            .prepare(
                "SELECT blocked, until FROM episodes
                 WHERE since < ?2 AND (until IS NULL OR until >= ?1) ORDER BY since",
            )
            .map_err(failure)?;
        let rows = statement
            .query_map(
                params![since.millis() as i64, until.millis() as i64],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<i64>>(1)?)),
            )
            .map_err(failure)?;
        let mut episodes = Vec::new();
        for row in rows {
            let (blocked, ended) = row.map_err(failure)?;
            // A row this version cannot read is skipped, not fatal.
            if let Ok(blocked) = serde_json::from_str(&blocked) {
                episodes.push(Episode {
                    blocked,
                    until: ended.map(|ms| Timestamp::from_millis(ms as u64)),
                });
            }
        }
        Ok(episodes)
    }

    pub fn prune(&self, now: Timestamp) -> Result<()> {
        let cutoff = now.millis().saturating_sub(RETENTION.as_millis() as u64);
        let connection = self.connection();
        connection
            .execute(
                "DELETE FROM episodes WHERE until IS NOT NULL AND until < ?1",
                [cutoff as i64],
            )
            .map_err(failure)?;
        crate::sqlite_retention::reclaim(&connection).map_err(failure)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        inspection::MemoryPressure,
        outside_pressure::blocked::{Blocked, Reason},
    };

    fn blocked(since: u64, attributed: u64) -> Blocked {
        Blocked {
            reason: Reason::MemoryPressure {
                level: MemoryPressure::Warning,
            },
            since: Timestamp::from_millis(since),
            attributed_at: Timestamp::from_millis(attributed),
            outside: vec![],
        }
    }

    #[test]
    fn keeps_episodes_closes_abandoned_ones_on_open_and_prunes_old_ones() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("outside-pressure.sqlite");
        let day = 86_400_000;
        let now = 40 * day;
        {
            let (store, interrupted) = EpisodeStore::open(&path).unwrap();
            assert_eq!(interrupted, None);
            store.record(&blocked(2 * day, 2 * day + 300_000)).unwrap();
            store
                .end(
                    Timestamp::from_millis(2 * day),
                    Timestamp::from_millis(3 * day),
                )
                .unwrap();
            store
                .record(&blocked(now - 600_000, now - 300_000))
                .unwrap();
            store
                .record(&blocked(now - 600_000, now - 240_000))
                .unwrap();
            let open = store
                .between(
                    Timestamp::from_millis(now - 300_000),
                    Timestamp::from_millis(now),
                )
                .unwrap();
            assert_eq!(open[0].until, None);
            assert_eq!(
                open[0].blocked.attributed_at,
                Timestamp::from_millis(now - 240_000)
            );
        }
        let (store, interrupted) = EpisodeStore::open(&path).unwrap();
        assert_eq!(
            interrupted.unwrap().attributed_at,
            Timestamp::from_millis(now - 240_000),
            "the episode the restart cut short"
        );
        let latest = |store: &EpisodeStore| {
            store
                .between(
                    Timestamp::from_millis(now - 300_000),
                    Timestamp::from_millis(now),
                )
                .unwrap()
                .pop()
                .unwrap()
        };
        assert_eq!(
            latest(&store).until,
            Some(Timestamp::from_millis(now - 240_000)),
            "a restart ends the open episode at its last attribution"
        );
        let all = store
            .between(Timestamp::from_millis(0), Timestamp::from_millis(now))
            .unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(
            store
                .between(
                    Timestamp::from_millis(2 * day + 1),
                    Timestamp::from_millis(2 * day + 2)
                )
                .unwrap()
                .len(),
            1,
            "an episode that spans the period overlaps it"
        );
        store.prune(Timestamp::from_millis(now)).unwrap();
        assert_eq!(
            store
                .between(Timestamp::from_millis(0), Timestamp::from_millis(now))
                .unwrap()
                .len(),
            1
        );
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
