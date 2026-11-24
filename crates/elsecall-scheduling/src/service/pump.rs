//! Start local grants when capacity opens or queued work changes priority.
use super::{Book, Phase, Scheduler};
use crate::record::{Outcome, Verdict};
use elsecall_contract::scheduling_model::request::{AtDeadline, Urgency};
use std::collections::BTreeSet;

impl Scheduler {
    /// Starts every waiting request that may start now, in order.
    pub(super) fn pump(&self, book: &mut Book) {
        if book.update_held > 0 || book.pumping {
            return;
        }
        // Expiry and disconnected responders also ask for admission to run.
        // Let this loop drain the queue without nesting one call per expired job.
        book.pumping = true;
        let gate = self.gate(book);
        while let Some(id) = book.admission.ready(gate.admits()).cloned() {
            if self.due_placement(book, &id) {
                self.expire(book, &id);
                continue;
            }
            let reason = match book.entries[&id].record.urgency {
                Urgency::Deferrable {
                    deadline,
                    at_deadline: AtDeadline::Start,
                } if deadline <= self.clock.now() => {
                    "Deadline passed; started ahead of interactive work once it fit."
                }
                Urgency::Deferrable { .. } => "Capacity was free and no interactive work waited.",
                Urgency::Interactive { .. } => "Fit free capacity and was first in line.",
            };
            let entry = &book.entries[&id];
            let decision = if entry.auto.is_some() {
                let own = self.name();
                let reason = Self::start_reason(entry, &own);
                self.auto_decision(entry, Verdict::Started, Some(&own), &reason)
            } else {
                self.decision(book, entry, Verdict::Started, reason)
            };
            book.admission.start(&id);
            let mut entry = book.entries.remove(&id).expect("waiting work has an entry");
            entry.record.decision = Some(decision);
            let Phase::Waiting(waiting) = std::mem::replace(
                &mut entry.phase,
                Phase::Active {
                    holders: BTreeSet::new(),
                },
            ) else {
                unreachable!("only waiting work is queued")
            };
            entry.phase = Phase::Active {
                holders: BTreeSet::from([waiting.holder]),
            };
            if let Some(timer) = waiting.timer {
                timer.abort();
            }
            // A lease outlives the elsecall process that asked for it.
            if waiting.requester != waiting.holder {
                book.bindings.unbind(&id, waiting.requester);
            }
            let grant = self.begin(&mut entry, self.name(), None);
            book.entries.insert(id.clone(), entry);
            if waiting.responder.send(Ok(grant)).is_err() {
                self.end(
                    book,
                    &id,
                    Outcome::Cancelled {
                        reason: "The caller disconnected before the grant arrived.".into(),
                    },
                );
            }
        }
        book.pumping = false;
    }
}
