//! What an upgrade of this member needs from admission: the work running or queued
//! here, and a hold that turns away runs other members would place here.
use super::{Phase, Scheduler};
use crate::scheduling::request::Kind;
use std::sync::Weak;

/// One request running or queued on this member.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkInFlight {
    pub run_id: String,
    pub kind: Kind,
    pub label: Option<String>,
    pub running: bool,
    /// Whether another member placed it here.
    pub hosted: bool,
    /// Service-owned work, such as a housekeeping pass (the record's `internal`).
    pub internal: bool,
}

/// Refuses placed runs as busy until dropped.
pub struct HostingHold(Weak<Scheduler>);
impl Drop for HostingHold {
    fn drop(&mut self) {
        if let Some(scheduler) = self.0.upgrade() {
            let mut book = scheduler.book();
            book.hosting_held = book.hosting_held.saturating_sub(1);
        }
    }
}

impl Scheduler {
    /// Close admission and test idleness under the decision lock, so new work
    /// cannot race an automatic configuration restart. Queued work is preserved.
    pub fn stop_if_idle(&self) -> bool {
        let mut book = self.book();
        if book.stopping
            || book.hosting_held > 0
            || !book.entries.is_empty()
            || !book.lingering.is_empty()
        {
            return false;
        }
        book.stopping = true;
        true
    }

    /// Every request running or queued on this member, oldest first. A request this
    /// member is still placing on another one counts as queued: its caller waits on
    /// this service.
    pub fn in_flight(&self) -> Vec<WorkInFlight> {
        let book = self.book();
        let mut work: Vec<_> = book
            .entries
            .iter()
            .map(|(id, entry)| {
                let running = matches!(entry.phase, Phase::Active { .. });
                (
                    entry.record.requested_at,
                    WorkInFlight {
                        run_id: id.to_string(),
                        kind: entry.record.kind,
                        label: entry.record.label.clone(),
                        running,
                        hosted: entry.hosted,
                        internal: entry.record.internal,
                    },
                )
            })
            .collect();
        work.sort_by_key(|(at, w)| (*at, w.run_id.clone()));
        work.into_iter().map(|(_, w)| w).collect()
    }

    /// Whether runs other members would place here are being turned away.
    pub fn hosting_held(&self) -> bool {
        self.book().hosting_held > 0
    }

    /// Turns away runs other members would place here, as a stopping service does,
    /// until the hold is dropped.
    pub fn hold_hosting(&self) -> HostingHold {
        self.book().hosting_held += 1;
        HostingHold(self.this.clone())
    }
}
