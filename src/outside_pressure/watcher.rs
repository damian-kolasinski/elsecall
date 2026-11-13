//! Samples the machine's processes every [`trend::INTERVAL`], keeps the outside
//! candidates of each round, and raises and ends early warnings as the trends
//! say. It runs whether or not the gate is open: the point is to warn before an
//! outside process closes it.
use crate::{
    failure::Result,
    outside_pressure::{
        attribution::{self, OwnWork},
        detector::GateSource,
        sampling::{ProcessSampler, Snapshot, Usage, usage_between},
        trend::{self, Round, Seen},
        trend_store::TrendStore,
        warning::{Warning, WarningEpisode},
    },
    scheduling::timestamp::Timestamp,
};
use serde::Serialize;
use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

/// How often rounds and warnings past retention are deleted while running.
const PRUNE_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// A warning that holds again within this long of ending continues.
const LAPSE: Duration = Duration::from_secs(60 * 60);

/// Where other modules read this member's early warnings.
pub trait WarningReport: Send + Sync {
    /// Warnings that hold now, the longest-standing first.
    fn warnings(&self) -> Vec<Warning>;
    /// Warnings that held at some time from `since` to before `until`, oldest
    /// first.
    fn between(&self, since: Timestamp, until: Timestamp) -> Result<Vec<WarningEpisode>>;
}

pub struct Watcher {
    sampler: Arc<dyn ProcessSampler>,
    store: TrendStore,
    physical_memory: u64,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// The previous round's sample, the baseline for CPU rates.
    previous: Option<Snapshot>,
    current: Vec<Warning>,
    pruned: Option<Timestamp>,
}

fn log(event: &str, detail: impl Serialize) {
    crate::service_log!("{}", serde_json::json!({"event": event, "detail": detail}));
}

fn seen(usage: &Usage) -> Seen {
    let process = &usage.process;
    Seen {
        pid: process.pid,
        started: process.started,
        name: process.name.clone(),
        owner: process.owner,
        cpu_cores: usage.cpu_cores,
        memory_bytes: process
            .memory
            .footprint_bytes
            .unwrap_or(process.memory.resident_bytes),
    }
}

impl Watcher {
    /// `physical_memory` is the machine's, in bytes: memory concerns are
    /// shares of it.
    pub fn open(
        path: &Path,
        sampler: Arc<dyn ProcessSampler>,
        physical_memory: u64,
    ) -> Result<Self> {
        let store = TrendStore::open(path)?;
        // Until the first judged round, a start shows what the previous run
        // left open; that round confirms or ends each of them.
        let current = store.open_warnings()?;
        Ok(Self {
            sampler,
            store,
            physical_memory,
            state: Mutex::new(State {
                current,
                ..State::default()
            }),
        })
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Takes one round at `at`. The first round after a start only sets the
    /// baseline for CPU rates; rates since a process started would count a
    /// day-old daemon's whole life.
    pub fn watch(&self, at: Timestamp, own: &OwnWork) {
        let current = match self.sampler.sample() {
            Ok(processes) => Snapshot { at, processes },
            Err(error) => {
                // Without samples nothing is known to hold any more.
                self.state().current.clear();
                return log("outside_processes_sample_failed", &error.code);
            }
        };
        let Some(previous) = self.state().previous.replace(current.clone()) else {
            return;
        };
        let usage = usage_between(Some(&previous), &current);
        if let Err(error) = self.judge(at, &usage, own) {
            log("outside_processes_record_failed", &error.code);
        }
        self.prune_when_due(at);
    }

    /// Records the round's outside candidates and raises, keeps and ends
    /// warnings. A process that was ever Elsecall's own work stays so for its
    /// life: build daemons, compiler servers and VMs a grant started serve the
    /// grants after it too, outside its time.
    fn judge(&self, at: Timestamp, usage: &[Usage], own: &OwnWork) -> Result<()> {
        let identity = |u: &Usage| (u.process.pid, u.process.started);
        let own_now = attribution::own_processes(usage, own);
        let remembered = self.store.remember_own(
            &usage
                .iter()
                .filter(|u| own_now.contains(&u.process.pid))
                .map(identity)
                .collect::<Vec<_>>(),
            &usage.iter().map(identity).collect(),
        )?;
        let round = Round {
            at,
            seen: attribution::outside(usage, own)
                .into_iter()
                .filter(|u| !remembered.contains(&identity(u)))
                .map(seen)
                .filter(Seen::candidate)
                .collect(),
        };
        self.store.record(&round)?;
        let kept_since =
            Timestamp::from_millis(at.millis().saturating_sub(trend::KEPT.as_millis() as u64));
        let found = trend::concerns(&self.store.rounds(kept_since)?, self.physical_memory);
        let open = self.store.open_warnings()?;
        // A concern that lapsed for a round or two, as growth through a garbage
        // collection, continues its episode instead of starting another.
        let lapsed = self.store.between(
            Timestamp::from_millis(at.millis().saturating_sub(LAPSE.as_millis() as u64)),
            at,
        )?;
        let mut current = Vec::new();
        for mut warning in found {
            let continued = open
                .iter()
                .find(|earlier| earlier.same(&warning))
                .or_else(|| {
                    lapsed
                        .iter()
                        .map(|episode| &episode.warning)
                        .find(|earlier| earlier.same(&warning))
                });
            match continued {
                Some(earlier) => warning.since = earlier.since,
                None => log(
                    "outside_process_warning",
                    serde_json::json!({
                        "concern": warning.concern.kind(),
                        "process": &warning.process,
                        "pid": warning.pid,
                        "since": warning.since,
                    }),
                ),
            }
            self.store.raise(&warning)?;
            current.push(warning);
        }
        for earlier in open.iter().filter(|o| !current.iter().any(|w| w.same(o))) {
            log(
                "outside_process_warning_cleared",
                serde_json::json!({
                    "concern": earlier.concern.kind(),
                    "process": &earlier.process,
                    "pid": earlier.pid,
                    "until": earlier.observed_at,
                }),
            );
            self.store.end(earlier)?;
        }
        current.sort_by_key(|w| w.since);
        self.state().current = current;
        Ok(())
    }

    fn prune_when_due(&self, at: Timestamp) {
        let due = {
            let mut state = self.state();
            let due = state.pruned.is_none_or(|p| at.since(p) >= PRUNE_INTERVAL);
            if due {
                state.pruned = Some(at);
            }
            due
        };
        if due && let Err(error) = self.store.prune(at) {
            log("outside_processes_prune_failed", &error.code);
        }
    }
}

impl WarningReport for Watcher {
    fn warnings(&self) -> Vec<Warning> {
        self.state().current.clone()
    }

    fn between(&self, since: Timestamp, until: Timestamp) -> Result<Vec<WarningEpisode>> {
        self.store.between(since, until)
    }
}

/// Takes a round every [`trend::INTERVAL`], with Elsecall's own work as
/// `source` sees it then, until the task is dropped.
pub async fn follow(watcher: Arc<Watcher>, source: Arc<dyn GateSource>) {
    let mut ticks = tokio::time::interval(trend::INTERVAL);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticks.tick().await;
        let observation = source.observe();
        let watcher = watcher.clone();
        // Sampling spawns ps; keep it off the runtime's workers.
        if tokio::task::spawn_blocking(move || watcher.watch(observation.at, &observation.own))
            .await
            .is_err()
        {
            return;
        }
    }
}
