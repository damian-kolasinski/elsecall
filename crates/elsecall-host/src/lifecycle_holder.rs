//! Which process is replacing the service: the one holding the installation lock,
//! recorded in the lock file while it holds it. An install, release update, fleet
//! upgrade, restart or uninstall holds that lock from before it stops the service
//! until the new one is confirmed, so a CLI request that finds no service can tell
//! a handover from a service that is simply not running, without taking the lock
//! itself and so without ever making such an operation fail.
//!
//! A process is recorded by its ID and start time, so a recorded holder that died
//! without clearing the record (killed, or a crash) is never mistaken for a later
//! process that reuses its ID.
use crate::os_processes::processes;
use crate::processes::ProcessIdentity;
use std::{
    fs::File,
    io::{Seek, Write},
    path::Path,
};

/// Records this process as the holder of `lock`, which it has just locked. The
/// record is advisory: a failed write only means a handover is not recognized.
pub fn record(lock: &mut File) {
    let Some(this) = processes::describe(std::process::id()) else {
        return;
    };
    let _ = lock
        .set_len(0)
        .and_then(|()| lock.rewind())
        .and_then(|()| write!(lock, "{} {}", this.identity.pid, this.identity.started_us));
}

/// Clears the record before `lock` is released.
pub fn clear(lock: &File) {
    let _ = lock.set_len(0);
}

/// The lock every lifecycle operation holds, in the state directory.
pub const INSTALLATION_LOCK: &str = "installation.lock";

/// Whether an install, update, upgrade, restart or uninstall is replacing the
/// service in the state directory `data`: from before it stops the service until it
/// has confirmed the new one. Asking never takes the lock, so it cannot make such
/// an operation fail.
pub fn operation_running(data: &Path) -> bool {
    running(&data.join(INSTALLATION_LOCK))
}

/// Whether the holder recorded in the lock file at `path` still runs, other than
/// this process.
fn running(path: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    let Some(recorded) = parse(&text) else {
        return false;
    };
    recorded.pid != std::process::id()
        && processes::describe(recorded.pid).is_some_and(|now| now.identity == recorded)
}

fn parse(text: &str) -> Option<ProcessIdentity> {
    let (pid, started_us) = text.trim().split_once(' ')?;
    Some(ProcessIdentity {
        pid: pid.parse().ok()?,
        started_us: started_us.parse().ok()?,
    })
}

#[cfg(test)]
mod tests {
    use crate::lifecycle_holder::*;

    #[test]
    fn only_a_recorded_holder_that_still_runs_counts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("installation.lock");
        assert!(!running(&path), "no lock file");
        std::fs::write(&path, "").unwrap();
        assert!(!running(&path), "cleared");

        let mut child = std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let identity = processes::describe(child.id()).unwrap().identity;
        std::fs::write(&path, format!("{} {}", identity.pid, identity.started_us)).unwrap();
        assert!(running(&path));
        std::fs::write(
            &path,
            format!("{} {}", identity.pid, identity.started_us + 1),
        )
        .unwrap();
        assert!(!running(&path), "another process with the same ID");
        std::fs::write(&path, identity.pid.to_string()).unwrap();
        assert!(!running(&path), "an unrecognized record");
        std::fs::write(&path, format!("{} {}", identity.pid, identity.started_us)).unwrap();
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(!running(&path), "the holder ended without clearing it");
    }

    #[test]
    fn the_holder_records_itself_and_clears_the_record_and_is_not_running_to_itself() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("installation.lock");
        std::fs::write(&path, "a longer earlier record that must not survive").unwrap();
        let mut lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        record(&mut lock);
        let recorded = parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(recorded.pid, std::process::id());
        assert!(!running(&path), "a process does not wait for itself");
        clear(&lock);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "");
    }
}
