//! Hold limits and deadlines of waiting requests.
use super::{Book, Entry, Phase, Scheduler, Waiting};
use crate::{
    failure::Failure,
    scheduling::{
        record::Verdict,
        request::{AtDeadline, Urgency},
        run_id::RunId,
        timestamp::Timestamp,
    },
};
use std::time::Duration;
use tokio::task::AbortHandle;

impl Scheduler {
    pub(super) fn timer(&self, id: &RunId, after: Duration) -> AbortHandle {
        let scheduler = self.this.clone();
        let id = id.clone();
        // Freeze the deadline before spawning: its first poll may be delayed.
        let until = tokio::time::Instant::now() + after;
        tokio::spawn(async move {
            tokio::time::sleep_until(until).await;
            if let Some(scheduler) = scheduler.upgrade() {
                let mut book = scheduler.book();
                scheduler.expire(&mut book, &id);
            }
        })
        .abort_handle()
    }

    /// A waiting request reached its hold limit or deadline.
    pub(super) fn expire(&self, book: &mut Book, id: &RunId) {
        if book.internal_paused.is_some()
            && book.entries.get(id).is_some_and(|e| {
                e.record.internal && matches!(e.record.urgency, Urgency::Deferrable { .. })
            })
        {
            return;
        }
        match book.entries.get(id) {
            Some(Entry {
                phase: Phase::Placing(_),
                ..
            }) => return self.expire_placing(book, id),
            Some(Entry {
                phase: Phase::Waiting(_),
                auto: Some(_),
                ..
            }) => return self.expire_auto(book, id),
            _ => (),
        }
        let Some(Entry {
            record,
            phase: Phase::Waiting(waiting),
            ..
        }) = book.entries.get_mut(id)
        else {
            return;
        };
        if waiting.expires_at.is_none() {
            return;
        }
        match record.urgency {
            Urgency::Deferrable {
                at_deadline: AtDeadline::Start,
                ..
            } => {
                waiting.expires_at = None;
                book.admission.promote(id);
                self.pump(book);
            }
            Urgency::Deferrable {
                at_deadline: AtDeadline::Skip,
                ..
            } => {
                let failure = Failure::new(
                    "deadline_skipped",
                    "The deadline passed before the member had room, and the request chose to skip.",
                    "Nothing ran. Request again later, or use --at-deadline start.",
                );
                self.fail(book, id, Verdict::DeadlineSkipped, failure);
            }
            Urgency::Interactive { .. } => {
                let failure = self.busy(book, id);
                self.fail(book, id, Verdict::FleetBusy, failure);
            }
        }
    }

    /// Expires waiting work on the wall clock. Timers measure time the machine is
    /// awake, so after sleep this catches hold limits and deadlines that passed.
    pub(super) fn expire_due(&self, book: &mut Book, now: Timestamp) {
        let mut due: Vec<(Timestamp, RunId)> = book
            .entries
            .iter()
            .filter(|(_, e)| match &e.phase {
                Phase::Waiting(Waiting {
                    expires_at: Some(at),
                    ..
                }) => *at <= now,
                Phase::Placing(placing) => placing.expires_at <= now,
                _ => false,
            })
            .map(|(id, e)| (e.record.requested_at, id.clone()))
            .collect();
        due.sort();
        for (_, id) in due {
            self.expire(book, &id);
        }
    }
}
