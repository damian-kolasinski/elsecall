//! Processes a placed command left running in its process group when it exited:
//! they keep running, as they would after a local run, until they end, reach the
//! cap, or the next run of the same copy stops them. They are not admitted work;
//! their CPU time is measured and reported for the run's record.
use crate::{
    macos::process_group::{self, Member},
    scheduling::{
        leftovers::{Ending, Leftovers},
        processes::ProcessIdentity,
        timestamp::Timestamp,
    },
    workspace::copies::Lingering,
};
use futures_util::future::BoxFuture;
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{sync::watch, task::JoinHandle};

/// How often the group is sampled.
const SAMPLE: Duration = Duration::from_millis(500);
/// How long stopped leftovers have to exit before they are killed.
const STOP_GRACE: Duration = Duration::from_secs(5);
/// How long killed leftovers have to disappear before the watch gives up on them.
const KILL_WAIT: Duration = Duration::from_secs(2);
/// At most this many of them are recorded, for a restart to find the group.
const RECORDED: usize = 8;
/// The longest stopping leftovers takes, from the request to the end of the watch.
pub const STOPPING: Duration = Duration::from_secs(8);

/// How long leftovers may run after the command exits; zero stops them at once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cap(pub Duration);

impl Default for Cap {
    fn default() -> Self {
        Cap(Duration::from_secs(10 * 60))
    }
}

/// Watches one run's leftovers; cloned handles share them.
#[derive(Clone)]
pub struct Watch {
    stop: Arc<watch::Sender<Option<String>>>,
    ended: watch::Receiver<bool>,
}

/// What a watch reports as the leftovers change, for the run's record.
pub type Report = Box<dyn Fn(Leftovers) + Send + Sync>;

/// Starts watching group `group` of a command that just exited. `None` when the
/// command left nothing running. `cap` bounds how long they run; `zero` stops
/// them at once. `tied` are tasks that live as long as the leftovers, such as
/// readers keeping their output pipes open.
pub fn watch(
    group: u32,
    Cap(cap): Cap,
    report: Report,
    tied: Vec<JoinHandle<()>>,
) -> Option<Watch> {
    let exited = Instant::now();
    let since = Timestamp::now();
    let first = ours(group, process_group::members(group));
    if first.is_empty() {
        for task in tied {
            task.abort();
        }
        return None;
    }
    let (stop, stop_requested) = watch::channel(None);
    let (ended_sender, ended) = watch::channel(false);
    let sampled = Sampled::new(&first);
    report(running(since, cap, group, &sampled.recorded));
    tokio::spawn(async move {
        let mut leftovers = Supervised {
            group,
            since,
            cap,
            exited,
            report,
            sampled,
            #[cfg(test)]
            observed_cpu: None,
        };
        let record = leftovers.supervise(stop_requested).await;
        for task in tied {
            task.abort();
        }
        (leftovers.report)(record);
        let _ = ended_sender.send(true);
    });
    Some(Watch {
        stop: Arc::new(stop),
        ended,
    })
}

fn running(since: Timestamp, cap: Duration, group: u32, recorded: &[ProcessIdentity]) -> Leftovers {
    Leftovers::Running {
        since,
        cap_ms: millis(cap),
        group,
        processes: recorded.to_vec(),
    }
}

fn millis(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

/// The members of `group` that are the exited command's leftovers. The command
/// led the group and was reaped, so a process whose PID is the group's ID means
/// the ID was reused by an unrelated group once the leftovers had all ended: a
/// PID is never reused while its process group exists. Then there are none.
fn ours(group: u32, members: Vec<Member>) -> Vec<Member> {
    if members.iter().any(|m| m.identity.pid == group) {
        Vec::new()
    } else {
        members
    }
}

/// CPU time the leftovers used since the command exited, from samples.
struct Sampled {
    /// Each process's CPU time when first seen after the exit: zero for those
    /// that started later, what it had already used for those running at the exit.
    baseline: HashMap<ProcessIdentity, Duration>,
    /// Each process's latest CPU time, with its reaped children's, and its parent.
    latest: HashMap<ProcessIdentity, (Duration, u32)>,
    /// The processes of the latest sample.
    alive: Vec<ProcessIdentity>,
    /// The oldest processes, which the record names; while one of them lives, a
    /// restart finds the group through it.
    recorded: Vec<ProcessIdentity>,
}

impl Sampled {
    fn new(first: &[Member]) -> Self {
        let mut sampled = Sampled {
            baseline: first.iter().map(|m| (m.identity, m.cpu)).collect(),
            latest: HashMap::new(),
            alive: Vec::new(),
            recorded: Vec::new(),
        };
        sampled.add(first);
        sampled
    }

    /// Takes in a sample; true when one of the recorded processes ended, so the
    /// record should name the current oldest ones.
    fn add(&mut self, members: &[Member]) -> bool {
        for member in members {
            self.baseline.entry(member.identity).or_default();
            let latest = self
                .latest
                .entry(member.identity)
                .or_insert((Duration::ZERO, member.parent));
            *latest = (latest.0.max(member.cpu), member.parent);
        }
        // A process that ended while its parent still runs is reaped by that
        // parent, whose CPU time then includes all of the child's: counted once.
        let pids: Vec<u32> = members.iter().map(|m| m.identity.pid).collect();
        for ended in self
            .alive
            .iter()
            .filter(|a| !members.iter().any(|m| m.identity == **a))
        {
            if let Some((cpu, parent)) = self.latest.get(ended).copied()
                && pids.contains(&parent)
            {
                self.baseline.insert(*ended, cpu);
            }
        }
        self.alive = members.iter().map(|m| m.identity).collect();
        let stale = !members.is_empty()
            && (self.recorded.is_empty() || self.recorded.iter().any(|r| !self.alive.contains(r)));
        if stale {
            let mut oldest = self.alive.clone();
            oldest.sort_by_key(|identity| (identity.started_us, identity.pid));
            oldest.truncate(RECORDED);
            self.recorded = oldest;
        }
        stale
    }

    fn cpu(&self) -> Duration {
        self.latest
            .iter()
            .map(|(identity, (latest, _))| latest.saturating_sub(self.baseline[identity]))
            .sum()
    }
}

struct Supervised {
    group: u32,
    since: Timestamp,
    cap: Duration,
    exited: Instant,
    report: Report,
    sampled: Sampled,
    /// Observe real sampling without changing its result or scheduling.
    #[cfg(test)]
    observed_cpu: Option<watch::Sender<Duration>>,
}

impl Supervised {
    /// Follows the leftovers until they end or are stopped; returns the record of
    /// how they ended.
    async fn supervise(&mut self, mut stop: watch::Receiver<Option<String>>) -> Leftovers {
        let cap_at = tokio::time::Instant::from_std(self.exited + self.cap);
        let mut ticks = tokio::time::interval(SAMPLE);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // Until every handle is gone, when nothing can ask to stop them any more.
        let mut stoppable = true;
        let ending = loop {
            tokio::select! {
                _ = tokio::time::sleep_until(cap_at) => break Ending::Capped,
                changed = stop.changed(), if stoppable => {
                    match changed.map(|()| stop.borrow().clone()) {
                        Ok(Some(run_id)) => break Ending::NextRun { run_id },
                        Ok(None) => (),
                        Err(_) => stoppable = false,
                    }
                }
                _ = ticks.tick() => {
                    let members = sample(self.group).await;
                    if members.is_empty() {
                        break Ending::Finished;
                    }
                    if self.sampled.add(&members) {
                        (self.report)(running(self.since, self.cap, self.group, &self.sampled.recorded));
                    }
                    #[cfg(test)]
                    if let Some(observed) = &self.observed_cpu {
                        observed.send_replace(self.sampled.cpu());
                    }
                }
            }
        };
        if ending != Ending::Finished {
            self.stop().await;
        }
        Leftovers::Ended {
            since: self.since,
            cap_ms: millis(self.cap),
            duration_ms: Some(millis(self.exited.elapsed())),
            cpu_ms: Some(millis(self.sampled.cpu())),
            ending,
        }
    }

    /// A termination signal to the whole group, then a kill for what remains
    /// after the grace period. Each signal follows a sample that found the
    /// leftovers still there, so an ID reused since they ended is never signalled.
    async fn stop(&mut self) {
        for (signal, wait) in [(libc::SIGTERM, STOP_GRACE), (libc::SIGKILL, KILL_WAIT)] {
            if !self.signal(signal) {
                return;
            }
            let deadline = Instant::now() + wait;
            loop {
                tokio::time::sleep(Duration::from_millis(50)).await;
                let members = sample(self.group).await;
                if members.is_empty() {
                    return;
                }
                self.sampled.add(&members);
                if Instant::now() >= deadline {
                    break;
                }
            }
        }
    }

    /// Signals the group if its leftovers are still in it; false when they ended.
    fn signal(&self, signal: i32) -> bool {
        let group = self.group;
        tokio::task::block_in_place(|| {
            let present = !ours(group, process_group::members(group)).is_empty();
            if present {
                process_group::signal(group, signal);
            }
            present
        })
    }
}

/// Reads the group off the async runtime: it reads every process of it.
async fn sample(group: u32) -> Vec<Member> {
    tokio::task::spawn_blocking(move || ours(group, process_group::members(group)))
        .await
        .unwrap_or_default()
}

impl Watch {
    pub fn has_ended(&self) -> bool {
        *self.ended.borrow()
    }

    async fn until_ended(mut ended: watch::Receiver<bool>) {
        let _ = ended.wait_for(|ended| *ended).await;
    }
}

impl Lingering for Watch {
    fn ended(&self) -> bool {
        self.has_ended()
    }

    fn settle(&self, wait: Duration, next_run: &str) -> BoxFuture<'static, ()> {
        let stop = self.stop.clone();
        let ended = self.ended.clone();
        let next_run = next_run.to_owned();
        Box::pin(async move {
            if tokio::time::timeout(wait, Watch::until_ended(ended.clone()))
                .await
                .is_err()
            {
                stop.send_replace(Some(next_run));
                Watch::until_ended(ended).await;
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{os::unix::process::CommandExt, sync::Mutex};

    /// Collects what a watch reports.
    #[derive(Clone, Default)]
    struct Reports(Arc<Mutex<Vec<Leftovers>>>);

    impl Reports {
        fn report(&self) -> Report {
            let reports = self.clone();
            Box::new(move |leftovers| reports.0.lock().unwrap().push(leftovers))
        }
        fn last(&self) -> Leftovers {
            self.0.lock().unwrap().last().cloned().unwrap()
        }
    }

    /// Starts `script` as a group leader and waits for it to exit.
    fn exited(script: &str) -> u32 {
        let mut leader = std::process::Command::new("/bin/sh")
            .args(["-c", script])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();
        leader.wait().unwrap();
        leader.id()
    }

    fn member(pid: u32, parent: u32, cpu_ms: u64) -> Member {
        Member {
            identity: ProcessIdentity {
                pid,
                started_us: u64::from(pid),
            },
            parent,
            cpu: Duration::from_millis(cpu_ms),
        }
    }

    #[test]
    fn a_reused_group_id_is_never_taken_for_the_leftovers() {
        let group = 500;
        assert_eq!(ours(group, vec![member(501, 1, 0)]).len(), 1);
        assert!(
            ours(group, vec![member(501, 1, 0), member(group, 1, 0)]).is_empty(),
            "the command was reaped: a process with its PID leads a new group"
        );
    }

    #[test]
    fn cpu_time_counts_after_the_exit_and_waited_children_once() {
        // At the exit, 10 used 100 ms already.
        let mut sampled = Sampled::new(&[member(10, 1, 100)]);
        assert_eq!(sampled.recorded.len(), 1);
        // 11, its child, starts and works; then 10 reaps it, which adds all of
        // 11's time to 10's.
        assert!(!sampled.add(&[member(10, 1, 150), member(11, 10, 300)]));
        assert_eq!(sampled.cpu(), Duration::from_millis(50 + 300));
        sampled.add(&[member(10, 1, 150 + 400)]);
        assert_eq!(sampled.cpu(), Duration::from_millis(450), "11's 400 once");
        // 12, started by 10, is left to launchd when 10 ends: its time stays.
        sampled.add(&[member(10, 1, 550), member(12, 10, 200)]);
        assert!(sampled.add(&[member(12, 1, 250)]), "the recorded 10 ended");
        assert_eq!(sampled.recorded[0].pid, 12);
        sampled.add(&[member(13, 1, 0)]);
        assert_eq!(sampled.cpu(), Duration::from_millis(450 + 250));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_command_that_leaves_nothing_has_no_leftovers() {
        let reports = Reports::default();
        let group = exited("true");
        assert!(
            watch(
                group,
                Cap(Duration::from_secs(5)),
                reports.report(),
                Vec::new()
            )
            .is_none()
        );
        assert!(reports.0.lock().unwrap().is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn leftovers_finish_on_their_own_with_their_cpu_time() {
        let reports = Reports::default();
        // Hold the orphan at the process boundary. Fixed CPU work, not a
        // wall-clock loop, establishes what its descendants actually used.
        let mut leader = std::process::Command::new("/bin/sh")
            .args(["-c", "(read go; /bin/sh -c 'i=0; while [ $i -lt 600000 ]; do i=$((i+1)); done'; read finish) <&0 &"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .process_group(0)
            .spawn().unwrap();
        let group = leader.id();
        let mut input = leader.stdin.take().unwrap();
        leader.wait().unwrap();
        let first = sample(group).await;
        let (observed, mut cpu) = watch::channel(Duration::ZERO);
        let mut supervised = Supervised {
            group,
            since: Timestamp::now(),
            cap: Duration::from_secs(120),
            exited: Instant::now(),
            report: reports.report(),
            sampled: Sampled::new(&first),
            observed_cpu: Some(observed),
        };
        let (_stop, receiver) = watch::channel(None);
        let watching = tokio::spawn(async move {
            let ended = supervised.supervise(receiver).await;
            (supervised.report)(ended);
        });
        use std::io::Write;
        input.write_all(b"go\n").unwrap();
        // The live supervisor itself must observe the CPU budget. Removing its
        // sampling makes this bounded wait fail, even if the child did its work.
        tokio::time::timeout(Duration::from_secs(120), async {
            while *cpu.borrow_and_update() < Duration::from_millis(200) {
                cpu.changed().await.expect("the supervisor still runs");
            }
        })
        .await
        .expect("the supervisor observes the child's CPU budget");
        input.write_all(b"finish\n").unwrap();
        drop(input);
        tokio::time::timeout(Duration::from_secs(120), watching)
            .await
            .expect("the orphan finishes")
            .unwrap();
        assert!(process_group::members(group).is_empty());
        let Leftovers::Ended {
            ending,
            duration_ms,
            cpu_ms,
            ..
        } = reports.last()
        else {
            panic!("{:?}", reports.last())
        };
        assert_eq!(ending, Ending::Finished);
        assert!(duration_ms.unwrap() > 0);
        assert!(
            cpu_ms.unwrap() >= 200,
            "the loop's and its children's time after the exit: {cpu_ms:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn leftovers_are_stopped_at_the_cap_even_when_they_ignore_termination() {
        let reports = Reports::default();
        let gate = tempfile::tempdir().unwrap();
        let ready = gate.path().join("ready");
        let group = exited(&format!(
            "(trap '' TERM; touch '{}'; while [ -d '{}' ]; do sleep 1; done) &",
            ready.display(),
            gate.path().display()
        ));
        let deadline = Instant::now() + Duration::from_secs(120);
        while !ready.exists() {
            assert!(
                Instant::now() < deadline,
                "the TERM-ignore trap was never ready"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let started = Instant::now();
        let leftovers = watch(
            group,
            Cap(Duration::from_millis(300)),
            reports.report(),
            Vec::new(),
        )
        .unwrap();
        Watch::until_ended(leftovers.ended.clone()).await;
        assert!(started.elapsed() >= STOP_GRACE, "the grace period first");
        assert!(process_group::members(group).is_empty());
        assert!(reports.last().capped(), "{:?}", reports.last());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_next_run_waits_for_leftovers_then_stops_them() {
        let reports = Reports::default();
        let gate = tempfile::tempdir().unwrap();
        let ready = gate.path().join("ready");
        let term = gate.path().join("term");
        let group = exited(&format!(
            "(trap 'touch {}; exit 0' TERM; touch {}; while [ -d '{}' ]; do sleep 1; done) &",
            term.display(),
            ready.display(),
            gate.path().display()
        ));
        let deadline = Instant::now() + Duration::from_secs(120);
        while !ready.exists() {
            assert!(
                Instant::now() < deadline,
                "the TERM handler was never ready"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let leftovers = watch(
            group,
            Cap(Duration::from_secs(300)),
            reports.report(),
            Vec::new(),
        )
        .unwrap();
        let started = Instant::now();
        tokio::time::timeout(
            Duration::from_secs(120),
            leftovers.settle(Duration::from_secs(1), "r-000000000002"),
        )
        .await
        .expect("leftovers stop");
        assert!(started.elapsed() >= Duration::from_secs(1));
        assert!(term.exists(), "the process handled TERM before it ended");
        assert!(leftovers.ended());
        assert!(process_group::members(group).is_empty());
        let Leftovers::Ended { ending, .. } = reports.last() else {
            panic!()
        };
        assert_eq!(
            ending,
            Ending::NextRun {
                run_id: "r-000000000002".into()
            }
        );

        // Leftovers that end within the wait are not stopped.
        let reports = Reports::default();
        let mut leader = std::process::Command::new("/bin/sh")
            .args(["-c", "(read finish) <&0 &"])
            .stdin(std::process::Stdio::piped())
            .process_group(0)
            .spawn()
            .unwrap();
        let group = leader.id();
        let input = leader.stdin.take().unwrap();
        leader.wait().unwrap();
        let leftovers = watch(
            group,
            Cap(Duration::from_secs(300)),
            reports.report(),
            Vec::new(),
        )
        .unwrap();
        drop(input);
        tokio::time::timeout(
            Duration::from_secs(120),
            leftovers.settle(Duration::from_secs(120), "r-000000000003"),
        )
        .await
        .expect("leftovers finish");
        assert!(leftovers.ended());
        let Leftovers::Ended { ending, .. } = reports.last() else {
            panic!()
        };
        assert_eq!(
            ending,
            Ending::Finished,
            "they ended before the wait ran out"
        );
    }
}
