//! The scheduler's background tasks, for a scheduler the service drives itself:
//! sampling load each second and pruning old history daily, measuring this
//! member's speed now and then, following the exits of admitted processes, and
//! reconsidering where waiting work is placed. Each holds the scheduler weakly
//! and ends once it is gone; dropping [`Tasks`] stops them all.
use super::{SAMPLE_INTERVAL, Scheduler, auto, log};
use elsecall_scheduling_model::speed::{self, SpeedMeter};
use std::{
    sync::{Arc, Weak},
    time::Duration,
};
use tokio::{sync::mpsc, task::AbortHandle, time::Instant};

/// How often ended records past their retention are deleted.
const PRUNE_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// Background work of a running scheduler, stopped when dropped.
pub struct Tasks(Vec<AbortHandle>);

impl Tasks {
    /// No background work, for a scheduler its caller drives step by step.
    pub(super) fn none() -> Self {
        Tasks(Vec::new())
    }
}

impl Drop for Tasks {
    fn drop(&mut self) {
        for task in &self.0 {
            task.abort();
        }
    }
}

/// Starts every background task of `scheduler`.
pub(super) fn start(
    scheduler: &Arc<Scheduler>,
    exits: mpsc::UnboundedReceiver<u32>,
    meter: Arc<dyn SpeedMeter>,
) -> Tasks {
    Tasks(vec![
        tokio::spawn(sample(Arc::downgrade(scheduler))).abort_handle(),
        tokio::spawn(follow_exits(Arc::downgrade(scheduler), exits)).abort_handle(),
        tokio::spawn(measure_speed(Arc::downgrade(scheduler), meter)).abort_handle(),
        tokio::spawn(auto::reconsider(Arc::downgrade(scheduler))).abort_handle(),
    ])
}

async fn sample(scheduler: Weak<Scheduler>) {
    let mut interval = tokio::time::interval(SAMPLE_INTERVAL);
    // After a stall, sample on, rather than several times in one second.
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut pruned = Instant::now();
    loop {
        interval.tick().await;
        let Some(scheduler) = scheduler.upgrade() else {
            return;
        };
        if let Err(error) = scheduler.tick() {
            log("load_history_write_failed", &error);
        }
        if pruned.elapsed() >= PRUNE_INTERVAL {
            pruned = Instant::now();
            if let Err(error) = scheduler.records.prune(scheduler.clock.now()) {
                log("record_prune_failed", &error);
            }
            if let Err(error) = scheduler.load_log.prune(scheduler.clock.now()) {
                log("load_history_prune_failed", &error);
            }
        }
    }
}

/// Measures this member's speed now and then. The workload is short and runs on
/// a blocking thread, so admission never waits for it.
async fn measure_speed(scheduler: Weak<Scheduler>, meter: Arc<dyn SpeedMeter>) {
    let mut interval = tokio::time::interval(speed::MEASURE_EVERY);
    loop {
        interval.tick().await;
        let meter = meter.clone();
        let measured = tokio::task::spawn_blocking(move || meter.measure()).await;
        let Some(scheduler) = scheduler.upgrade() else {
            return;
        };
        match measured {
            Ok(Ok(speed)) => scheduler
                .speed
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .record(speed),
            Ok(Err(error)) => log("speed_unavailable", &error),
            Err(_) => log("speed_unavailable", "the measurement panicked"),
        }
    }
}

async fn follow_exits(scheduler: Weak<Scheduler>, mut exits: mpsc::UnboundedReceiver<u32>) {
    while let Some(pid) = exits.recv().await {
        let Some(scheduler) = scheduler.upgrade() else {
            return;
        };
        scheduler.exited(pid);
    }
}
