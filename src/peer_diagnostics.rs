//! Bounded private peer-attempt history. No endpoints, request bodies, signatures,
//! paths or server error text are retained; a verified refusal keeps only its
//! stable code. A failed retry remains observable.
use crate::{
    failure::{Failure, Result},
    scheduling::timestamp::Timestamp,
};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use std::{
    path::Path,
    sync::{Mutex, OnceLock},
    time::Instant,
};
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Connect,
    Authentication,
    Send,
    Receive,
    Request,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Connect,
    Authentication,
    Send,
    Receive,
    Timeout,
    Protocol,
    Remote,
}
/// The stable code a member refused a request with in its verified answer, such
/// as `member_upgrading`. Its message and hint are never kept.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct RefusalCode(String);
impl RefusalCode {
    /// A failure code as Elsecall writes them. Anything else is no code at all, so
    /// free text never enters history.
    pub fn parse(code: &str) -> Option<Self> {
        crate::reporting::facts::is_code(code).then(|| Self(code.to_owned()))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
/// Other members' history is untrusted input: an invalid code is dropped rather
/// than failing the whole history it arrived in.
fn refusal_code<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<RefusalCode>, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(value.as_str().and_then(RefusalCode::parse))
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Attempt {
    pub at: Timestamp,
    pub member: String,
    pub operation: String,
    pub attempt: usize,
    pub phase: Phase,
    pub failure_kind: Option<Kind>,
    pub duration_ms: u64,
    /// Set when the member answered and refused; older builds omit and ignore it.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "refusal_code"
    )]
    pub refusal: Option<RefusalCode>,
}
impl Attempt {
    pub fn new(
        member: &str,
        path: &str,
        attempt: usize,
        phase: Phase,
        kind: Option<Kind>,
        started: Instant,
    ) -> Self {
        Self {
            at: Timestamp::now(),
            member: member.into(),
            operation: operation(path).into(),
            attempt,
            phase,
            failure_kind: kind,
            duration_ms: started.elapsed().as_millis() as u64,
            refusal: None,
        }
    }
    /// The member answered with a verified refusal. Every I/O phase completed, so
    /// the attempt failed as a whole request, with the member's code.
    pub fn refused(member: &str, path: &str, attempt: usize, code: &str, started: Instant) -> Self {
        Self {
            refusal: RefusalCode::parse(code),
            ..Self::new(
                member,
                path,
                attempt,
                Phase::Request,
                Some(Kind::Remote),
                started,
            )
        }
    }
}
fn operation(path: &str) -> &'static str {
    if path == "/peer/v1/status" {
        "status"
    } else if path == "/peer/v1/placements/quote" {
        "quote"
    } else if path.starts_with("/peer/v1/config-sync") {
        "sync"
    } else if path.contains("/attach") {
        "result_stream"
    } else if path.starts_with("/peer/v1/sync") {
        "sync"
    } else if path == "/peer/v1/placements" {
        "offer"
    } else if path == "/peer/v1/rehearsal" {
        "rehearsal"
    } else if path.starts_with("/peer/v1/report") || path.starts_with("/peer/v1/trace") {
        "history"
    } else {
        "member_request"
    }
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct History {
    pub attempts: Vec<Attempt>,
    pub truncated: bool,
    pub coverage_since: Option<Timestamp>,
    pub error: Option<String>,
}
pub struct Store(Mutex<Connection>);
impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
            .map_err(|e| Failure::io("Open peer diagnostics", e))?;
        let meta = file
            .metadata()
            .map_err(|e| Failure::io("Inspect peer diagnostics", e))?;
        if !meta.is_file() || meta.uid() != crate::local_access::user_id() || meta.nlink() != 1 {
            return Err(Failure::new(
                "peer_history_unavailable",
                "Peer diagnostics must be an owner-owned regular file without hard links.",
                "Check the Elsecall state directory.",
            ));
        }
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|e| Failure::io("Protect peer diagnostics", e))?;
        let db = Connection::open(path).map_err(db_error)?;
        db.busy_timeout(std::time::Duration::from_millis(100))
            .map_err(db_error)?;
        db.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE IF NOT EXISTS coverage (id INTEGER PRIMARY KEY, since INTEGER NOT NULL); CREATE TABLE IF NOT EXISTS attempts (id INTEGER PRIMARY KEY, at INTEGER NOT NULL, record TEXT NOT NULL); CREATE INDEX IF NOT EXISTS attempts_at ON attempts(at);").map_err(db_error)?;
        db.execute(
            "INSERT OR IGNORE INTO coverage(id,since) VALUES(1,?1)",
            [Timestamp::now().millis() as i64],
        )
        .map_err(db_error)?;
        Ok(Self(Mutex::new(db)))
    }
    pub fn record(&self, attempt: &Attempt) -> Result<()> {
        let db = self.0.lock().unwrap_or_else(|e| e.into_inner());
        db.execute(
            "INSERT INTO attempts(at,record) VALUES(?1,?2)",
            params![
                attempt.at.millis() as i64,
                serde_json::to_string(attempt).expect("attempt serializes")
            ],
        )
        .map_err(db_error)?;
        db.execute("DELETE FROM attempts WHERE at < ?1 OR id <= (SELECT COALESCE(MAX(id),0)-10000 FROM attempts)", [attempt.at.millis().saturating_sub(30*86400000) as i64]).map_err(db_error)?;
        Ok(())
    }
    pub fn between(&self, since: Timestamp, until: Timestamp) -> Result<History> {
        let db = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let mut q = db.prepare("SELECT record FROM attempts WHERE at >= ?1 AND at <= ?2 ORDER BY id DESC LIMIT 1001").map_err(db_error)?;
        let mut attempts: Vec<Attempt> = q
            .query_map(params![since.millis() as i64, until.millis() as i64], |r| {
                r.get::<_, String>(0)
            })
            .map_err(db_error)?
            .map(|row| {
                serde_json::from_str(&row.map_err(db_error)?).map_err(|_| {
                    Failure::new(
                        "peer_history_invalid",
                        "Peer attempt history is invalid.",
                        "Run elsecall doctor.",
                    )
                })
            })
            .collect::<Result<_>>()?;
        let count: i64 = db
            .query_row("SELECT COUNT(*) FROM attempts", [], |r| r.get(0))
            .map_err(db_error)?;
        let began: i64 = db
            .query_row("SELECT since FROM coverage WHERE id=1", [], |r| r.get(0))
            .map_err(db_error)?;
        let oldest: i64 = db
            .query_row("SELECT COALESCE(MIN(at),0) FROM attempts", [], |r| r.get(0))
            .map_err(db_error)?;
        let coverage_since = Timestamp::from_millis(
            (began as u64)
                .max(Timestamp::now().millis().saturating_sub(30 * 86400000))
                .max(if count >= 10000 { oldest as u64 } else { 0 }),
        );
        let truncated = attempts.len() > 1000;
        attempts.truncate(1000);
        Ok(History {
            attempts,
            truncated,
            coverage_since: Some(coverage_since),
            error: None,
        })
    }
}
fn db_error(_: rusqlite::Error) -> Failure {
    Failure::new(
        "peer_history_unavailable",
        "Peer attempt history could not be read or written.",
        "Run elsecall doctor and check state directory permissions.",
    )
}
static STORE: OnceLock<Store> = OnceLock::new();
pub fn initialize(paths: &crate::paths::Paths) -> Result<()> {
    let store = Store::open(&paths.data.join("peer-io.sqlite"))?;
    let _ = STORE.set(store);
    Ok(())
}
pub fn record(attempt: &Attempt) {
    if attempt.failure_kind.is_none() {
        return;
    }
    if let Some(store) = STORE.get()
        && let Err(error) = store.record(attempt)
    {
        crate::service_log!(
            "{}",
            serde_json::json!({"event":"peer_history_failed","code":error.code})
        );
    }
    if attempt.failure_kind.is_some() {
        crate::service_log!(
            "{}",
            serde_json::json!({"event":"peer_request_failed","peer_io":attempt})
        );
    }
}
pub fn between(since: Timestamp, until: Timestamp) -> History {
    match STORE.get().map(|s| s.between(since, until)) {
        Some(Ok(history)) => history,
        Some(Err(error)) => History {
            error: Some(error.code),
            ..Default::default()
        },
        None => History {
            error: Some("peer_history_unavailable".into()),
            ..Default::default()
        },
    }
}
pub fn recent() -> History {
    between(
        Timestamp::from_millis(Timestamp::now().millis().saturating_sub(3600000)),
        Timestamp::now(),
    )
}
/// An outer whole-operation bound, rather than a timeout in a known socket phase.
pub fn operation_timeout(member: &crate::roster::Member, path: &str, started: Instant) -> Failure {
    let attempt = Attempt::new(
        member.name.as_str(),
        path,
        1,
        Phase::Request,
        Some(Kind::Timeout),
        started,
    );
    record(&attempt);
    crate::peer_client::unavailable(member).with_details(serde_json::json!({"peer_io":attempt}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    #[test]
    fn failure_kinds_and_durations_survive_reopen_in_private_bounded_history() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("peer.sqlite");
        let store = Store::open(&path).unwrap();
        let since = Timestamp::now();
        for kind in [
            Kind::Connect,
            Kind::Authentication,
            Kind::Send,
            Kind::Receive,
            Kind::Timeout,
            Kind::Protocol,
            Kind::Remote,
        ] {
            let attempt = Attempt::new(
                "peer",
                "/peer/v1/status",
                1,
                Phase::Receive,
                Some(kind),
                Instant::now() - std::time::Duration::from_millis(25),
            );
            store.record(&attempt).unwrap();
        }
        drop(store);
        let store = Store::open(&path).unwrap();
        let history = store.between(since, Timestamp::now()).unwrap();
        assert_eq!(history.attempts.len(), 7);
        assert!(history.attempts.iter().all(|a| a.duration_ms >= 25));
        assert!(history.error.is_none());
        assert!(!history.truncated);
        assert!(history.coverage_since.is_some());
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        // An oversized query is explicitly partial, rather than silently empty.
        for _ in 0..1000 {
            store.record(&history.attempts[0]).unwrap();
        }
        let history = store.between(since, Timestamp::now()).unwrap();
        assert_eq!(history.attempts.len(), 1000);
        assert!(history.truncated);
    }
    #[test]
    fn refusal_codes_cross_builds_without_breaking_history() {
        let record = |refusal: serde_json::Value| {
            let mut value = serde_json::json!({"at":"2026-11-09T15:37:15.599Z","member":"peer","operation":"quote","attempt":1,"phase":"request","failure_kind":"remote","duration_ms":10});
            if !refusal.is_null() {
                value["refusal"] = refusal;
            }
            serde_json::from_value::<Attempt>(value).unwrap()
        };
        // Records from builds before refusal codes still read, and write it back out
        // in the shape those builds expect.
        let old = record(serde_json::Value::Null);
        assert_eq!(old.refusal, None);
        assert!(serde_json::to_value(&old).unwrap().get("refusal").is_none());
        assert_eq!(
            record("member_upgrading".into()).refusal,
            RefusalCode::parse("member_upgrading")
        );
        // Another member's malformed code is dropped, never the whole history.
        for invalid in [
            serde_json::json!("Denied: see /Users/x"),
            serde_json::json!(""),
            serde_json::json!("a".repeat(65)),
            serde_json::json!(7),
        ] {
            assert_eq!(record(invalid).refusal, None);
        }
    }
    #[test]
    fn unsafe_history_file_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        let original = temp.path().join("original");
        let linked = temp.path().join("linked");
        std::fs::write(&original, b"private original").unwrap();
        std::fs::hard_link(&original, &linked).unwrap();
        assert!(Store::open(&linked).is_err());
        assert_eq!(std::fs::read(original).unwrap(), b"private original");
    }
}
