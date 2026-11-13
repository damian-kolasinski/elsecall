//! The processes of one process group: who is in it, what CPU time each has used,
//! and signals to all of them. A placed command leads its own group, so what it
//! leaves running after it exits is found here.
use crate::{
    macos::{process_usage, processes},
    scheduling::processes::ProcessIdentity,
};
use std::time::Duration;

/// A live process of the group.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Member {
    pub identity: ProcessIdentity,
    pub parent: u32,
    /// Its user and system time, with that of the children it has reaped.
    pub cpu: Duration,
}

/// The live processes of group `group`, leaving out those that exited but were
/// not reaped yet. An unreadable process table reads as empty.
pub fn members(group: u32) -> Vec<Member> {
    let Ok(group) = libc::pid_t::try_from(group) else {
        return Vec::new();
    };
    let mut capacity = 256usize;
    loop {
        let mut pids = vec![0 as libc::pid_t; capacity];
        let bytes = (capacity * std::mem::size_of::<libc::pid_t>()) as libc::c_int;
        // SAFETY: the buffer is valid and writable for `bytes` bytes.
        let count = unsafe { libc::proc_listpgrppids(group, pids.as_mut_ptr().cast(), bytes) };
        if count < 0 {
            return Vec::new();
        }
        let count = count as usize;
        // A full buffer may have cut the list short.
        if count >= capacity && capacity < 65_536 {
            capacity *= 4;
            continue;
        }
        return pids[..count.min(capacity)]
            .iter()
            .filter(|pid| **pid > 0)
            .filter_map(|pid| {
                let pid = *pid as u32;
                let info = processes::describe(pid)?;
                // Read after its identity, so a process that ended in between, and
                // a PID reused since, are never counted.
                let cpu = process_usage::cpu_time(pid)?;
                (group_of(pid) == Some(group as u32)).then_some(Member {
                    identity: info.identity,
                    parent: info.parent,
                    cpu,
                })
            })
            .collect();
    }
}

fn group_of(pid: u32) -> Option<u32> {
    // SAFETY: getpgid has no memory-safety preconditions.
    let group = unsafe { libc::getpgid(pid as libc::pid_t) };
    (group >= 0).then_some(group as u32)
}

/// Sends `signal` to every process of group `group`.
pub fn signal(group: u32, signal: i32) {
    if let Ok(group) = libc::pid_t::try_from(group)
        && group > 1
    {
        // SAFETY: kill has no memory-safety preconditions; a negative PID names
        // the group.
        unsafe { libc::kill(-group, signal) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;

    #[test]
    fn lists_a_groups_processes_after_its_leader_exited_and_signals_them() {
        // The leader starts a busy and an idle process and exits at once.
        let mut leader = std::process::Command::new("/bin/sh")
            .args([
                "-c",
                "(i=0; while [ $i -lt 40000 ]; do i=$((i+1)); done; read finish) <&0 & (read finish) <&0 & exit 0",
            ])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();
        let group = leader.id();
        let _input = leader.stdin.take().unwrap();
        leader.wait().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(120);
        let busy = loop {
            let members = members(group);
            assert_eq!(members.len(), 2, "{members:?}");
            if let Some(busy) = members.iter().find(|m| m.cpu > Duration::from_millis(20)) {
                break *busy;
            }
            assert!(std::time::Instant::now() < deadline, "{members:?}");
            std::thread::sleep(Duration::from_millis(50));
        };
        assert!(busy.identity.pid != group, "the leader is gone");
        signal(group, libc::SIGKILL);
        let deadline = std::time::Instant::now() + Duration::from_secs(120);
        while !members(group).is_empty() {
            assert!(
                std::time::Instant::now() < deadline,
                "the group did not end"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(members(u32::MAX).is_empty());
    }
}
