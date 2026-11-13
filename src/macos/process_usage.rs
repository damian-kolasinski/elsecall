//! CPU time a command used, including every descendant it waited for, read from
//! the exited command before it is reaped. Learned sizes come from it: CPU time
//! divided by run time is the number of cores a run kept busy on average.
use std::time::Duration;

/// Blocks until `pid`, a child of this process, has exited, leaving it unreaped so
/// its usage can still be read. Fails if it is not a child or was already reaped.
pub fn wait_exited(pid: u32) -> std::io::Result<()> {
    loop {
        // SAFETY: siginfo_t is plain old data; zero is a valid bit pattern.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: `info` is valid for the call to fill; WNOWAIT leaves the child
        // waitable, so its owner still reaps it.
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOWAIT,
            )
        };
        if result == 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

/// Whether `pid`, a child of this process, has exited, without waiting and without
/// reaping it. Fails if it is not a child or was already reaped.
pub fn has_exited(pid: u32) -> std::io::Result<bool> {
    loop {
        // SAFETY: siginfo_t is plain old data; zero is a valid bit pattern.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: as in `wait_exited`; WNOHANG returns at once, with `si_pid` zero
        // while the child still runs.
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOWAIT | libc::WNOHANG,
            )
        };
        if result == 0 {
            return Ok(info.si_pid != 0);
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

/// User and system time of `pid` and its reaped descendants. Works for a process
/// that exited but was not reaped yet; `None` once it is gone.
pub fn cpu_time(pid: u32) -> Option<Duration> {
    let usage = rusage(pid)?;
    let ticks = usage
        .ri_user_time
        .saturating_add(usage.ri_system_time)
        .saturating_add(usage.ri_child_user_time)
        .saturating_add(usage.ri_child_system_time);
    Some(Duration::from_nanos(nanoseconds(ticks)))
}

fn rusage(pid: u32) -> Option<libc::rusage_info_v2> {
    // SAFETY: rusage_info_v2 is plain old data; zero is a valid bit pattern.
    let mut usage: libc::rusage_info_v2 = unsafe { std::mem::zeroed() };
    // SAFETY: the buffer is a rusage_info_v2, which the V2 flavor fills.
    let result = unsafe {
        libc::proc_pid_rusage(
            pid as libc::c_int,
            libc::RUSAGE_INFO_V2,
            (&mut usage as *mut libc::rusage_info_v2).cast(),
        )
    };
    (result == 0).then_some(usage)
}

/// `mach_timebase_info_data_t` from mach/mach_time.h.
#[repr(C)]
struct Timebase {
    numer: u32,
    denom: u32,
}
unsafe extern "C" {
    fn mach_timebase_info(info: *mut Timebase) -> libc::c_int;
}

/// Converts Mach absolute time units, which rusage_info uses on Apple silicon, to
/// nanoseconds.
fn nanoseconds(ticks: u64) -> u64 {
    let mut base = Timebase { numer: 0, denom: 0 };
    // SAFETY: `base` is valid for the call to fill.
    if unsafe { mach_timebase_info(&mut base) } != 0 || base.denom == 0 {
        return ticks;
    }
    (u128::from(ticks) * u128::from(base.numer) / u128::from(base.denom)) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn reads_an_exited_childs_cpu_time_including_its_own_children() {
        // The child's own child does the work, so only descendant accounting sees
        // it; the trailing `true` keeps the outer shell from replacing itself.
        #[allow(clippy::zombie_processes, reason = "reaped with wait4 below")]
        let mut child = Command::new("/bin/sh")
            .args([
                "-c",
                "read go; /bin/sh -c 'i=0; while [ $i -lt 60000 ]; do i=$((i+1)); done'; true",
            ])
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let pid = child.id();
        assert!(!has_exited(pid).unwrap(), "blocked until released");
        use std::io::Write;
        child.stdin.take().unwrap().write_all(b"go\n").unwrap();
        wait_exited(pid).unwrap();
        assert!(has_exited(pid).unwrap(), "exited, not reaped");
        let measured = cpu_time(pid).expect("an unreaped child is readable");
        // Reap it with wait4, whose rusage is the reference for the same process.
        let mut status = 0;
        // SAFETY: rusage is plain old data; zero is a valid bit pattern.
        let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
        // SAFETY: valid pointers for the call to fill.
        assert_eq!(
            unsafe { libc::wait4(pid as i32, &mut status, 0, &mut usage) },
            pid as i32
        );
        let tv = |t: libc::timeval| Duration::new(t.tv_sec as u64, t.tv_usec as u32 * 1000);
        let reference = tv(usage.ru_utime) + tv(usage.ru_stime);
        assert!(reference > Duration::from_millis(20), "{reference:?}");
        let difference = measured.abs_diff(reference);
        assert!(
            difference < Duration::from_millis(10) + reference / 20,
            "{measured:?} vs {reference:?}"
        );
        assert_eq!(cpu_time(pid), None, "a reaped process is gone");
    }
}
