//! A step's commands, run as its admitted run's holders.
//!
//! Each command starts in a process group of its own, with no input and its
//! output discarded: nothing it prints is kept, so a token in a download URL
//! never reaches a record or a log. Its process is bound to the run, as a
//! placed command's is, so a lease it takes nests inside the run. Past its
//! limit, or when the step is dropped while it runs, the whole group is killed.
//! A command that exits by itself leaves its group alone: once its leader is
//! reaped the group's ID may be reused by an unrelated group.
use crate::install::{InstallAdmission, Ran, Work};
use elsecall_contract::scheduling_model::run_id::RunId;
use futures_util::future::BoxFuture;
use std::{process::Stdio, sync::Arc, time::Duration};

pub(crate) struct AdmittedWork {
    pub(crate) admission: Arc<dyn InstallAdmission>,
    pub(crate) run: RunId,
}

impl Work for AdmittedWork {
    fn run(&self, mut command: tokio::process::Command, limit: Duration) -> BoxFuture<'_, Ran> {
        Box::pin(async move {
            command
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .process_group(0)
                .kill_on_drop(true);
            let mut child = match elsecall_host::child_start::spawn_async(&mut command) {
                Ok(child) => child,
                Err(_) => return Ran::NotStarted,
            };
            let group = child.id();
            if let Some(pid) = group {
                self.admission.started(&self.run, pid);
            }
            let mut guard = GroupGuard(group);
            match tokio::time::timeout(limit, child.wait()).await {
                Ok(Ok(status)) => {
                    guard.0 = None;
                    Ran::Ended(status)
                }
                Ok(Err(_)) => Ran::NotStarted,
                Err(_) => Ran::TimedOut,
            }
        })
    }
}

/// Kills a command's process group when it times out or the step is dropped
/// while it runs.
struct GroupGuard(Option<u32>);

impl Drop for GroupGuard {
    fn drop(&mut self) {
        if let Some(group) = self.0.and_then(|pid| libc::pid_t::try_from(pid).ok()) {
            // SAFETY: killpg only sends a signal; the group is the command's own.
            unsafe {
                libc::killpg(group, libc::SIGKILL);
            }
        }
    }
}
