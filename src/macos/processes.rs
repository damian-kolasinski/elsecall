//! Process identity through libproc and exit notification through kqueue.
use crate::{
    failure::{Failure, Result},
    scheduling::processes::{ProcessIdentity, ProcessInfo, ProcessTable},
};
use std::{
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    sync::Arc,
};
use tokio::{
    io::{Interest, unix::AsyncFd},
    sync::mpsc,
};

/// `SZOMB` from sys/proc.h: exited but not yet reaped by its parent.
const ZOMBIE: u32 = 5;

pub struct MacProcesses {
    queue: Arc<OwnedFd>,
}

impl MacProcesses {
    /// Starts reporting watched exits on the returned channel. Needs a Tokio runtime.
    pub fn start() -> Result<(Arc<Self>, mpsc::UnboundedReceiver<u32>)> {
        // SAFETY: kqueue has no preconditions; a nonnegative result is a new fd.
        let fd = unsafe { libc::kqueue() };
        if fd < 0 {
            return Err(Failure::io(
                "Create process watcher",
                std::io::Error::last_os_error(),
            ));
        }
        // SAFETY: fd is a newly created descriptor that nothing else owns.
        let queue = Arc::new(unsafe { OwnedFd::from_raw_fd(fd) });
        // SAFETY: fd is valid; FD_CLOEXEC keeps it out of spawned commands.
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
        // SAFETY: the Arc keeps the descriptor open and unchanged for the AsyncFd's
        // lifetime. A kqueue supports read readiness only.
        let readiness =
            unsafe { AsyncFd::register_with_interest(queue.clone(), Interest::READABLE) }
                .map_err(|e| Failure::io("Register process watcher", e.into_parts().1))?;
        let (sender, receiver) = mpsc::unbounded_channel();
        tokio::spawn(report_exits(readiness, sender));
        Ok((Arc::new(Self { queue }), receiver))
    }
}

async fn report_exits(readiness: AsyncFd<Arc<OwnedFd>>, exits: mpsc::UnboundedSender<u32>) {
    loop {
        let Ok(mut guard) = readiness.readable().await else {
            return;
        };
        // SAFETY: kevent is plain old data; zero is a valid bit pattern.
        let mut events: [libc::kevent; 32] = unsafe { std::mem::zeroed() };
        let poll = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: the event buffer and timeout are valid for the call; no changes
        // are submitted.
        let count = unsafe {
            libc::kevent(
                readiness.get_ref().as_raw_fd(),
                std::ptr::null(),
                0,
                events.as_mut_ptr(),
                events.len() as libc::c_int,
                &poll,
            )
        };
        if count < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        if count <= 0 {
            guard.clear_ready();
            continue;
        }
        for event in &events[..count as usize] {
            if event.filter == libc::EVFILT_PROC
                && event.fflags & libc::NOTE_EXIT != 0
                && exits.send(event.ident as u32).is_err()
            {
                return;
            }
        }
    }
}

/// A running process of this machine; `None` once it exited or when it cannot be read.
pub fn describe(pid: u32) -> Option<ProcessInfo> {
    // SAFETY: proc_bsdinfo is plain old data; zero is a valid bit pattern.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: the buffer is valid and writable for `size` bytes.
    let read = unsafe {
        libc::proc_pidinfo(
            libc::c_int::try_from(pid).ok()?,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    (read == size && info.pbi_status != ZOMBIE).then(|| ProcessInfo {
        identity: ProcessIdentity {
            pid,
            started_us: info
                .pbi_start_tvsec
                .saturating_mul(1_000_000)
                .saturating_add(info.pbi_start_tvusec),
        },
        parent: info.pbi_ppid,
    })
}

impl ProcessTable for MacProcesses {
    fn describe(&self, pid: u32) -> Option<ProcessInfo> {
        describe(pid)
    }

    fn watch(&self, process: ProcessIdentity) -> bool {
        let change = libc::kevent {
            ident: process.pid as usize,
            filter: libc::EVFILT_PROC,
            flags: libc::EV_ADD,
            fflags: libc::NOTE_EXIT,
            data: 0,
            udata: std::ptr::null_mut(),
        };
        // SAFETY: one valid change is submitted and no events are read.
        let added = unsafe {
            libc::kevent(
                self.queue.as_raw_fd(),
                &change,
                1,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
            )
        } == 0;
        // Checked after registering, so an exit in between is never missed: either
        // the process is still the same one and its exit will be reported, or it
        // is gone and the caller learns so now.
        added
            && self
                .describe(process.pid)
                .is_some_and(|info| info.identity == process)
    }

    fn kill_group(&self, process: ProcessIdentity, group: u32) -> bool {
        // Only the same process, never one that reused its PID, and only while it
        // is still in `group`: a PID is never reused while its group exists, so
        // the group is still the one `process` was seen in.
        let member = self.describe(process.pid).is_some_and(|info| info.identity == process)
            // SAFETY: getpgid has no memory-safety preconditions.
            && unsafe { libc::getpgid(process.pid as i32) } == group as i32;
        if member && group > 1 {
            // SAFETY: kill has no memory-safety preconditions.
            unsafe { libc::kill(-(group as i32), libc::SIGKILL) };
        }
        member
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduling::processes::lineage;
    use std::process::Command;

    #[tokio::test]
    async fn identifies_lineage_and_reports_exits_including_sigkill() {
        let (table, mut exits) = MacProcesses::start().unwrap();
        let me = table.describe(std::process::id()).unwrap();
        // SAFETY: getppid has no preconditions.
        assert_eq!(me.parent, unsafe { libc::getppid() } as u32);
        assert_eq!(lineage(&*table, me.identity.pid)[0], me);

        let mut child = Command::new("/bin/sleep").arg("30").spawn().unwrap();
        let identity = table.describe(child.id()).unwrap().identity;
        assert_eq!(table.describe(child.id()).unwrap().parent, me.identity.pid);
        assert!(table.watch(identity));
        child.kill().unwrap();
        let exited = tokio::time::timeout(std::time::Duration::from_secs(120), exits.recv())
            .await
            .unwrap();
        assert_eq!(exited, Some(identity.pid));
        assert!(table.describe(child.id()).is_none(), "zombies are gone");
        child.wait().unwrap();
        assert!(
            !table.watch(identity),
            "an exited process cannot be watched"
        );
    }
}
