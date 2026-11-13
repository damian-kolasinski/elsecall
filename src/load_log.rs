//! This member's load over time: one summary per minute of the load samples the
//! scheduler takes every second anyway, kept for [`store::RETENTION`]. It covers
//! time when no Elsecall work ran, so reports can tell outside load from
//! Elsecall's own. It holds machine state only: never processes, arguments,
//! environment values or paths.
//!
//! - `minute` folds per-second observations into a [`minute::Minute`];
//! - `store` keeps minutes in SQLite;
//! - `buckets` rolls minutes into aligned buckets and summaries for the fleet
//!   view, members' reports and the report.
pub mod buckets;
pub mod minute;
pub mod store;

use crate::{failure::Result, scheduling::timestamp::Timestamp};
use buckets::{Bucket, LoadSummary};
use minute::{Accumulator, Second};
use std::{path::Path, sync::Mutex, time::Duration};
use store::LoadStore;

/// The most buckets one request returns, so an answer fits a member response.
pub const MAXIMUM_BUCKETS: usize = 2000;

pub struct LoadLog {
    store: LoadStore,
    accumulator: Mutex<Accumulator>,
    rehearsal: Mutex<crate::rehearsal::load_recording::Recorder>,
}

impl LoadLog {
    /// Opens the history and deletes minutes past retention, as the service
    /// starts: a member that restarts or sleeps often may never run a day.
    pub fn open(path: &Path) -> Result<Self> {
        let store = LoadStore::open(path)?;
        store.prune(Timestamp::now())?;
        Ok(Self {
            store,
            accumulator: Mutex::new(Accumulator::default()),
            rehearsal: Mutex::default(),
        })
    }

    /// Takes in one second; writes the previous minute once it is complete.
    pub fn observe(&self, second: Second) -> Result<()> {
        self.rehearsal
            .lock()
            .expect("rehearsal load recorder")
            .observe(&second);
        let done = self
            .accumulator
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .add(second);
        match done {
            Some(minute) => self.store.append(&minute),
            None => Ok(()),
        }
    }

    /// Minutes from `since` to before `until`, oldest first, with the minute
    /// still being folded when it falls in that period.
    fn minutes(&self, since: Timestamp, until: Timestamp) -> Result<Vec<minute::Minute>> {
        let mut minutes = self.store.between(since, until)?;
        let current = self
            .accumulator
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .current();
        if let Some(current) = current.filter(|m| m.at >= since && m.at < until)
            && minutes.last().is_none_or(|last| last.at < current.at)
        {
            minutes.push(current);
        }
        Ok(minutes)
    }

    /// The last hour and day, for the fleet view, up to this second.
    pub fn summary(&self, now: Timestamp) -> Result<LoadSummary> {
        let since = Timestamp::from_millis(now.millis().saturating_sub(86_400_000));
        let until = Timestamp::from_millis(now.millis() + 60_000);
        Ok(buckets::summary(&self.minutes(since, until)?, now))
    }

    /// Buckets of `width` from `since` to before `until`, oldest first, at most
    /// [`MAXIMUM_BUCKETS`]: a longer period gets wider buckets.
    pub fn buckets(
        &self,
        since: Timestamp,
        until: Timestamp,
        width: Duration,
    ) -> Result<Vec<Bucket>> {
        let span = until.millis().saturating_sub(since.millis());
        let narrowest = span.div_ceil(MAXIMUM_BUCKETS as u64);
        let width = Duration::from_millis((width.as_millis() as u64).max(narrowest));
        Ok(buckets::buckets(&self.minutes(since, until)?, width))
    }

    pub fn rehearsal_observations_ready(&self) -> bool {
        self.rehearsal
            .lock()
            .expect("rehearsal load recorder")
            .ready(Timestamp::now())
    }
    pub fn begin_rehearsal(&self, session: String) {
        self.rehearsal
            .lock()
            .expect("rehearsal load recorder")
            .begin(session);
    }
    pub fn rehearsal(
        &self,
        session: &str,
        since: Timestamp,
        until: Timestamp,
    ) -> Result<crate::rehearsal::load_recording::Recorded> {
        self.rehearsal
            .lock()
            .expect("rehearsal load recorder")
            .read(session, since, until)
    }

    pub fn prune(&self, now: Timestamp) -> Result<()> {
        self.store.prune(now)
    }
}

/// What other modules may read of a member's load history.
pub trait LoadReport: Send + Sync {
    /// The last hour and day, up to this second.
    fn summary(&self, now: Timestamp) -> Result<LoadSummary>;
    /// Aligned buckets of the period, as [`LoadLog::buckets`].
    fn buckets(&self, since: Timestamp, until: Timestamp, width: Duration) -> Result<Vec<Bucket>>;
}

impl LoadReport for LoadLog {
    fn summary(&self, now: Timestamp) -> Result<LoadSummary> {
        LoadLog::summary(self, now)
    }
    fn buckets(&self, since: Timestamp, until: Timestamp, width: Duration) -> Result<Vec<Bucket>> {
        LoadLog::buckets(self, since, until, width)
    }
}
