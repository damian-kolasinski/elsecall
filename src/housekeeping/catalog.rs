//! The member's durable account of its copies: where each lives and whose it is,
//! when it was last used, what it and each object store last measured, every
//! removal with its reason, and removals still being deleted.
use crate::{
    failure::{Failure, Result},
    housekeeping::clock::Clock,
    scheduling::timestamp::Timestamp,
    workspace::{
        copies::UseLog,
        location::{CopyLocation, Setting},
    },
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::OpenOptions,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

/// Removal records older than this are deleted.
const RETENTION_MS: u64 = 90 * 24 * 60 * 60 * 1000;

/// A copy removal its calling member has not been told of.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unnotified {
    /// The removal's place in the record, in the order removals happened.
    pub id: i64,
    pub path: String,
    pub removed_at: Timestamp,
}

/// A copy as the catalog knows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub location: CopyLocation,
    pub last_used: Timestamp,
    pub size: Option<Measured>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Measured {
    pub bytes: u64,
    pub at: Timestamp,
}

/// Why something was removed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    /// The calling member reported that the copy's workspace no longer exists.
    CallerDeleted,
    /// Not used for the idle period.
    Idle,
    /// Evicted, least recently used first, while free space was below the threshold.
    DiskPressure,
    /// An object store no copy uses any more.
    Unreferenced,
    /// The copy's directory was already gone; only Elsecall's state of it was removed.
    Missing,
}

/// What a removal removed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Removed {
    Copy {
        calling_member: String,
        caller_path: String,
        location: PathBuf,
        setting: Setting,
    },
    Store {
        calling_member: String,
        path: PathBuf,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Removal {
    pub removed_at: Timestamp,
    #[serde(flatten)]
    pub removed: Removed,
    pub reason: Reason,
    pub size_bytes: Option<u64>,
    pub last_used_at: Option<Timestamp>,
}

pub struct Catalog {
    connection: Mutex<Connection>,
    clock: Arc<dyn Clock>,
}

fn failure(error: rusqlite::Error) -> Failure {
    Failure::new(
        "copies_unavailable",
        format!("The record of workspace copies could not be read or written: {error}"),
        "Run elsecall doctor; if copies.sqlite is damaged, stop the service and move it aside. Copies are kept: the next housekeeping pass finds mapped copies again, and same-path copies once a run uses them.",
    )
}

fn setting_name(setting: Setting) -> &'static str {
    match setting {
        Setting::Mapped => "mapped",
        Setting::SamePath => "same-path",
    }
}

fn path(text: String) -> PathBuf {
    PathBuf::from(text)
}

impl Catalog {
    pub fn open(path: &Path, clock: Arc<dyn Clock>) -> Result<Self> {
        OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .map_err(|e| Failure::io("Open the record of workspace copies", e))?;
        let connection = Connection::open(path).map_err(failure)?;
        connection
            .busy_timeout(std::time::Duration::from_secs(5))
            .map_err(failure)?;
        connection
            .execute_batch(
                "PRAGMA journal_mode = WAL;
                 PRAGMA synchronous = NORMAL;
                 CREATE TABLE IF NOT EXISTS copies (
                     root TEXT PRIMARY KEY,
                     calling_member TEXT NOT NULL,
                     caller_root TEXT NOT NULL,
                     store TEXT NOT NULL,
                     setting TEXT NOT NULL,
                     state_file TEXT NOT NULL,
                     last_used_ms INTEGER NOT NULL,
                     size_bytes INTEGER,
                     measured_ms INTEGER
                 ) STRICT;
                 CREATE TABLE IF NOT EXISTS stores (
                     path TEXT PRIMARY KEY,
                     size_bytes INTEGER NOT NULL,
                     measured_ms INTEGER NOT NULL
                 ) STRICT;
                 CREATE TABLE IF NOT EXISTS removals (
                     id INTEGER PRIMARY KEY AUTOINCREMENT,
                     removed_ms INTEGER NOT NULL,
                     removal TEXT NOT NULL
                 ) STRICT;
                 CREATE TABLE IF NOT EXISTS trash (
                     path TEXT PRIMARY KEY,
                     device INTEGER NOT NULL,
                     inode INTEGER NOT NULL
                 ) STRICT;
                 CREATE TABLE IF NOT EXISTS notified (
                     calling_member TEXT PRIMARY KEY,
                     until_id INTEGER NOT NULL
                 ) STRICT;",
            )
            .map_err(failure)?;
        Ok(Self {
            connection: Mutex::new(connection),
            clock,
        })
    }

    fn connection(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.connection.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn now(&self) -> Timestamp {
        self.clock.now()
    }

    /// Notes that `location` was used at `at`, adding it if it is new.
    pub fn record_use(&self, location: &CopyLocation, at: Timestamp) -> Result<()> {
        self.connection()
            .execute(
                "INSERT INTO copies
                     (root, calling_member, caller_root, store, setting, state_file, last_used_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT (root) DO UPDATE SET
                     calling_member = excluded.calling_member,
                     caller_root = excluded.caller_root,
                     store = excluded.store,
                     setting = excluded.setting,
                     state_file = excluded.state_file,
                     last_used_ms = max(last_used_ms, excluded.last_used_ms)",
                params![
                    location.root.to_string_lossy(),
                    location.calling_member,
                    location.caller_root,
                    location.store.to_string_lossy(),
                    setting_name(location.setting),
                    location.state_file.to_string_lossy(),
                    at.millis() as i64,
                ],
            )
            .map_err(failure)?;
        Ok(())
    }

    /// Adds a copy found on disk that the catalog did not know, last used at
    /// `last_used`; a copy it knows is left as it is.
    pub fn adopt(&self, location: &CopyLocation, last_used: Timestamp) -> Result<bool> {
        let added = self
            .connection()
            .execute(
                "INSERT OR IGNORE INTO copies
                     (root, calling_member, caller_root, store, setting, state_file, last_used_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    location.root.to_string_lossy(),
                    location.calling_member,
                    location.caller_root,
                    location.store.to_string_lossy(),
                    setting_name(location.setting),
                    location.state_file.to_string_lossy(),
                    last_used.millis() as i64,
                ],
            )
            .map_err(failure)?;
        Ok(added > 0)
    }

    pub fn entries(&self) -> Result<Vec<Entry>> {
        let connection = self.connection();
        let mut statement = connection
            .prepare(
                "SELECT root, calling_member, caller_root, store, setting, state_file,
                        last_used_ms, size_bytes, measured_ms
                 FROM copies ORDER BY calling_member, caller_root, root",
            )
            .map_err(failure)?;
        statement
            .query_map([], |row| {
                let size: Option<i64> = row.get(7)?;
                let measured: Option<i64> = row.get(8)?;
                Ok(Entry {
                    location: CopyLocation {
                        root: path(row.get(0)?),
                        calling_member: row.get(1)?,
                        caller_root: row.get(2)?,
                        store: path(row.get(3)?),
                        setting: if row.get::<_, String>(4)? == "same-path" {
                            Setting::SamePath
                        } else {
                            Setting::Mapped
                        },
                        state_file: path(row.get(5)?),
                    },
                    last_used: Timestamp::from_millis(row.get::<_, i64>(6)? as u64),
                    size: size.zip(measured).map(|(bytes, at)| Measured {
                        bytes: bytes as u64,
                        at: Timestamp::from_millis(at as u64),
                    }),
                })
            })
            .map_err(failure)?
            .collect::<rusqlite::Result<_>>()
            .map_err(failure)
    }

    /// When the copy at `root` was last used; `None` if the catalog does not know it.
    pub fn last_used(&self, root: &Path) -> Result<Option<Timestamp>> {
        self.connection()
            .query_row(
                "SELECT last_used_ms FROM copies WHERE root = ?1",
                [root.to_string_lossy()],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map(|ms| ms.map(|ms| Timestamp::from_millis(ms as u64)))
            .map_err(failure)
    }

    pub fn set_size(&self, root: &Path, measured: Measured) -> Result<()> {
        self.connection()
            .execute(
                "UPDATE copies SET size_bytes = ?2, measured_ms = ?3 WHERE root = ?1",
                params![
                    root.to_string_lossy(),
                    measured.bytes as i64,
                    measured.at.millis() as i64
                ],
            )
            .map_err(failure)?;
        Ok(())
    }

    /// Forgets a removed copy.
    pub fn forget(&self, root: &Path) -> Result<()> {
        self.connection()
            .execute(
                "DELETE FROM copies WHERE root = ?1",
                [root.to_string_lossy()],
            )
            .map_err(failure)?;
        Ok(())
    }

    pub fn store_size(&self, store: &Path) -> Result<Option<Measured>> {
        self.connection()
            .query_row(
                "SELECT size_bytes, measured_ms FROM stores WHERE path = ?1",
                [store.to_string_lossy()],
                |row| {
                    Ok(Measured {
                        bytes: row.get::<_, i64>(0)? as u64,
                        at: Timestamp::from_millis(row.get::<_, i64>(1)? as u64),
                    })
                },
            )
            .optional()
            .map_err(failure)
    }

    pub fn set_store_size(&self, store: &Path, measured: Measured) -> Result<()> {
        self.connection()
            .execute(
                "INSERT OR REPLACE INTO stores (path, size_bytes, measured_ms) VALUES (?1, ?2, ?3)",
                params![
                    store.to_string_lossy(),
                    measured.bytes as i64,
                    measured.at.millis() as i64
                ],
            )
            .map_err(failure)?;
        Ok(())
    }

    pub fn forget_store(&self, store: &Path) -> Result<()> {
        self.connection()
            .execute(
                "DELETE FROM stores WHERE path = ?1",
                [store.to_string_lossy()],
            )
            .map_err(failure)?;
        Ok(())
    }

    pub fn record_removal(&self, removal: &Removal) -> Result<()> {
        let connection = self.connection();
        connection
            .execute(
                "INSERT INTO removals (removed_ms, removal) VALUES (?1, ?2)",
                params![
                    removal.removed_at.millis() as i64,
                    serde_json::to_string(&StoredRemoval::from(removal))
                        .expect("removals serialize"),
                ],
            )
            .map_err(failure)?;
        connection
            .execute(
                "DELETE FROM removals WHERE removed_ms < ?1",
                [removal.removed_at.millis().saturating_sub(RETENTION_MS) as i64],
            )
            .map_err(failure)?;
        Ok(())
    }

    /// Removed synthetic copies retain the identities needed to retry their
    /// shared-store cleanup after the last copy has left the live catalog.
    pub fn rehearsal_removals(&self, session: &str) -> Result<Vec<Removal>> {
        let connection = self.connection();
        let mut statement = connection
            .prepare(
                "SELECT removal FROM removals WHERE json_extract(removal, '$.caller_path') LIKE ?1",
            )
            .map_err(failure)?;
        statement
            .query_map(
                [format!("/private/tmp/elsecall-rehearsal-{session}-%")],
                |row| {
                    let json: String = row.get(0)?;
                    serde_json::from_str::<StoredRemoval>(&json)
                        .map(Into::into)
                        .map_err(|e| {
                            rusqlite::Error::FromSqlConversionFailure(
                                json.len(),
                                rusqlite::types::Type::Text,
                                Box::new(e),
                            )
                        })
                },
            )
            .map_err(failure)?
            .collect::<rusqlite::Result<_>>()
            .map_err(failure)
    }

    /// The latest removals, newest first.
    pub fn removals(&self, limit: u32) -> Result<Vec<Removal>> {
        let connection = self.connection();
        let mut statement = connection
            .prepare("SELECT removal FROM removals ORDER BY id DESC LIMIT ?1")
            .map_err(failure)?;
        let rows: Vec<String> = statement
            .query_map([limit], |row| row.get(0))
            .map_err(failure)?
            .collect::<rusqlite::Result<_>>()
            .map_err(failure)?;
        Ok(rows
            .into_iter()
            .filter_map(|json| serde_json::from_str::<StoredRemoval>(&json).ok())
            .map(Removal::from)
            .collect())
    }

    /// Copies removed since their calling member was last told, by member.
    pub fn unnotified(&self) -> Result<BTreeMap<String, Vec<Unnotified>>> {
        let connection = self.connection();
        let mut statement = connection
            .prepare(
                "SELECT id, removal FROM removals
                 WHERE id > coalesce(
                     (SELECT until_id FROM notified
                      WHERE calling_member = json_extract(removal, '$.calling_member')),
                     0)
                   AND json_extract(removal, '$.kind') = 'copy'
                 ORDER BY id",
            )
            .map_err(failure)?;
        let rows: Vec<(i64, String)> = statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .map_err(failure)?
            .collect::<rusqlite::Result<_>>()
            .map_err(failure)?;
        let mut pending: BTreeMap<String, Vec<Unnotified>> = BTreeMap::new();
        for (id, json) in rows {
            let Ok(stored) = serde_json::from_str::<StoredRemoval>(&json) else {
                continue;
            };
            if let Some(path) = stored.caller_path {
                pending
                    .entry(stored.calling_member)
                    .or_default()
                    .push(Unnotified {
                        id,
                        path,
                        removed_at: stored.removed_at,
                    });
            }
        }
        Ok(pending)
    }

    /// Notes that `member` was told of every removal up to the one numbered `until`.
    pub fn notified(&self, member: &str, until: i64) -> Result<()> {
        self.connection()
            .execute(
                "INSERT INTO notified (calling_member, until_id) VALUES (?1, ?2)
                 ON CONFLICT (calling_member) DO UPDATE SET
                     until_id = max(until_id, excluded.until_id)",
                params![member, until],
            )
            .map_err(failure)?;
        Ok(())
    }

    /// Notes the directory, by device and inode, about to be moved aside to `path`
    /// for deletion.
    pub fn add_trash(&self, path: &Path, (device, inode): (u64, u64)) -> Result<()> {
        self.connection()
            .execute(
                "INSERT OR REPLACE INTO trash (path, device, inode) VALUES (?1, ?2, ?3)",
                params![path.to_string_lossy(), device as i64, inode as i64],
            )
            .map_err(failure)?;
        Ok(())
    }

    pub fn trash(&self) -> Result<Vec<(PathBuf, (u64, u64))>> {
        let connection = self.connection();
        let mut statement = connection
            .prepare("SELECT path, device, inode FROM trash ORDER BY path")
            .map_err(failure)?;
        statement
            .query_map([], |row| {
                Ok((
                    path(row.get(0)?),
                    (row.get::<_, i64>(1)? as u64, row.get::<_, i64>(2)? as u64),
                ))
            })
            .map_err(failure)?
            .collect::<rusqlite::Result<_>>()
            .map_err(failure)
    }

    pub fn remove_trash(&self, path: &Path) -> Result<()> {
        self.connection()
            .execute(
                "DELETE FROM trash WHERE path = ?1",
                [path.to_string_lossy()],
            )
            .map_err(failure)?;
        Ok(())
    }
}

impl UseLog for Catalog {
    fn used(&self, location: &CopyLocation) {
        if let Err(error) = self.record_use(location, self.clock.now()) {
            crate::service_log!(
                "{}",
                serde_json::json!({"event": "copy_use_unrecorded", "detail": error})
            );
        }
    }
}

/// A removal as stored: the same fields, readable back.
#[derive(Serialize, Deserialize)]
struct StoredRemoval {
    removed_at: Timestamp,
    kind: String,
    calling_member: String,
    path: PathBuf,
    caller_path: Option<String>,
    setting: Option<Setting>,
    reason: Reason,
    size_bytes: Option<u64>,
    last_used_at: Option<Timestamp>,
}

impl From<&Removal> for StoredRemoval {
    fn from(removal: &Removal) -> Self {
        let (kind, calling_member, path, caller_path, setting) = match &removal.removed {
            Removed::Copy {
                calling_member,
                caller_path,
                location,
                setting,
            } => (
                "copy",
                calling_member.clone(),
                location.clone(),
                Some(caller_path.clone()),
                Some(*setting),
            ),
            Removed::Store {
                calling_member,
                path,
            } => ("store", calling_member.clone(), path.clone(), None, None),
        };
        Self {
            removed_at: removal.removed_at,
            kind: kind.into(),
            calling_member,
            path,
            caller_path,
            setting,
            reason: removal.reason,
            size_bytes: removal.size_bytes,
            last_used_at: removal.last_used_at,
        }
    }
}

impl From<StoredRemoval> for Removal {
    fn from(stored: StoredRemoval) -> Self {
        let removed = match (stored.kind.as_str(), stored.caller_path, stored.setting) {
            ("copy", Some(caller_path), Some(setting)) => Removed::Copy {
                calling_member: stored.calling_member,
                caller_path,
                location: stored.path,
                setting,
            },
            _ => Removed::Store {
                calling_member: stored.calling_member,
                path: stored.path,
            },
        };
        Self {
            removed_at: stored.removed_at,
            removed,
            reason: stored.reason,
            size_bytes: stored.size_bytes,
            last_used_at: stored.last_used_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::housekeeping::clock::SystemClock;

    fn location(root: &str) -> CopyLocation {
        CopyLocation {
            setting: Setting::Mapped,
            root: root.into(),
            store: "/w/.stores/alpha/1.git".into(),
            state_file: "/state/1.state".into(),
            calling_member: "alpha".into(),
            caller_root: "/Users/a/app".into(),
        }
    }

    #[test]
    fn uses_keep_the_latest_time_and_adoption_never_overrides() {
        let dir = tempfile::tempdir().unwrap();
        let catalog =
            Catalog::open(&dir.path().join("copies.sqlite"), Arc::new(SystemClock)).unwrap();
        let a = location("/w/alpha/app-1");
        catalog
            .record_use(&a, Timestamp::from_millis(2_000))
            .unwrap();
        catalog
            .record_use(&a, Timestamp::from_millis(1_000))
            .unwrap();
        assert!(!catalog.adopt(&a, Timestamp::from_millis(9_000)).unwrap());
        let entries = catalog.entries().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].location, a);
        assert_eq!(entries[0].last_used, Timestamp::from_millis(2_000));
        assert_eq!(entries[0].size, None);
        let measured = Measured {
            bytes: 4096,
            at: Timestamp::from_millis(3_000),
        };
        catalog.set_size(&a.root, measured).unwrap();
        assert_eq!(catalog.entries().unwrap()[0].size, Some(measured));
        catalog.forget(&a.root).unwrap();
        assert!(catalog.entries().unwrap().is_empty());
    }

    #[test]
    fn removals_read_back_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let catalog =
            Catalog::open(&dir.path().join("copies.sqlite"), Arc::new(SystemClock)).unwrap();
        let copy = Removal {
            removed_at: Timestamp::now(),
            removed: Removed::Copy {
                calling_member: "alpha".into(),
                caller_path: "/Users/a/app".into(),
                location: "/w/alpha/app-1".into(),
                setting: Setting::Mapped,
            },
            reason: Reason::Idle,
            size_bytes: Some(10),
            last_used_at: Some(Timestamp::from_millis(5)),
        };
        let store = Removal {
            removed_at: Timestamp::now(),
            removed: Removed::Store {
                calling_member: "alpha".into(),
                path: "/w/.stores/alpha/1.git".into(),
            },
            reason: Reason::Unreferenced,
            size_bytes: None,
            last_used_at: None,
        };
        catalog.record_removal(&copy).unwrap();
        catalog.record_removal(&store).unwrap();
        assert_eq!(catalog.removals(10).unwrap(), vec![store, copy.clone()]);
        let pending = catalog.unnotified().unwrap();
        assert_eq!(pending["alpha"].len(), 1, "only copies are told");
        assert_eq!(pending["alpha"][0].path, "/Users/a/app");
        assert_eq!(pending["alpha"][0].removed_at, copy.removed_at);
        // A later removal at the same instant is still told.
        catalog.notified("alpha", pending["alpha"][0].id).unwrap();
        assert!(catalog.unnotified().unwrap().is_empty());
        catalog.record_removal(&copy).unwrap();
        assert_eq!(catalog.unnotified().unwrap()["alpha"].len(), 1);
        assert_eq!(catalog.removals(1).unwrap().len(), 1);
        let json = serde_json::to_value(&copy).unwrap();
        assert_eq!(json["kind"], "copy");
        assert_eq!(json["reason"], "idle");
        assert_eq!(json["caller_path"], "/Users/a/app");
    }
}
