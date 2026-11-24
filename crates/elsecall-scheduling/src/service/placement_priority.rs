//! Queue priority on the master follows a request as it moves between members.
use super::{Book, Entry, Phase, Scheduler, placing::Placing};
use crate::{
    admission::{Admission, Class},
    placed::DeferredOffer,
    quote::QueuePriority,
};
use elsecall_contract::scheduling_model::{
    request::{AtDeadline, Urgency},
    run_id::RunId,
    timestamp::Timestamp,
};
use std::time::Duration;

impl Entry {
    pub(super) fn waiting_deadline(&self) -> Option<Timestamp> {
        match &self.phase {
            Phase::Placing(p) => p.expires_at,
            Phase::Waiting(w) => w.expires_at,
            Phase::Active { .. } => None,
        }
    }

    pub(super) fn deferred_offer(&self) -> Option<DeferredOffer> {
        match self.record.urgency {
            Urgency::Interactive { .. } => None,
            Urgency::Deferrable {
                deadline,
                at_deadline: AtDeadline::Skip,
            } => Some(DeferredOffer::SkipAt(deadline)),
            Urgency::Deferrable { deadline, .. } if self.waiting_deadline().is_some() => {
                Some(DeferredOffer::StartAt(deadline))
            }
            Urgency::Deferrable { deadline, .. } => Some(DeferredOffer::Promoted(deadline)),
        }
    }

    pub(super) fn queue_order(&self) -> (QueuePriority, Timestamp) {
        match self.deferred_offer() {
            Some(DeferredOffer::Promoted(deadline)) => (QueuePriority::Promoted, deadline),
            None => (QueuePriority::Interactive, self.record.requested_at),
            Some(_) => (QueuePriority::Deferrable, self.record.requested_at),
        }
    }

    pub(super) fn enqueue_local(&self, admission: &mut Admission) {
        let id = &self.record.run_id;
        let class = if self.deferred_offer().is_some() {
            Class::Deferrable
        } else {
            Class::Interactive
        };
        admission.enqueue(id.clone(), self.record.size, class);
        if matches!(self.deferred_offer(), Some(DeferredOffer::Promoted(_))) {
            admission.promote(id);
        }
    }

    /// Once granted, a deferred job has the ordinary bounded workspace-start window.
    pub(super) fn transfer_window(&self, placing: &Placing, now: Timestamp) -> Duration {
        match self.record.urgency {
            Urgency::Interactive { .. } => placing
                .expires_at
                .expect("interactive hold limit")
                .since(now),
            Urgency::Deferrable { .. } => {
                elsecall_contract::scheduling_model::request::MAXIMUM_HOLD_LIMIT
            }
        }
    }
}

impl Scheduler {
    pub(super) fn order_offers(book: &mut Book, target: &str) {
        if let Some(queue) = book.placing.get_mut(target) {
            queue
                .make_contiguous()
                .sort_by_key(|id| book.entries[id].queue_order());
        }
    }

    pub(super) fn due_placement(&self, book: &Book, id: &RunId) -> bool {
        book.entries.get(id).is_some_and(|e| {
            matches!(e.record.urgency, Urgency::Deferrable { .. })
                && e.waiting_deadline()
                    .is_some_and(|at| at <= self.clock.now())
        })
    }

    pub(super) fn higher_priority_waits(
        book: &Book,
        target: &str,
        id: &RunId,
        tried: &[RunId],
    ) -> bool {
        let Some(entry) = book.entries.get(id) else {
            return false;
        };
        book.placing.get(target).is_some_and(|queue| {
            queue
                .iter()
                .take_while(|queued| *queued != id)
                .any(|queued| {
                    !tried.contains(queued)
                        && book.entries[queued].queue_order().0 < entry.queue_order().0
                })
        })
    }
}
