//! Follows this member's gate each second, samples processes while it stays
//! closed, and keeps the blocked state and its episodes.
use crate::{
    failure::Result,
    outside_pressure::{
        attribution::{self, OwnWork},
        blocked::Blocked,
        episode::{Change, Step, Watch},
        sampling::{ProcessSampler, Snapshot, usage_between},
        store::{Episode, EpisodeStore},
    },
    scheduling::{load::Gate, timestamp::Timestamp},
};
use serde::Serialize;
use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

/// How often ended episodes past retention are deleted while running.
const PRUNE_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// What the gate's owner says each second.
#[derive(Clone, Debug, PartialEq)]
pub struct Observation {
    pub at: Timestamp,
    pub gate: Gate,
    /// What identifies Elsecall's own work among the machine's processes.
    pub own: OwnWork,
}

/// Where the detector reads the gate; the scheduler in production.
pub trait GateSource: Send + Sync {
    fn observe(&self) -> Observation;
}

/// Where other modules read this member's blocked state.
pub trait BlockedReport: Send + Sync {
    fn blocked(&self) -> Option<Blocked>;
}

pub struct Detector {
    sampler: Arc<dyn ProcessSampler>,
    store: EpisodeStore,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    watch: Watch,
    /// The latest process sample of this closure, the baseline for CPU rates.
    previous: Option<Snapshot>,
    /// The episode the previous service run left open, until the first
    /// observation decides whether it still lasts.
    interrupted: Option<Blocked>,
    pruned: Option<Timestamp>,
}

fn log(event: &str, detail: impl Serialize) {
    crate::service_log!("{}", serde_json::json!({"event": event, "detail": detail}));
}

impl Detector {
    pub fn open(path: &Path, sampler: Arc<dyn ProcessSampler>) -> Result<Self> {
        let (store, interrupted) = EpisodeStore::open(path)?;
        Ok(Self {
            sampler,
            store,
            state: Mutex::new(State {
                interrupted,
                ..State::default()
            }),
        })
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Takes in one second. Sampling processes happens outside the lock, so
    /// readers of the blocked state never wait on it.
    pub fn observe(&self, observation: Observation) {
        let at = observation.at;
        let step = {
            let mut state = self.state();
            // The first seconds after a start can read the gate as unknown or
            // open before the scheduler has a load window, so the interrupted
            // episode waits for a closed observation until it is too old.
            let resumed = match state.interrupted.take() {
                Some(previous) if Watch::resumable(&previous, at) => {
                    let resumed = Watch::resumed(&previous, at, observation.gate);
                    if resumed.is_none() {
                        state.interrupted = Some(previous);
                    }
                    resumed
                }
                _ => None,
            };
            let step = match resumed {
                // Attribute at once, so the block shows again without waiting a
                // minute for a CPU baseline; rates are since each process started
                // until the next attribution.
                Some(watch) => {
                    state.watch = watch;
                    Step::Attribute
                }
                None => state.watch.observe(at, observation.gate),
            };
            if state.watch == Watch::Open {
                state.previous = None;
            }
            step
        };
        match step {
            Step::Nothing => (),
            Step::Ended { blocked, until } => self.ended(&blocked, until),
            Step::Sample => {
                if let Some(snapshot) = self.sample(at) {
                    self.state().previous = Some(snapshot);
                }
            }
            Step::Attribute => {
                let Some(current) = self.sample(at) else {
                    return self.prune_when_due(at);
                };
                let change = {
                    let mut state = self.state();
                    let usage = usage_between(state.previous.as_ref(), &current);
                    let reason = match &state.watch {
                        Watch::Closed { reason, .. } => reason.clone(),
                        Watch::Blocked { blocked, .. } => blocked.reason.clone(),
                        Watch::Open => return self.prune_when_due(at),
                    };
                    let attribution = attribution::attribute(&reason, &usage, &observation.own);
                    state.previous = Some(current);
                    state.watch.attributed(at, attribution)
                };
                self.changed(change);
            }
        }
        self.prune_when_due(at);
    }

    fn sample(&self, at: Timestamp) -> Option<Snapshot> {
        match self.sampler.sample() {
            Ok(processes) => Some(Snapshot { at, processes }),
            Err(error) => {
                log("outside_pressure_sample_failed", &error.code);
                None
            }
        }
    }

    fn changed(&self, change: Change) {
        match change {
            Change::Unchanged => (),
            Change::Started(blocked) => {
                log(
                    "outside_pressure_blocked",
                    serde_json::json!({
                        "reason": &blocked.reason,
                        "since": blocked.since,
                        "leading": blocked.leading().map(|p| &p.name),
                    }),
                );
                self.record(&blocked);
            }
            Change::Updated(blocked) => self.record(&blocked),
            Change::Ended { blocked, until } => self.ended(&blocked, until),
        }
    }

    fn record(&self, blocked: &Blocked) {
        if let Err(error) = self.store.record(blocked) {
            log("outside_pressure_record_failed", &error.code);
        }
    }

    fn ended(&self, blocked: &Blocked, until: Timestamp) {
        log(
            "outside_pressure_cleared",
            serde_json::json!({"since": blocked.since, "until": until}),
        );
        if let Err(error) = self.store.end(blocked.since, until) {
            log("outside_pressure_record_failed", &error.code);
        }
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
            log("outside_pressure_prune_failed", &error.code);
        }
    }

    /// Episodes that overlap `since` to before `until`, oldest first.
    pub fn episodes(&self, since: Timestamp, until: Timestamp) -> Result<Vec<Episode>> {
        self.store.between(since, until)
    }
}

impl BlockedReport for Detector {
    fn blocked(&self) -> Option<Blocked> {
        self.state().watch.blocked().cloned()
    }
}

/// Feeds the detector from `source` every `interval` until the task is dropped.
pub async fn follow(detector: Arc<Detector>, source: Arc<dyn GateSource>, interval: Duration) {
    let mut ticks = tokio::time::interval(interval);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticks.tick().await;
        let observation = source.observe();
        let detector = detector.clone();
        // Sampling spawns ps; keep it off the runtime's workers.
        if tokio::task::spawn_blocking(move || detector.observe(observation))
            .await
            .is_err()
        {
            return;
        }
    }
}
