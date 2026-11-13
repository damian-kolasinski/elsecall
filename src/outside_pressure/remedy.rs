//! What the operator can do about a blocked member, as one line an agent can act
//! on. Elsecall never stops the process itself: it is not Elsecall's work, and
//! a root daemon needs privileges the service does not have.
use crate::{
    inspection::MemoryPressure,
    outside_pressure::{
        blocked::{Blocked, OutsideProcess, Reason},
        sampling::Owner,
        warning::{Concern, Warning},
    },
    scheduling::timestamp::Timestamp,
};

/// Processes whose restart ends the user's session or the machine's: the
/// remedy for them is a reboot, never a kill.
const SESSION_CRITICAL: [&str; 4] = ["WindowServer", "loginwindow", "kernel_task", "launchd"];
use std::time::Duration;

/// The next step for `member`, blocked as `blocked`, at `now`.
pub fn next_step(member: &str, blocked: &Blocked, now: Timestamp) -> String {
    let cause = match &blocked.reason {
        Reason::MemoryPressure { level } => format!(
            "memory pressure ({}) from outside Elsecall",
            match level {
                MemoryPressure::Normal => "normal",
                MemoryPressure::Warning => "warning",
                MemoryPressure::Critical => "critical",
            }
        ),
        Reason::OutsideSaturation {
            outside_utilization,
        } => format!(
            "outside load holding {:.0}% of its CPUs",
            outside_utilization * 100.0
        ),
    };
    let held = format!(
        "{member}'s admission gate has been closed for {}: {cause}.",
        duration(now.since(blocked.since))
    );
    match blocked.leading() {
        None => format!(
            "{held} No single outside process could be read; check Activity Monitor on {member}. Elsecall never stops outside processes."
        ),
        Some(leading) => format!(
            "{held} It is led by {}. Elsecall never stops outside processes: {} Its blocked.outside lists the others; the block clears a minute after the gate reopens.",
            describe(leading),
            restart(member, &leading.name, leading.pid, leading.owner)
        ),
    }
}

fn describe(process: &OutsideProcess) -> String {
    let memory = match process.memory.footprint_bytes {
        Some(bytes) => format!("{} footprint", bytes_text(bytes)),
        None => format!("{} resident", bytes_text(process.memory.resident_bytes)),
    };
    format!(
        "{} (pid {}, {}, {memory}, {:.1} cores, up {})",
        process.name,
        process.pid,
        process.owner.spoken(),
        process.cpu_cores,
        duration(Duration::from_secs(process.uptime_seconds))
    )
}

/// What the operator does about an outside process when its PID may no longer
/// be current, as in a report of a past block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Act {
    /// A root daemon: launchd restarts the system daemons it manages.
    Restart,
    /// The service user's own process.
    Quit,
    /// Another user's process.
    AskItsUserToQuit,
    /// Restarting it ends the session: close the heaviest apps, or reboot.
    CloseAppsOrReboot,
}

pub fn act_on(process: &OutsideProcess) -> Act {
    act(&process.name, process.owner)
}

/// What the operator does about the process `name` run by `owner`.
pub fn act(name: &str, owner: Owner) -> Act {
    if SESSION_CRITICAL.contains(&name) {
        return Act::CloseAppsOrReboot;
    }
    match owner {
        Owner::Root => Act::Restart,
        Owner::OtherUser => Act::AskItsUserToQuit,
        Owner::ThisUser => Act::Quit,
    }
}

/// What the operator can do about an early warning on `member` at `now`. The
/// member still admits work: the warning says what will close its gate if
/// nothing changes.
pub fn warning_step(member: &str, warning: &Warning, now: Timestamp) -> String {
    let process = format!(
        "{} (pid {}, {})",
        warning.process,
        warning.pid,
        warning.owner.spoken()
    );
    let held = duration(now.since(warning.since));
    let what = match warning.concern {
        Concern::Spinning { cpu_cores } => format!(
            "{process} on {member} has kept about {cpu_cores:.1} cores busy for {held} without a break, so Elsecall admits that much less work there"
        ),
        Concern::Growing {
            memory_bytes,
            growth_bytes_per_hour,
        } => format!(
            "{process} on {member} has grown to {} of memory, by {} an hour for {held}; if it keeps growing, memory pressure will close {member}'s admission gate",
            bytes_text(memory_bytes),
            bytes_text(growth_bytes_per_hour)
        ),
        Concern::Oversized {
            memory_bytes,
            share_of_memory,
        } => format!(
            "{process} on {member} has held {} of memory, {:.0}% of the machine's, for {held}; a little more and memory pressure closes {member}'s admission gate",
            bytes_text(memory_bytes),
            share_of_memory * 100.0
        ),
    };
    format!(
        "{what}. {member} still admits work. Elsecall never stops outside processes: {}",
        restart(member, &warning.process, warning.pid, warning.owner)
    )
}

fn restart(member: &str, name: &str, pid: u32, owner: Owner) -> String {
    if SESSION_CRITICAL.contains(&name) {
        return format!(
            "restarting it ends the session, so close the apps that use the most memory or CPU on {member}, or reboot it."
        );
    }
    // The PID was read minutes ago at most: check it still names the same process.
    let check =
        format!("after checking that `ps -c -o comm= -p {pid}` on {member} still prints {name}");
    match owner {
        Owner::Root => format!(
            "restart it, {check}, with `sudo kill {pid}` (launchd restarts the system daemons it manages), or reboot {member}."
        ),
        Owner::OtherUser => format!("ask its user to quit it, or, {check}, run `sudo kill {pid}`."),
        Owner::ThisUser => {
            format!("quit it, or, {check}, run `kill {pid}` if it does not respond.")
        }
    }
}

fn duration(duration: Duration) -> String {
    let minutes = duration.as_secs() / 60;
    match (minutes / (24 * 60), minutes / 60 % 24, minutes % 60) {
        (0, 0, m) => format!("{m} min"),
        (0, h, 0) => format!("{h} h"),
        (0, h, m) => format!("{h} h {m} min"),
        (d, h, _) => format!("{d} d {h} h"),
    }
}

/// `bytes` as GB to a tenth, or as whole MB under one GB.
pub fn bytes_text(bytes: u64) -> String {
    let gib = bytes as f64 / f64::from(1u32 << 30);
    if gib >= 1.0 {
        format!("{gib:.1} GB")
    } else {
        format!("{:.0} MB", bytes as f64 / f64::from(1u32 << 20))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outside_pressure::sampling::Memory;

    fn blocked(owner: Owner, outside: bool) -> Blocked {
        Blocked {
            reason: Reason::MemoryPressure {
                level: MemoryPressure::Warning,
            },
            since: Timestamp::parse_rfc3339("2026-11-10T04:48:00Z").unwrap(),
            attributed_at: Timestamp::parse_rfc3339("2026-11-10T13:09:00Z").unwrap(),
            outside: if outside {
                vec![OutsideProcess {
                    name: "fseventsd".into(),
                    pid: 412,
                    owner,
                    memory: Memory {
                        resident_bytes: 27 << 30,
                        footprint_bytes: None,
                    },
                    cpu_cores: 1.0,
                    uptime_seconds: 62 * 86_400 + 3 * 3600,
                }]
            } else {
                vec![]
            },
        }
    }

    #[test]
    fn names_the_leading_process_and_who_can_restart_it() {
        let now = Timestamp::parse_rfc3339("2026-11-10T13:10:30Z").unwrap();
        assert_eq!(
            next_step("macbook-pro", &blocked(Owner::Root, true), now),
            "macbook-pro's admission gate has been closed for 8 h 22 min: memory pressure (warning) from outside Elsecall. \
             It is led by fseventsd (pid 412, root, 27.0 GB resident, 1.0 cores, up 62 d 3 h). \
             Elsecall never stops outside processes: restart it, after checking that `ps -c -o comm= -p 412` on macbook-pro \
             still prints fseventsd, with `sudo kill 412` (launchd restarts the system daemons it manages), or reboot macbook-pro. \
             Its blocked.outside lists the others; the block clears a minute after the gate reopens."
        );
        let mine = next_step("mini", &blocked(Owner::ThisUser, true), now);
        assert!(mine.contains("quit it, or, after checking"), "{mine}");
        assert!(
            mine.contains("run `kill 412` if it does not respond"),
            "{mine}"
        );
        assert!(
            next_step("mini", &blocked(Owner::OtherUser, true), now)
                .contains("run `sudo kill 412`")
        );
        let mut window_server = blocked(Owner::OtherUser, true);
        window_server.outside[0].name = "WindowServer".into();
        window_server.outside[0].memory.footprint_bytes = Some(3 << 30);
        let session = next_step("mini", &window_server, now);
        assert!(!session.contains("kill"), "{session}");
        assert!(session.contains("3.0 GB footprint"), "{session}");
        assert!(session.contains("or reboot it"), "{session}");
        let unread = next_step("mini", &blocked(Owner::Root, false), now);
        assert!(unread.contains("No single outside process"), "{unread}");
        let mut saturated = blocked(Owner::ThisUser, true);
        saturated.reason = Reason::OutsideSaturation {
            outside_utilization: 0.96,
        };
        saturated.since = Timestamp::parse_rfc3339("2026-11-10T13:04:00Z").unwrap();
        assert!(next_step("mini", &saturated, now).starts_with(
            "mini's admission gate has been closed for 6 min: outside load holding 96% of its CPUs."
        ));
        assert_eq!(bytes_text(512 << 20), "512 MB");
    }
}
