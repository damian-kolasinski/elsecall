//! Early warnings from the real watcher, trend rules and store, with a fake
//! process table at the system boundary.
use super::{
    attribution::OwnWork,
    remedy,
    sampling::{Memory, Owner, ProcessSampler, ProcessUse},
    trend::INTERVAL,
    warning::{Concern, Warning},
    watcher::{WarningReport, Watcher},
};
use crate::{failure::Result, scheduling::timestamp::Timestamp};
use std::{
    ops::Range,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

const START: u64 = 1_794_000_000;
const SERVICE: u32 = 500;
const GIB: u64 = 1 << 30;
const PHYSICAL: u64 = 64 * GIB;
const HOUR: u64 = 3600;

/// A process that burns CPU at a fixed rate and grows at a fixed rate while it
/// lives, in seconds after START.
struct Process {
    pid: u32,
    parent: u32,
    name: &'static str,
    owner: Owner,
    lives: Range<u64>,
    cores: f64,
    memory: u64,
    growth_per_hour: u64,
    /// When its memory briefly reads as nothing, as through a collection.
    dip: Range<u64>,
}

#[derive(Default)]
struct Machine {
    now: Mutex<u64>,
    processes: Mutex<Vec<Process>>,
}
impl Machine {
    fn add(&self, process: Process) {
        self.processes.lock().unwrap().push(process);
    }
}
impl ProcessSampler for Machine {
    fn sample(&self) -> Result<Vec<ProcessUse>> {
        let now = *self.now.lock().unwrap();
        Ok(self
            .processes
            .lock()
            .unwrap()
            .iter()
            .filter(|p| p.lives.contains(&now))
            .map(|p| {
                let alive = (now - p.lives.start) as f64;
                ProcessUse {
                    pid: p.pid,
                    parent: p.parent,
                    started: Timestamp::from_millis((START + p.lives.start) * 1000),
                    name: p.name.into(),
                    owner: p.owner,
                    memory: Memory {
                        resident_bytes: if p.dip.contains(&now) {
                            0
                        } else {
                            p.memory + (p.growth_per_hour as f64 * alive / 3600.0) as u64
                        },
                        footprint_bytes: None,
                    },
                    cpu_time: Duration::from_secs_f64(alive * p.cores),
                }
            })
            .collect())
    }
}

fn daemon(pid: u32, name: &'static str, cores: f64, memory: u64, growth_per_hour: u64) -> Process {
    Process {
        pid,
        parent: 1,
        name,
        owner: Owner::Root,
        // Up since two days before START, and still running.
        lives: 0..u64::MAX,
        cores,
        memory,
        growth_per_hour,
        dip: 0..0,
    }
}

/// An ordinary machine: launchd, a calm WindowServer and the service.
fn machine() -> Arc<Machine> {
    let machine = Arc::new(Machine::default());
    *machine.now.lock().unwrap() = 2 * 24 * HOUR;
    machine.add(daemon(1, "launchd", 0.0, 0, 0));
    machine.add(daemon(700, "WindowServer", 0.3, GIB, 0));
    machine.add(Process {
        owner: Owner::ThisUser,
        ..daemon(SERVICE, "elsecall", 0.01, 64 << 20, 0)
    });
    machine
}

/// An active grant: its holder and when it started, in seconds after START.
#[derive(Clone, Copy)]
struct Grant {
    holder: u32,
    since: u64,
}

struct Member {
    machine: Arc<Machine>,
    watcher: Watcher,
    path: PathBuf,
    _dir: Arc<tempfile::TempDir>,
}
impl Member {
    fn new(machine: Arc<Machine>) -> Self {
        let dir = Arc::new(tempfile::tempdir().unwrap());
        let path = dir.path().join("outside-processes.sqlite");
        Self {
            watcher: Watcher::open(&path, machine.clone(), PHYSICAL).unwrap(),
            machine,
            path,
            _dir: dir,
        }
    }

    /// The service restarted: a new watcher over the same history.
    fn restarted(self) -> Self {
        Self {
            watcher: Watcher::open(&self.path, self.machine.clone(), PHYSICAL).unwrap(),
            ..self
        }
    }

    fn now(&self) -> u64 {
        *self.machine.now.lock().unwrap()
    }

    /// Takes a round every interval from the next one through `until`, in
    /// seconds after START.
    fn run(&self, until: u64, grant: Option<Grant>) {
        let step = INTERVAL.as_secs();
        let mut at = self.now() + step;
        while at <= until {
            *self.machine.now.lock().unwrap() = at;
            let mut roots: Vec<u32> = grant.iter().map(|g| g.holder).collect();
            roots.push(SERVICE);
            self.watcher.watch(
                Timestamp::from_millis((START + at) * 1000),
                &OwnWork {
                    roots,
                    granted_since: grant.map(|g| Timestamp::from_millis((START + g.since) * 1000)),
                },
            );
            at += step;
        }
    }

    fn warnings(&self) -> Vec<(&'static str, String)> {
        self.watcher
            .warnings()
            .iter()
            .map(|w| (w.concern.kind(), w.process.clone()))
            .collect()
    }
}

fn at(seconds: u64) -> Timestamp {
    Timestamp::from_millis((START + seconds) * 1000)
}

#[test]
fn a_daemon_spinning_a_core_for_six_hours_warns_and_clears_when_restarted() {
    let machine = machine();
    machine.add(daemon(412, "fseventsd", 1.0, 100 << 20, 0));
    let member = Member::new(machine.clone());
    let begin = member.now();
    member.run(begin + 5 * HOUR, None);
    assert_eq!(member.warnings(), [], "five hours are not enough");
    // The first round only set the baseline, so six hours of rates need one
    // more interval.
    member.run(begin + 6 * HOUR + INTERVAL.as_secs(), None);
    assert_eq!(member.warnings(), [("spinning", "fseventsd".to_owned())]);
    let warning = member.watcher.warnings().remove(0);
    assert_eq!(warning.since, at(begin + INTERVAL.as_secs()));
    assert_eq!(warning.concern, Concern::Spinning { cpu_cores: 1.0 });

    // A restart keeps the history and the onset: the warning shows from the
    // start, through the baseline round, and the round after confirms it.
    let member = member.restarted();
    assert_eq!(member.watcher.warnings(), std::slice::from_ref(&warning));
    member.run(member.now() + INTERVAL.as_secs(), None);
    assert_eq!(member.watcher.warnings(), [warning]);
    member.run(member.now() + INTERVAL.as_secs(), None);
    assert_eq!(
        member.watcher.warnings()[0].since,
        at(begin + INTERVAL.as_secs())
    );

    // The operator restarts it: the new process is not spinning, and the
    // warning ends at the last round that saw the old one.
    let killed = member.now() + 1;
    for process in machine.processes.lock().unwrap().iter_mut() {
        if process.pid == 412 {
            process.lives = 0..killed;
        }
    }
    machine.add(Process {
        lives: killed..u64::MAX,
        ..daemon(412, "fseventsd", 0.001, 8 << 20, 0)
    });
    member.run(member.now() + 2 * INTERVAL.as_secs(), None);
    assert_eq!(member.warnings(), []);
    let episodes = member.watcher.between(at(0), at(member.now())).unwrap();
    assert_eq!(episodes.len(), 1);
    assert_eq!(episodes[0].until, Some(at(killed - 1)));
}

#[test]
fn a_long_build_under_an_elsecall_grant_never_warns() {
    // The grant's holder, and below it a build that keeps four cores busy and
    // grows for eight hours, and a simulator the build started that launchd
    // adopted.
    let build = || {
        let machine = machine();
        let begin = machine.now.lock().unwrap().to_owned();
        machine.add(Process {
            owner: Owner::ThisUser,
            lives: begin..u64::MAX,
            ..daemon(900, "elsecall", 0.0, 32 << 20, 0)
        });
        machine.add(Process {
            parent: 900,
            owner: Owner::ThisUser,
            lives: begin + 1..u64::MAX,
            ..daemon(901, "xcodebuild", 4.0, 3 * GIB, GIB / 2)
        });
        machine.add(Process {
            owner: Owner::ThisUser,
            lives: begin + 60..u64::MAX,
            ..daemon(902, "launchd_sim", 1.0, GIB, 0)
        });
        (Member::new(machine), begin)
    };
    let (granted, begin) = build();
    granted.run(
        begin + 8 * HOUR,
        Some(Grant {
            holder: 900,
            since: begin,
        }),
    );
    assert_eq!(granted.warnings(), []);
    // The same work outside any grant is outside, and warns.
    let (outside, begin) = build();
    outside.run(begin + 8 * HOUR, None);
    assert_eq!(
        outside.warnings(),
        [
            ("spinning", "xcodebuild".to_owned()),
            ("growing", "xcodebuild".to_owned()),
            ("spinning", "launchd_sim".to_owned()),
        ]
    );
}

#[test]
fn a_build_daemon_a_grant_started_stays_elsecall_work_in_later_grants() {
    // A build server a granted build starts and launchd adopts, kept busy by
    // back-to-back grants for nine hours.
    let gradle = |first_grant: bool| {
        let machine = machine();
        let begin = machine.now.lock().unwrap().to_owned();
        machine.add(Process {
            owner: Owner::ThisUser,
            lives: begin + 60..u64::MAX,
            ..daemon(950, "java", 1.0, 2 * GIB, 0)
        });
        let member = Member::new(machine);
        let grant = |holder, since| Some(Grant { holder, since });
        // Without the first grant the operator started it: outside.
        member.run(
            begin + HOUR,
            if first_grant { grant(900, begin) } else { None },
        );
        member.run(begin + 9 * HOUR, grant(901, begin + HOUR));
        member.warnings()
    };
    assert_eq!(gradle(true), []);
    assert_eq!(gradle(false), [("spinning", "java".to_owned())]);
}

#[test]
fn growth_that_lapses_for_a_round_continues_its_warning() {
    let machine = machine();
    let begin = machine.now.lock().unwrap().to_owned();
    machine.add(Process {
        lives: begin - HOUR..u64::MAX,
        ..daemon(412, "fseventsd", 0.0, 3 * GIB, GIB)
    });
    let member = Member::new(machine.clone());
    member.run(begin + 6 * HOUR + INTERVAL.as_secs(), None);
    let raised = member.watcher.warnings();
    assert_eq!(member.warnings(), [("growing", "fseventsd".to_owned())]);
    // One sample sees it shrunk, as through a collection: the warning lapses…
    let next = member.now() + INTERVAL.as_secs();
    machine.processes.lock().unwrap().last_mut().unwrap().dip = next..next + 1;
    member.run(next, None);
    assert_eq!(member.warnings(), []);
    // …and continues, with its onset, when the next sample shows it again.
    member.run(next + INTERVAL.as_secs(), None);
    assert_eq!(member.watcher.warnings()[0].since, raised[0].since);
    let episodes = member.watcher.between(at(0), at(member.now())).unwrap();
    assert_eq!(episodes.len(), 1, "{episodes:#?}");
    assert_eq!(episodes[0].until, None);
}

#[test]
fn a_short_burst_does_not_warn() {
    let machine = machine();
    let begin = machine.now.lock().unwrap().to_owned();
    // An outside build: four cores for forty minutes, then gone.
    machine.add(Process {
        owner: Owner::ThisUser,
        lives: begin + 2 * HOUR..begin + 2 * HOUR + 40 * 60,
        ..daemon(1200, "swift-frontend", 4.0, 2 * GIB, 0)
    });
    // Spotlight: about half a core, all day.
    machine.add(Process {
        owner: Owner::OtherUser,
        ..daemon(822, "mds_stores", 0.55, GIB, 0)
    });
    let member = Member::new(machine);
    member.run(begin + 8 * HOUR, None);
    assert_eq!(member.warnings(), []);
}

#[test]
fn steady_growth_warns_and_a_steady_large_app_does_not() {
    let machine = machine();
    let begin = machine.now.lock().unwrap().to_owned();
    // A leaking daemon, up for an hour: 3 GB, growing a gigabyte an hour.
    machine.add(Process {
        lives: begin - HOUR..u64::MAX,
        ..daemon(412, "fseventsd", 0.0, 3 * GIB, GIB)
    });
    machine.add(Process {
        owner: Owner::ThisUser,
        ..daemon(1300, "Xcode", 0.1, 8 * GIB, 0)
    });
    let member = Member::new(machine);
    member.run(begin + 6 * HOUR + INTERVAL.as_secs(), None);
    let warnings = member.watcher.warnings();
    assert_eq!(member.warnings(), [("growing", "fseventsd".to_owned())]);
    let Concern::Growing {
        memory_bytes,
        growth_bytes_per_hour,
    } = warnings[0].concern
    else {
        unreachable!()
    };
    assert!(memory_bytes > 9 * GIB, "{memory_bytes}");
    assert!(
        growth_bytes_per_hour.abs_diff(GIB) < GIB / 50,
        "{growth_bytes_per_hour}"
    );
    // Once it holds a quarter of memory for an hour, it is oversized instead.
    member.run(begin + 15 * HOUR, None);
    assert_eq!(member.warnings(), [("oversized", "fseventsd".to_owned())]);
}

#[test]
fn warning_steps_name_the_process_and_who_can_restart_it() {
    let now = Timestamp::parse_rfc3339("2026-11-10T04:48:00Z").unwrap();
    let warning = |concern| Warning {
        concern,
        process: "fseventsd".into(),
        pid: 412,
        started: Timestamp::parse_rfc3339("2026-09-09T08:00:00Z").unwrap(),
        owner: Owner::Root,
        since: Timestamp::parse_rfc3339("2026-11-08T21:00:00Z").unwrap(),
        observed_at: now,
    };
    assert_eq!(
        remedy::warning_step(
            "macbook-pro",
            &warning(Concern::Spinning { cpu_cores: 1.0 }),
            now
        ),
        "fseventsd (pid 412, root) on macbook-pro has kept about 1.0 cores busy for 1 d 7 h without a break, \
         so Elsecall admits that much less work there. macbook-pro still admits work. \
         Elsecall never stops outside processes: restart it, after checking that `ps -c -o comm= -p 412` on macbook-pro \
         still prints fseventsd, with `sudo kill 412` (launchd restarts the system daemons it manages), or reboot macbook-pro."
    );
    assert_eq!(
        remedy::warning_step(
            "macbook-pro",
            &warning(Concern::Growing {
                memory_bytes: 21 * GIB,
                growth_bytes_per_hour: GIB / 2,
            }),
            now
        ),
        "fseventsd (pid 412, root) on macbook-pro has grown to 21.0 GB of memory, by 512 MB an hour for 1 d 7 h; \
         if it keeps growing, memory pressure will close macbook-pro's admission gate. macbook-pro still admits work. \
         Elsecall never stops outside processes: restart it, after checking that `ps -c -o comm= -p 412` on macbook-pro \
         still prints fseventsd, with `sudo kill 412` (launchd restarts the system daemons it manages), or reboot macbook-pro."
    );
    let mut mine = warning(Concern::Oversized {
        memory_bytes: 18 * GIB,
        share_of_memory: 0.28,
    });
    mine.owner = Owner::ThisUser;
    mine.process = "node".into();
    mine.since = Timestamp::parse_rfc3339("2026-11-10T03:18:00Z").unwrap();
    assert_eq!(
        remedy::warning_step("mini", &mine, now),
        "node (pid 412, this user) on mini has held 18.0 GB of memory, 28% of the machine's, for 1 h 30 min; \
         a little more and memory pressure closes mini's admission gate. mini still admits work. \
         Elsecall never stops outside processes: quit it, or, after checking that `ps -c -o comm= -p 412` on mini \
         still prints node, run `kill 412` if it does not respond."
    );
}
