//! The wall clock housekeeping judges idleness by: a port, so tests can stand at
//! the boundary of an idle period without waiting days.
use crate::scheduling::timestamp::Timestamp;
use std::{path::PathBuf, sync::Arc};

pub trait Clock: Send + Sync {
    fn now(&self) -> Timestamp;
}

pub struct SystemClock;
impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        Timestamp::now()
    }
}

/// A clock read from a file holding an RFC 3339 time, for services that tests
/// start; the system clock while the file is absent or unreadable.
pub struct FileClock(pub PathBuf);
impl Clock for FileClock {
    fn now(&self) -> Timestamp {
        std::fs::read_to_string(&self.0)
            .ok()
            .and_then(|text| Timestamp::parse_rfc3339(text.trim()))
            .unwrap_or_else(Timestamp::now)
    }
}

/// The system clock, unless `ELSECALL_HOUSEKEEPING_CLOCK` names a clock file.
pub fn from_environment() -> Arc<dyn Clock> {
    match std::env::var_os("ELSECALL_HOUSEKEEPING_CLOCK") {
        Some(path) if !path.is_empty() => {
            crate::service_log!(
                "{}",
                serde_json::json!({"event": "test_seam_in_use", "detail": "ELSECALL_HOUSEKEEPING_CLOCK sets the clock housekeeping judges idleness by"})
            );
            Arc::new(FileClock(path.into()))
        }
        _ => Arc::new(SystemClock),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clock_file_sets_the_time_and_its_absence_falls_back() {
        let dir = tempfile::tempdir().unwrap();
        let clock = FileClock(dir.path().join("clock"));
        let before = Timestamp::now();
        assert!(clock.now() >= before);
        std::fs::write(dir.path().join("clock"), "2030-01-02T03:04:05Z\n").unwrap();
        assert_eq!(
            clock.now(),
            Timestamp::parse_rfc3339("2030-01-02T03:04:05Z").unwrap()
        );
    }
}
