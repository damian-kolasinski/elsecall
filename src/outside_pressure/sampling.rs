//! The port through which Elsecall sees what every process on this machine uses,
//! and the pure arithmetic that turns two samples into rates.
//!
//! A sample names processes only: never their arguments, environment or paths,
//! which can hold secrets.
use crate::{failure::Result, scheduling::timestamp::Timestamp};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, time::Duration};

/// Reads what the machine's processes use now.
pub trait ProcessSampler: Send + Sync {
    /// Every process this user can read. Processes that exit or cannot be read
    /// while sampling are left out.
    fn sample(&self) -> Result<Vec<ProcessUse>>;
}

/// One process at one moment.
#[derive(Clone, Debug, PartialEq)]
pub struct ProcessUse {
    pub pid: u32,
    pub parent: u32,
    /// When it started, to the second: with the PID, it tells a process from
    /// a later one that reused its PID.
    pub started: Timestamp,
    /// The executable's short name, as the kernel records it.
    pub name: String,
    pub owner: Owner,
    pub memory: Memory,
    /// User and system time since it started.
    pub cpu_time: Duration,
}

/// Who runs a process, which decides who can restart it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Owner {
    Root,
    /// The user the service runs as.
    ThisUser,
    OtherUser,
}

impl Owner {
    /// The owner as messages to the operator name it.
    pub fn spoken(self) -> &'static str {
        match self {
            Owner::Root => "root",
            Owner::ThisUser => "this user",
            Owner::OtherUser => "another user",
        }
    }
}

/// Memory a process holds. Resident size is readable for every process, so it
/// is what processes are compared by. The physical footprint, which macOS weighs
/// for memory pressure and which includes compressed pages, is larger for a
/// process whose pages were compressed, but it cannot always be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Memory {
    pub resident_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub footprint_bytes: Option<u64>,
}

/// Processes sampled at one instant.
#[derive(Clone, Debug)]
pub struct Snapshot {
    pub at: Timestamp,
    pub processes: Vec<ProcessUse>,
}

/// One process over the period between two snapshots.
#[derive(Clone, Debug, PartialEq)]
pub struct Usage {
    pub process: ProcessUse,
    /// Cores the process kept busy on average: 1.0 is one whole core.
    pub cpu_cores: f64,
    pub uptime: Duration,
}

/// What each process of `current` used since `previous`. A process missing from
/// `previous`, or a new one that reused a PID, is measured since it started.
pub fn usage_between(previous: Option<&Snapshot>, current: &Snapshot) -> Vec<Usage> {
    let earlier: HashMap<(u32, Timestamp), (Duration, Timestamp)> = previous
        .map(|snapshot| {
            snapshot
                .processes
                .iter()
                .map(|p| ((p.pid, p.started), (p.cpu_time, snapshot.at)))
                .collect()
        })
        .unwrap_or_default();
    current
        .processes
        .iter()
        .map(|process| {
            let uptime = current.at.since(process.started);
            let (cpu_before, since) = match earlier.get(&(process.pid, process.started)) {
                Some(&(cpu, at)) if at < current.at => (cpu, current.at.since(at)),
                _ => (Duration::ZERO, uptime),
            };
            let busy = process.cpu_time.saturating_sub(cpu_before);
            Usage {
                process: process.clone(),
                cpu_cores: if since.is_zero() {
                    0.0
                } else {
                    busy.as_secs_f64() / since.as_secs_f64()
                },
                uptime,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn process(pid: u32, started_s: u64, cpu_s: u64) -> ProcessUse {
        ProcessUse {
            pid,
            parent: 1,
            started: Timestamp::from_millis(started_s * 1000),
            name: format!("p{pid}"),
            owner: Owner::ThisUser,
            memory: Memory {
                resident_bytes: 1,
                footprint_bytes: None,
            },
            cpu_time: Duration::from_secs(cpu_s),
        }
    }

    #[test]
    fn rates_come_from_the_period_or_from_the_start() {
        let previous = Snapshot {
            at: Timestamp::from_millis(1_000_000),
            processes: vec![process(10, 0, 500), process(11, 0, 7)],
        };
        let current = Snapshot {
            at: Timestamp::from_millis(1_060_000),
            processes: vec![
                process(10, 0, 560),
                // PID 11 was reused by a process that started 30 s ago.
                process(11, 1_030, 15),
                process(12, 1_050, 5),
            ],
        };
        let usage = usage_between(Some(&previous), &current);
        assert!((usage[0].cpu_cores - 1.0).abs() < 1e-9, "60 s in 60 s");
        assert_eq!(usage[0].uptime, Duration::from_secs(1_060));
        assert!((usage[1].cpu_cores - 0.5).abs() < 1e-9, "since it started");
        assert!((usage[2].cpu_cores - 0.5).abs() < 1e-9);
        let alone = usage_between(None, &current);
        assert!((alone[0].cpu_cores - 560.0 / 1_060.0).abs() < 1e-9);
    }
}
