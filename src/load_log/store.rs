//! Minutes of load in SQLite, one row per minute, for [`RETENTION`].
use crate::{
    failure::{Failure, Result},
    load_log::minute::Minute,
    scheduling::timestamp::Timestamp,
};
use rusqlite::{Connection, params};
use std::{fs::OpenOptions, os::unix::fs::OpenOptionsExt, path::Path, sync::Mutex, time::Duration};

/// Minutes older than this are deleted: as long as request records are kept.
pub const RETENTION: Duration = crate::scheduling::records::RETENTION;

pub struct LoadStore {
    connection: Mutex<Connection>,
}

fn failure(error: rusqlite::Error) -> Failure {
    Failure::new(
        "load_history_unavailable",
        format!("The load history could not be read or written: {error}"),
        "Run elsecall doctor; if load.sqlite is damaged, stop the service and move it aside.",
    )
}

impl LoadStore {
    pub fn open(path: &Path) -> Result<Self> {
        OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .map_err(|e| Failure::io("Open load history", e))?;
        let connection = Connection::open(path).map_err(failure)?;
        connection
            .busy_timeout(Duration::from_secs(5))
            .map_err(failure)?;
        connection
            .execute_batch(
                "PRAGMA journal_mode = WAL;
                 PRAGMA synchronous = NORMAL;
                 CREATE TABLE IF NOT EXISTS minutes (
                     at INTEGER PRIMARY KEY,
                     minute TEXT NOT NULL
                 ) STRICT;",
            )
            .map_err(failure)?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    /// Offline export adapter; does not create schema or prune history.
    pub fn open_read_only(path: &Path) -> Result<Self> {
        let connection =
            Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .map_err(failure)?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    fn connection(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.connection.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn append(&self, minute: &Minute) -> Result<()> {
        self.connection()
            .execute(
                "INSERT OR REPLACE INTO minutes (at, minute) VALUES (?1, ?2)",
                params![
                    minute.at.millis() as i64,
                    serde_json::to_string(minute).expect("minutes serialize")
                ],
            )
            .map_err(failure)?;
        Ok(())
    }

    /// Minutes starting from `since` up to before `until`, oldest first.
    pub fn between(&self, since: Timestamp, until: Timestamp) -> Result<Vec<Minute>> {
        let connection = self.connection();
        let mut statement = connection
            .prepare("SELECT minute FROM minutes WHERE at >= ?1 AND at < ?2 ORDER BY at")
            .map_err(failure)?;
        let rows = statement
            .query_map(
                params![since.millis() as i64, until.millis() as i64],
                |row| row.get::<_, String>(0),
            )
            .map_err(failure)?;
        let mut minutes = Vec::new();
        for row in rows {
            // A row this version cannot read is skipped, not fatal.
            if let Ok(minute) = serde_json::from_str(&row.map_err(failure)?) {
                minutes.push(minute);
            }
        }
        Ok(minutes)
    }

    pub fn prune(&self, now: Timestamp) -> Result<()> {
        let cutoff = now.millis().saturating_sub(RETENTION.as_millis() as u64);
        let connection = self.connection();
        connection
            .execute("DELETE FROM minutes WHERE at < ?1", [cutoff as i64])
            .map_err(failure)?;
        crate::sqlite_retention::reclaim(&connection).map_err(failure)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduling::capacity::Cores;

    fn minute(at: u64) -> Minute {
        Minute {
            at: Timestamp::from_millis(at),
            seconds: 60,
            cpu_mean: Some(0.5),
            cpu_max: Some(0.75),
            outside_mean: Some(0.25),
            weighted_cores: Cores::from_millis(12_000),
            capacity: Cores::from_millis(9_000),
            in_use_mean: 3.0,
            in_use_max: Cores::from_millis(4_000),
            queued_max: 1,
            interactive_queued_max: Some(1),
            busy_seconds: 60,
            saturated_seconds: 0,
            saturated_busy_seconds: 0,
            memory_pressure_seconds: 0,
            worst_memory_pressure: None,
            worst_thermal_state: None,
            speed: Some(0.75),
        }
    }

    #[test]
    fn keeps_minutes_in_order_and_prunes_old_ones() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("load.sqlite");
        let store = LoadStore::open(&path).unwrap();
        let day = 86_400_000;
        let now = 40 * day;
        for at in [now - 60_000, now - 120_000, 2 * day] {
            store.append(&minute(at)).unwrap();
        }
        store.append(&minute(now - 60_000)).unwrap();
        let all = store
            .between(Timestamp::from_millis(0), Timestamp::from_millis(now))
            .unwrap();
        assert_eq!(
            all.iter().map(|m| m.at.millis()).collect::<Vec<_>>(),
            [2 * day, now - 120_000, now - 60_000]
        );
        assert_eq!(all[2], minute(now - 60_000));
        store.prune(Timestamp::from_millis(now)).unwrap();
        assert_eq!(
            store
                .between(Timestamp::from_millis(0), Timestamp::from_millis(now))
                .unwrap()
                .len(),
            2
        );
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
