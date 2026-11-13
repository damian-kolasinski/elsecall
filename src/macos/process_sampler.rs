//! What every process on this Mac uses, read without privileges.
//!
//! macOS refuses libproc's task and usage calls for other users' processes, root
//! daemons included, but `/bin/ps` is setuid and reads every process's resident
//! size, CPU time and start time. So one `ps` listing covers every process, and
//! `proc_pid_rusage` upgrades memory to the physical footprint, which is what
//! macOS weighs for memory pressure, wherever the kernel allows it: this user's
//! processes. `ps` is asked for the executable's name only (`-c -o comm`), never
//! its arguments or environment.
use crate::{
    failure::{Failure, Result},
    outside_pressure::sampling::{Memory, Owner, ProcessSampler, ProcessUse},
    scheduling::timestamp::Timestamp,
};
use std::{process::Command, time::Duration};

const PS: &str = "/bin/ps";
/// The columns read: `lstart` is five words, and `comm` comes last because a
/// name can contain spaces.
const COLUMNS: &str = "pid=,ppid=,uid=,rss=,time=,lstart=,comm=";

#[derive(Default)]
pub struct MacProcessSampler;

impl ProcessSampler for MacProcessSampler {
    fn sample(&self) -> Result<Vec<ProcessUse>> {
        // A fixed locale and zone, so start times parse the same everywhere.
        let output = Command::new(PS)
            .args(["-A", "-c", "-o", COLUMNS])
            .env_clear()
            .env("LC_ALL", "C")
            .env("TZ", "UTC0")
            .output()
            .map_err(|e| Failure::io("List processes", e))?;
        if !output.status.success() {
            return Err(Failure::new(
                "process_listing_failed",
                format!("ps could not list processes ({}).", output.status),
                "Check that /bin/ps runs for this user.",
            ));
        }
        // SAFETY: getuid has no preconditions.
        let me = unsafe { libc::getuid() };
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| parse(line, me))
            .map(|mut process| {
                process.memory.footprint_bytes = footprint(process.pid);
                process
            })
            .collect())
    }
}

/// One `ps` line; `None` for a line this parser does not understand.
fn parse(line: &str, me: u32) -> Option<ProcessUse> {
    let mut rest = line.trim_start();
    let mut word = || {
        let end = rest.find(char::is_whitespace)?;
        let (word, tail) = rest.split_at(end);
        rest = tail.trim_start();
        Some(word)
    };
    let pid = word()?.parse().ok()?;
    let parent = word()?.parse().ok()?;
    let uid: u32 = word()?.parse().ok()?;
    let resident_kib: u64 = word()?.parse().ok()?;
    let cpu_time = cpu_time(word()?)?;
    let _weekday = word()?;
    let month = month(word()?)?;
    let day: u32 = word()?.parse().ok()?;
    let clock = word()?;
    let year: i64 = word()?.parse().ok()?;
    // Names reach agent-facing text: no control characters.
    let name: String = rest
        .trim_end()
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if name.is_empty() {
        return None;
    }
    Some(ProcessUse {
        pid,
        parent,
        started: started(year, month, day, clock)?,
        name,
        owner: match uid {
            0 => Owner::Root,
            uid if uid == me => Owner::ThisUser,
            _ => Owner::OtherUser,
        },
        memory: Memory {
            resident_bytes: resident_kib.saturating_mul(1024),
            footprint_bytes: None,
        },
        cpu_time,
    })
}

/// `ps` CPU time: `[d-][h:]m:s.cc`, where minutes can exceed 59.
fn cpu_time(text: &str) -> Option<Duration> {
    let (days, clock) = match text.split_once('-') {
        Some((days, clock)) => (days.parse::<u64>().ok()?, clock),
        None => (0, text),
    };
    let mut seconds = 0.0;
    for part in clock.split(':') {
        seconds = seconds * 60.0 + part.parse::<f64>().ok()?;
    }
    (seconds.is_finite() && seconds >= 0.0)
        .then(|| Duration::from_secs(days * 86_400) + Duration::from_secs_f64(seconds))
}

fn month(name: &str) -> Option<u32> {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    MONTHS.iter().position(|m| *m == name).map(|i| i as u32 + 1)
}

/// A UTC civil time as a timestamp.
fn started(year: i64, month: u32, day: u32, clock: &str) -> Option<Timestamp> {
    let mut parts = clock.split(':').map(|p| p.parse::<u64>().ok());
    let (hours, minutes, seconds) = (parts.next()??, parts.next()??, parts.next()??);
    if !(1..=31).contains(&day) || hours > 23 || minutes > 59 || seconds > 60 {
        return None;
    }
    // Days from the epoch, after Howard Hinnant's days_from_civil.
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let year_of_era = y - era * 400;
    let m = i64::from(month);
    let day_of_year = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = u64::try_from(era * 146_097 + day_of_era - 719_468).ok()?;
    let seconds = days * 86_400 + hours * 3600 + minutes * 60 + seconds;
    Some(Timestamp::from_millis(seconds * 1000))
}

/// The physical footprint, where the kernel lets this user read it.
fn footprint(pid: u32) -> Option<u64> {
    // SAFETY: rusage_info_v2 is plain old data; zero is a valid bit pattern.
    let mut usage: libc::rusage_info_v2 = unsafe { std::mem::zeroed() };
    // SAFETY: the buffer is a rusage_info_v2, which the V2 flavor fills.
    let result = unsafe {
        libc::proc_pid_rusage(
            libc::c_int::try_from(pid).ok()?,
            libc::RUSAGE_INFO_V2,
            (&mut usage as *mut libc::rusage_info_v2).cast(),
        )
    };
    (result == 0).then_some(usage.ri_phys_footprint)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ps_lines_including_names_with_spaces() {
        let root = parse(
            "  628     1     0  18096 1589:31.94 Sat Nov  7 20:54:48 2026     fseventsd",
            502,
        )
        .unwrap();
        assert_eq!((root.pid, root.parent), (628, 1));
        assert_eq!(root.owner, Owner::Root);
        assert_eq!(root.memory.resident_bytes, 18096 * 1024);
        assert_eq!(root.cpu_time, Duration::from_millis(95_371_940));
        assert_eq!(
            root.started,
            Timestamp::parse_rfc3339("2026-11-07T20:54:48Z").unwrap()
        );
        let mine = parse(
            "70317 70168   502 912 2-03:04:05.50 Thu Feb 29 00:00:01 2024 Claude Helper (Renderer)",
            502,
        )
        .unwrap();
        assert_eq!(mine.name, "Claude Helper (Renderer)");
        assert_eq!(mine.owner, Owner::ThisUser);
        assert_eq!(
            mine.cpu_time,
            Duration::from_secs(2 * 86_400 + 3 * 3600 + 4 * 60 + 5) + Duration::from_millis(500)
        );
        assert_eq!(
            mine.started,
            Timestamp::parse_rfc3339("2024-02-29T00:00:01Z").unwrap()
        );
        assert_eq!(
            parse("1 0 7 1 0:00.01 Sat Nov  7 13:24:38 2026 x", 502)
                .unwrap()
                .owner,
            Owner::OtherUser
        );
        assert_eq!(
            parse("1 0 7 1 0:00.01 Sat Nov  7 13:24:38 2026 bad\x1bname", 502)
                .unwrap()
                .name,
            "bad name"
        );
        for broken in [
            "",
            "pid ppid",
            "1 0 0 1 0:00.01 Sat Nov  7 13:24:38 2026",
            "1 0 0 1 0:00.01 Sat Bad  7 13:24:38 2026 x",
            "1 0 0 1 x Sat Nov  7 13:24:38 2026 x",
        ] {
            assert_eq!(parse(broken, 502), None, "{broken:?}");
        }
    }

    #[test]
    fn samples_this_process_its_busy_child_and_root_daemons() {
        let mut child = Command::new("/usr/bin/yes")
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        std::thread::sleep(Duration::from_millis(300));
        let sampled = MacProcessSampler.sample().unwrap();
        child.kill().unwrap();
        child.wait().unwrap();
        let me = sampled
            .iter()
            .find(|p| p.pid == std::process::id())
            .expect("this process is listed");
        assert_eq!(me.owner, Owner::ThisUser);
        assert!(me.memory.footprint_bytes.is_some_and(|b| b > 0));
        assert!(me.memory.resident_bytes > 0);
        let busy = sampled
            .iter()
            .find(|p| p.pid == child.id())
            .expect("the child is listed");
        assert_eq!(busy.parent, std::process::id());
        assert_eq!(busy.name, "yes");
        assert!(busy.cpu_time >= Duration::from_millis(100), "{busy:?}");
        assert!(busy.started <= Timestamp::now());
        let launchd = sampled
            .iter()
            .find(|p| p.pid == 1)
            .expect("launchd is listed without privileges");
        assert_eq!(
            (launchd.owner, launchd.name.as_str()),
            (Owner::Root, "launchd")
        );
        assert!(launchd.memory.resident_bytes > 0);
        assert_eq!(launchd.memory.footprint_bytes, None, "refused for root");
        assert!(launchd.cpu_time > Duration::ZERO);
    }

    /// Prints the machine's leading processes:
    /// `cargo test live_sample -- --ignored --nocapture`.
    #[test]
    #[ignore = "reads this machine's processes for a manual check"]
    fn live_sample() {
        let mut sampled = MacProcessSampler.sample().unwrap();
        let footprints = sampled
            .iter()
            .filter(|p| p.memory.footprint_bytes.is_some())
            .count();
        println!("{} processes, {footprints} with a footprint", sampled.len());
        sampled.sort_by_key(|p| std::cmp::Reverse(p.memory.resident_bytes));
        for p in sampled.iter().take(8) {
            println!(
                "{:>6} {:<40} {:?} {:?} cpu {:?}",
                p.pid, p.name, p.owner, p.memory, p.cpu_time
            );
        }
        for p in sampled.iter().filter(|p| p.name == "fseventsd") {
            println!("fseventsd: {p:?}");
        }
        // Attribution as the detector would make it, over two seconds.
        use crate::outside_pressure::{
            attribution::attribute,
            blocked::Reason,
            sampling::{Snapshot, usage_between},
        };
        let earlier = Snapshot {
            at: Timestamp::now(),
            processes: MacProcessSampler.sample().unwrap(),
        };
        std::thread::sleep(Duration::from_secs(2));
        let later = Snapshot {
            at: Timestamp::now(),
            processes: MacProcessSampler.sample().unwrap(),
        };
        let usage = usage_between(Some(&earlier), &later);
        for reason in [
            Reason::MemoryPressure {
                level: crate::inspection::MemoryPressure::Warning,
            },
            Reason::OutsideSaturation {
                outside_utilization: 0.95,
            },
        ] {
            println!(
                "{reason:?}: {}",
                serde_json::to_string_pretty(&match attribute(
                    &reason,
                    &usage,
                    &crate::outside_pressure::attribution::OwnWork {
                        roots: vec![std::process::id()],
                        granted_since: None,
                    }
                ) {
                    crate::outside_pressure::attribution::Attribution::Outside(outside) =>
                        serde_json::to_value(outside).unwrap(),
                    own => serde_json::Value::String(format!("{own:?}")),
                })
                .unwrap()
            );
        }
    }
}
