//! Which processes outside Elsecall's work explain a closed gate.
//!
//! Elsecall's own work is every process descending from the service or from a
//! holder of an active grant, and, while a grant is active, every process of
//! the service's user that started since the earliest active grant did:
//! simulators and build daemons a granted run starts are reparented to launchd,
//! so ancestry alone misses them. The gate is attributed outside only when
//! outside processes use more, on the reason's measure, than Elsecall's own work
//! does; otherwise Elsecall's grants explain it. For CPU that is every outside
//! process together, since cores add up, so an outside `make -j` counts. For
//! memory it is the [`LISTED`] leading outside processes: resident sizes count
//! shared pages once per process, so hundreds of ordinary daemons and apps sum
//! to more than any grant even on an idle Mac.
use crate::{
    outside_pressure::{
        blocked::{OutsideProcess, Reason},
        sampling::{Owner, Usage},
    },
    scheduling::timestamp::Timestamp,
};
use std::collections::{HashMap, HashSet};

/// Outside processes named per blocked state.
pub const LISTED: usize = 5;

/// The kernel and launchd: nobody's to restart, so never named.
const SYSTEM: [u32; 2] = [0, 1];

/// What identifies Elsecall's own work among the machine's processes.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct OwnWork {
    /// The service and the holders of active grants.
    pub roots: Vec<u32>,
    /// When the earliest active grant started; `None` without active grants.
    pub granted_since: Option<Timestamp>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Attribution {
    /// Leading outside processes, leading first; empty when none could be read.
    Outside(Vec<OutsideProcess>),
    /// Elsecall's own work uses at least as much as every outside process
    /// together.
    OwnWork,
}

/// Attributes `reason` among `usage`, given what identifies Elsecall's own work.
pub fn attribute(reason: &Reason, usage: &[Usage], own_work: &OwnWork) -> Attribution {
    let own = own_processes(usage, own_work);
    // Resident size is the one measure every process has, so it is the one
    // processes are compared by.
    let measure = |u: &Usage| match reason {
        Reason::MemoryPressure { .. } => u.process.memory.resident_bytes as f64,
        Reason::OutsideSaturation { .. } => u.cpu_cores,
    };
    let own_total: f64 = usage
        .iter()
        .filter(|u| own.contains(&u.process.pid))
        .map(measure)
        .sum();
    let mut outside: Vec<&Usage> = usage
        .iter()
        .filter(|u| !SYSTEM.contains(&u.process.pid) && !own.contains(&u.process.pid))
        .collect();
    outside.sort_by(|a, b| measure(b).total_cmp(&measure(a)));
    let weighed = match reason {
        Reason::MemoryPressure { .. } => LISTED,
        Reason::OutsideSaturation { .. } => outside.len(),
    };
    let outside_total: f64 = outside.iter().take(weighed).map(|u| measure(u)).sum();
    if own_total > 0.0 && outside_total <= own_total {
        return Attribution::OwnWork;
    }
    Attribution::Outside(
        outside
            .into_iter()
            .take(LISTED)
            .map(|u| OutsideProcess {
                name: u.process.name.clone(),
                pid: u.process.pid,
                owner: u.process.owner,
                memory: u.process.memory,
                cpu_cores: (u.cpu_cores * 100.0).round() / 100.0,
                uptime_seconds: u.uptime.as_secs(),
            })
            .collect(),
    )
}

/// The processes of `usage` that are neither Elsecall's own work nor the
/// kernel or launchd.
pub fn outside<'a>(usage: &'a [Usage], own_work: &OwnWork) -> Vec<&'a Usage> {
    let own = own_processes(usage, own_work);
    usage
        .iter()
        .filter(|u| !SYSTEM.contains(&u.process.pid) && !own.contains(&u.process.pid))
        .collect()
}

/// The roots, this user's processes started since the earliest active grant,
/// and every process below them.
pub fn own_processes(usage: &[Usage], own_work: &OwnWork) -> HashSet<u32> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for u in usage {
        if u.process.pid != u.process.parent {
            children
                .entry(u.process.parent)
                .or_default()
                .push(u.process.pid);
        }
    }
    // Start times are whole seconds, so compare from the grant's second.
    let since = own_work
        .granted_since
        .map(|at| Timestamp::from_millis(at.millis() / 1000 * 1000));
    let mut pending: Vec<u32> = own_work.roots.clone();
    pending.extend(
        usage
            .iter()
            .filter(|u| {
                u.process.owner == Owner::ThisUser
                    && since.is_some_and(|since| u.process.started >= since)
            })
            .map(|u| u.process.pid),
    );
    let mut found: HashSet<u32> = HashSet::new();
    while let Some(pid) = pending.pop() {
        if !SYSTEM.contains(&pid) && found.insert(pid) {
            pending.extend(children.get(&pid).into_iter().flatten());
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        inspection::MemoryPressure,
        outside_pressure::sampling::{Memory, ProcessUse},
    };
    use std::time::Duration;

    fn usage(pid: u32, parent: u32, gib: u64, cores: f64) -> Usage {
        Usage {
            process: ProcessUse {
                pid,
                parent,
                started: Timestamp::from_millis(0),
                name: format!("p{pid}"),
                owner: Owner::ThisUser,
                memory: Memory {
                    resident_bytes: gib << 30,
                    footprint_bytes: None,
                },
                cpu_time: Duration::ZERO,
            },
            cpu_cores: cores,
            uptime: Duration::from_secs(60),
        }
    }
    const MEMORY: Reason = Reason::MemoryPressure {
        level: MemoryPressure::Warning,
    };
    const SATURATED: Reason = Reason::OutsideSaturation {
        outside_utilization: 0.95,
    };
    fn roots(roots: &[u32]) -> OwnWork {
        OwnWork {
            roots: roots.to_vec(),
            granted_since: None,
        }
    }
    fn pids(attribution: Attribution) -> Vec<u32> {
        match attribution {
            Attribution::Outside(outside) => outside.iter().map(|p| p.pid).collect(),
            Attribution::OwnWork => panic!("expected outside"),
        }
    }

    #[test]
    fn own_work_is_the_service_and_holders_with_all_their_descendants() {
        // 0 the kernel and 1 launchd; 50 the service, 51 its hosted command; 60 a
        // holder, 61 its command, 62 a grandchild; 70 and 71 outside.
        let table = [
            usage(0, 0, 90, 9.0),
            usage(1, 0, 2, 0.5),
            usage(50, 1, 0, 0.0),
            usage(51, 50, 6, 0.5),
            usage(60, 1, 0, 0.0),
            usage(61, 60, 4, 3.0),
            usage(62, 61, 4, 3.0),
            usage(70, 1, 15, 0.2),
            usage(71, 1, 2, 7.0),
        ];
        // 17 GiB outside beats 14 GiB of own work; the kernel and launchd are
        // never named.
        assert_eq!(
            pids(attribute(&MEMORY, &table, &roots(&[50, 60]))),
            [70, 71]
        );
        // 7.2 cores outside beat 6.5 of own work.
        assert_eq!(
            pids(attribute(&SATURATED, &table, &roots(&[50, 60]))),
            [71, 70]
        );
        let mut heavier = table.to_vec();
        heavier[6] = usage(62, 61, 20, 3.0);
        assert_eq!(
            attribute(&MEMORY, &heavier, &roots(&[50, 60])),
            Attribution::OwnWork,
            "30 GiB of own work explains the pressure"
        );
        assert_eq!(
            pids(attribute(&SATURATED, &heavier, &roots(&[])))[..3],
            [71, 61, 62],
            "without grants or the service everything is outside"
        );
        assert_eq!(
            attribute(&MEMORY, &[], &roots(&[50])),
            Attribution::Outside(vec![]),
            "an unreadable table names nobody but still blocks"
        );
    }

    #[test]
    fn many_small_outside_processes_outweigh_own_work_together() {
        // An outside `make -j14`: fourteen compilers at a core each against a
        // granted run on two.
        let mut table = vec![usage(60, 1, 0, 0.0), usage(61, 60, 1, 2.0)];
        table.extend((100..114).map(|pid| usage(pid, 99, 0, 1.0)));
        assert_eq!(
            pids(attribute(&SATURATED, &table, &roots(&[60]))).len(),
            LISTED
        );
    }

    #[test]
    fn an_ordinary_mac_does_not_outweigh_a_grant_that_fills_memory() {
        // Six hundred daemons and apps of 50 to 300 MB, 100 GiB together, and
        // a granted run holding 8 GiB.
        let mut table: Vec<Usage> = (1000..1600)
            .map(|pid| {
                let mut u = usage(pid, 1, 0, 0.01);
                u.process.memory.resident_bytes = (50 + (pid as u64 % 251)) << 20;
                u
            })
            .collect();
        table.push(usage(60, 1, 0, 0.0));
        table.push(usage(61, 60, 8, 2.0));
        assert_eq!(
            attribute(&MEMORY, &table, &roots(&[60])),
            Attribution::OwnWork
        );
        // A hung daemon among them still leads.
        table.push(usage(412, 1, 27, 1.0));
        assert_eq!(pids(attribute(&MEMORY, &table, &roots(&[60])))[0], 412);
    }

    #[test]
    fn this_users_processes_started_during_a_grant_are_own_work() {
        // Simulators a granted run boots are reparented to launchd.
        let granted = Timestamp::from_millis(1_000_500);
        let started = |mut u: Usage, s: u64| {
            u.process.started = Timestamp::from_millis(s * 1000);
            u
        };
        let mut root_daemon = started(usage(80, 1, 12, 0.0), 1_100);
        root_daemon.process.owner = Owner::Root;
        let table = [
            started(usage(60, 1, 0, 0.0), 1_000),
            started(usage(61, 60, 1, 1.0), 1_000),
            started(usage(70, 1, 30, 4.0), 1_000),
            started(usage(71, 70, 2, 0.0), 1_200),
            started(usage(72, 1, 3, 0.0), 999),
            root_daemon,
        ];
        let own = OwnWork {
            roots: vec![60],
            granted_since: Some(granted),
        };
        assert_eq!(
            attribute(&MEMORY, &table, &own),
            Attribution::OwnWork,
            "33 GiB of grant-era processes against 15 GiB outside"
        );
        let Attribution::Outside(outside) = attribute(&MEMORY, &table, &roots(&[60])) else {
            panic!("without the grant era the simulators are outside");
        };
        assert_eq!(outside[0].pid, 70);
    }
}
