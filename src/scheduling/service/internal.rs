//! Keep service-owned queued work from starting: for a bounded rehearsal (a
//! pause, which also extends the work's deadlines by the time it was paused), or
//! while this member drains for an upgrade (a hold). Neither owns capacity, and
//! production admission continues normally. A running pass drains.
use super::{Book, Class, Phase, Scheduler, Urgency};
use crate::{failure::Result, scheduling::run_id::RunId};
use std::sync::{Arc, Weak};

pub struct InternalPause(Arc<Scheduler>);
impl Drop for InternalPause {
    fn drop(&mut self) {
        self.0.resume_internal();
    }
}

/// Keeps Elsecall's own queued work from starting until dropped.
pub struct InternalHold(Weak<Scheduler>);
impl Drop for InternalHold {
    fn drop(&mut self) {
        if let Some(scheduler) = self.0.upgrade() {
            let mut book = scheduler.book();
            book.internal_held = book.internal_held.saturating_sub(1);
            if !book.internal_withheld() {
                scheduler.requeue_internal(&mut book);
                scheduler.pump(&mut book);
            }
        }
    }
}

impl Book {
    /// Whether Elsecall's own queued work is kept out of admission.
    pub(super) fn internal_withheld(&self) -> bool {
        self.internal_paused.is_some() || self.internal_held > 0
    }

    /// Elsecall's own deferrable work still waiting here, oldest first.
    fn waiting_internal(&self) -> Vec<RunId> {
        let mut waiting: Vec<_> = self
            .entries
            .iter()
            .filter(|(_, e)| {
                e.record.internal
                    && matches!(e.record.urgency, Urgency::Deferrable { .. })
                    && matches!(e.phase, Phase::Waiting(_))
            })
            .map(|(id, e)| (e.record.requested_at, id.clone()))
            .collect();
        waiting.sort();
        waiting.into_iter().map(|(_, id)| id).collect()
    }

    fn withdraw_internal(&mut self) {
        for id in self.waiting_internal() {
            self.admission.withdraw(&id);
        }
    }
}

impl Scheduler {
    /// Already-running passes drain within preparation's original deadline.
    pub async fn drain_internal(&self, expires: tokio::time::Instant) -> Result<()> {
        let drain = (tokio::time::Instant::now() + std::time::Duration::from_secs(20)).min(expires);
        while self.internal_active() {
            if tokio::time::Instant::now() >= drain {
                return Err(crate::rehearsal::invalid(
                    "Internal work is still draining; retry the rehearsal.",
                ));
            }
            tokio::time::sleep_until(
                (tokio::time::Instant::now() + std::time::Duration::from_millis(20)).min(drain),
            )
            .await;
        }
        if tokio::time::Instant::now() >= expires {
            return Err(crate::rehearsal::invalid(
                "Rehearsal preparation reached its execution limit.",
            ));
        }
        Ok(())
    }

    pub fn pause_internal(self: &Arc<Self>) -> Result<InternalPause> {
        let mut book = self.book();
        if book.internal_paused.is_some() {
            return Err(crate::rehearsal::invalid(
                "Internal work is already paused.",
            ));
        }
        book.internal_paused = Some(self.clock.now());
        book.withdraw_internal();
        Ok(InternalPause(self.clone()))
    }

    /// Keeps Elsecall's own queued work from starting, so nothing of its own
    /// starts while this member drains for an upgrade and is then cut short by the
    /// restart. Its deadlines still apply.
    pub fn hold_internal(&self) -> InternalHold {
        let mut book = self.book();
        book.internal_held += 1;
        book.withdraw_internal();
        InternalHold(self.this.clone())
    }

    pub fn internal_active(&self) -> bool {
        self.book()
            .entries
            .values()
            .any(|e| e.record.internal && matches!(e.phase, Phase::Active { .. }))
    }

    fn resume_internal(&self) {
        let mut book = self.book();
        let Some(paused) = book.internal_paused.take() else {
            return;
        };
        let now = self.clock.now();
        for id in book.waiting_internal() {
            let entry = book.entries.get_mut(&id).expect("listed entry");
            let extension = now.since(paused.max(entry.record.requested_at));
            let Phase::Waiting(waiting) = &mut entry.phase else {
                unreachable!()
            };
            if let Some(at) = waiting.expires_at {
                let at = at.after(extension);
                waiting.expires_at = Some(at);
                if let Some(timer) = waiting.timer.take() {
                    timer.abort();
                }
                waiting.timer = Some(self.timer(&id, at.since(now)));
                if let Urgency::Deferrable { deadline, .. } = &mut entry.record.urgency {
                    *deadline = at;
                }
            }
            entry.record.warnings.push(format!(
                "Internal deadline extended by {} ms for rehearsal isolation.",
                extension.as_millis()
            ));
            self.persist(entry);
        }
        if !book.internal_withheld() {
            self.requeue_internal(&mut book);
        }
        self.pump(&mut book);
    }

    /// Returns Elsecall's own waiting work to admission, oldest first; work whose
    /// deadline passed meanwhile and chose to start goes ahead of interactive work.
    fn requeue_internal(&self, book: &mut Book) {
        for id in book.waiting_internal() {
            let entry = &book.entries[&id];
            let Phase::Waiting(waiting) = &entry.phase else {
                unreachable!()
            };
            let promoted = waiting.expires_at.is_none();
            let size = entry.record.size;
            book.admission.enqueue(id.clone(), size, Class::Deferrable);
            if promoted {
                book.admission.promote(&id);
            }
        }
    }
}
