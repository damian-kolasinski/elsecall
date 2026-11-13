//! The detector against the real gate, state machine and episode store, with a
//! fake process table at the system boundary.
use super::{
    attribution::OwnWork,
    detector::{BlockedReport, Detector, Observation},
    sampling::{Memory, Owner, ProcessSampler, ProcessUse},
};
use crate::{
    failure::Result,
    inspection::MemoryPressure,
    outside_pressure::Reason,
    scheduling::{load, parameters::Parameters, timestamp::Timestamp},
};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

const START: u64 = 1_794_000_000;
const SERVICE: u32 = 500;

/// A process table whose processes burn CPU at fixed rates as time passes.
#[derive(Default)]
struct Machine {
    now: Mutex<u64>,
    processes: Mutex<Vec<(ProcessUse, f64)>>,
    samples: Mutex<u32>,
}
impl Machine {
    fn add(&self, pid: u32, parent: u32, name: &str, owner: Owner, gib: u64, cores: f64) {
        self.processes.lock().unwrap().push((
            ProcessUse {
                pid,
                parent,
                started: Timestamp::from_millis((START - 86_400) * 1000),
                name: name.into(),
                owner,
                memory: Memory {
                    resident_bytes: gib << 30,
                    footprint_bytes: None,
                },
                cpu_time: Duration::from_secs(1000),
            },
            cores,
        ));
    }
}
impl ProcessSampler for Machine {
    fn sample(&self) -> Result<Vec<ProcessUse>> {
        *self.samples.lock().unwrap() += 1;
        let elapsed = *self.now.lock().unwrap() - START;
        Ok(self
            .processes
            .lock()
            .unwrap()
            .iter()
            .map(|(process, cores)| ProcessUse {
                cpu_time: process.cpu_time + Duration::from_secs_f64(elapsed as f64 * cores),
                ..process.clone()
            })
            .collect())
    }
}

/// The machine in the 2026-11-10 incident: a hung fseventsd, ordinary apps,
/// and the Elsecall service.
fn incident() -> Arc<Machine> {
    let machine = Arc::new(Machine::default());
    *machine.now.lock().unwrap() = START;
    machine.add(1, 0, "launchd", Owner::Root, 0, 0.0);
    machine.add(412, 1, "fseventsd", Owner::Root, 27, 1.0);
    machine.add(700, 1, "WindowServer", Owner::Root, 1, 0.3);
    machine.add(800, 1, "Simulator", Owner::ThisUser, 3, 0.1);
    machine.add(SERVICE, 1, "elsecall", Owner::ThisUser, 0, 0.0);
    machine
}

/// An active grant: its holder's PID and when it started, in seconds after START.
#[derive(Clone, Copy)]
struct Grant {
    holder: u32,
    since: u64,
}

struct Member {
    machine: Arc<Machine>,
    detector: Detector,
    _dir: tempfile::TempDir,
}
impl Member {
    fn new(machine: Arc<Machine>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let detector =
            Detector::open(&dir.path().join("outside-pressure.sqlite"), machine.clone()).unwrap();
        Self {
            machine,
            detector,
            _dir: dir,
        }
    }

    /// Runs one second per second from the last one through `until` (seconds
    /// after START), with the gate the scheduler would compute from `pressure`,
    /// `cpu` and the grants in use.
    fn run(
        &self,
        until: u64,
        pressure: MemoryPressure,
        cpu: f64,
        in_use: f64,
        grant: Option<Grant>,
    ) {
        let parameters = Parameters::default().admission;
        let from = *self.machine.now.lock().unwrap() - START;
        for second in from + 1..=until {
            *self.machine.now.lock().unwrap() = START + second;
            let gate = load::gate(pressure, Some(cpu), in_use / 14.0, &parameters);
            let mut roots: Vec<u32> = grant.iter().map(|g| g.holder).collect();
            roots.push(SERVICE);
            self.detector.observe(Observation {
                at: at(second),
                gate,
                own: OwnWork {
                    roots,
                    granted_since: grant.map(|g| at(g.since)),
                },
            });
        }
    }

    fn episodes(&self) -> Vec<super::store::Episode> {
        self.detector.episodes(at(0), at(100_000)).unwrap()
    }
}

fn at(second: u64) -> Timestamp {
    Timestamp::from_millis((START + second) * 1000)
}

#[test]
fn sustained_memory_pressure_blocks_naming_the_leading_outside_process() {
    let member = Member::new(incident());
    member.run(10, MemoryPressure::Normal, 0.1, 0.0, None);
    member.run(299, MemoryPressure::Warning, 0.15, 0.0, None);
    assert_eq!(member.detector.blocked(), None, "not yet five minutes");
    member.run(311, MemoryPressure::Warning, 0.15, 0.0, None);
    let blocked = member
        .detector
        .blocked()
        .expect("blocked after five minutes");
    assert_eq!(
        blocked.reason,
        Reason::MemoryPressure {
            level: MemoryPressure::Warning
        }
    );
    assert_eq!(blocked.since, at(11), "since the gate closed");
    let leading = blocked.leading().unwrap();
    assert_eq!(
        (leading.name.as_str(), leading.pid, leading.owner),
        ("fseventsd", 412, Owner::Root)
    );
    assert_eq!(leading.memory.resident_bytes, 27 << 30);
    assert!(
        (leading.cpu_cores - 1.0).abs() < 0.02,
        "a rate over the last sample period, not its lifetime: {}",
        leading.cpu_cores
    );
    assert_eq!(leading.uptime_seconds, 86_400 + 311);
    assert_eq!(
        blocked.outside.iter().map(|p| p.pid).collect::<Vec<_>>(),
        [412, 800, 700],
        "by memory, without Elsecall's own service or launchd"
    );
    let samples = *member.machine.samples.lock().unwrap();
    assert!(
        samples <= 7,
        "about one sample a minute, not one a second: {samples}"
    );

    // Recorded while it lasts, so a report can name it.
    let episodes = member.episodes();
    assert_eq!(episodes.len(), 1);
    assert_eq!(episodes[0].until, None);
    assert_eq!(episodes[0].blocked.leading().unwrap().name, "fseventsd");

    // `sudo killall fseventsd`: the gate opens and, a minute later, the block clears.
    member.run(1000, MemoryPressure::Warning, 0.15, 0.0, None);
    member.run(1030, MemoryPressure::Normal, 0.05, 0.0, None);
    assert!(member.detector.blocked().is_some(), "a minute's grace");
    member.run(1061, MemoryPressure::Normal, 0.05, 0.0, None);
    assert_eq!(member.detector.blocked(), None);
    let episodes = member.episodes();
    assert_eq!(episodes.len(), 1);
    assert_eq!(episodes[0].until, Some(at(1000)));
    assert_eq!(episodes[0].blocked.since, at(11));
}

#[test]
fn a_short_spike_blocks_nothing() {
    let member = Member::new(incident());
    member.run(200, MemoryPressure::Warning, 0.2, 0.0, None);
    member.run(260, MemoryPressure::Normal, 0.2, 0.0, None);
    member.run(400, MemoryPressure::Critical, 0.2, 0.0, None);
    member.run(600, MemoryPressure::Normal, 0.2, 0.0, None);
    assert_eq!(member.detector.blocked(), None);
    assert!(member.episodes().is_empty());
}

#[test]
fn pressure_from_elsecalls_own_grants_is_not_attributed_outside() {
    let machine = incident();
    // fseventsd behaves; a granted UI test run holds 40 GB, mostly in the
    // simulator it booted, which launchd adopted.
    machine.processes.lock().unwrap()[1].0.memory.resident_bytes = 1 << 30;
    machine.add(900, 1, "elsecall", Owner::ThisUser, 0, 0.0);
    machine.add(901, 900, "xcodebuild", Owner::ThisUser, 4, 2.0);
    machine.add(902, 1, "launchd_sim", Owner::ThisUser, 36, 4.0);
    for process in machine.processes.lock().unwrap().iter_mut() {
        if process.0.pid >= 900 {
            process.0.started = at(5);
        }
    }
    let grant = Some(Grant {
        holder: 900,
        since: 5,
    });
    let member = Member::new(machine);
    member.run(900, MemoryPressure::Warning, 0.6, 6.0, grant);
    assert_eq!(member.detector.blocked(), None);
    assert!(member.episodes().is_empty());

    // Saturation the grants explain keeps the gate open: 13 of 14 cores are
    // granted, and the machine is 95% busy.
    member.run(1800, MemoryPressure::Normal, 0.95, 13.0, grant);
    assert_eq!(member.detector.blocked(), None);

    // Once the grant ends, the same saturation is outside load, and the outside
    // process burning the most CPU leads.
    member
        .machine
        .add(950, 1, "mds_stores", Owner::Root, 1, 9.0);
    member.run(2200, MemoryPressure::Normal, 0.95, 0.0, None);
    let blocked = member.detector.blocked().expect("outside saturation");
    assert!(matches!(blocked.reason, Reason::OutsideSaturation { .. }));
    assert_eq!(blocked.leading().unwrap().name, "mds_stores");
}

#[test]
fn a_restart_during_a_block_keeps_its_onset() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("outside-pressure.sqlite");
    let machine = incident();
    let first = Member {
        machine: machine.clone(),
        detector: Detector::open(&path, machine.clone()).unwrap(),
        _dir: tempfile::tempdir().unwrap(),
    };
    first.run(400, MemoryPressure::Warning, 0.15, 0.0, None);
    assert_eq!(first.detector.blocked().unwrap().since, at(1));
    drop(first);
    // The service restarts 20 s later, and the pressure still lasts.
    *machine.now.lock().unwrap() = START + 420;
    let second = Member {
        machine: machine.clone(),
        detector: Detector::open(&path, machine.clone()).unwrap(),
        _dir: dir,
    };
    // The first seconds read the gate as unknown, before the scheduler has a
    // load window.
    for second_at in 421..424 {
        second.detector.observe(Observation {
            at: at(second_at),
            gate: crate::scheduling::load::Gate::Unknown,
            own: OwnWork::default(),
        });
    }
    assert_eq!(second.detector.blocked(), None);
    *machine.now.lock().unwrap() = START + 423;
    second.run(424, MemoryPressure::Warning, 0.15, 0.0, None);
    assert_eq!(
        second.detector.blocked().unwrap().since,
        at(1),
        "back at once, with its onset"
    );
    let episodes = second.episodes();
    assert_eq!(episodes.len(), 1, "the same episode, reopened");
    assert_eq!(episodes[0].until, None);
}
