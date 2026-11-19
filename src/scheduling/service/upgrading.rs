//! What an upgrade of this member needs from admission: the work running or queued
//! here, which of it a restart of the service would cut short, and holds that turn
//! away runs other members would place here and close admission for the install.
//!
//! **What outlasts a restart.** A local run or lease keeps running: its processes
//! are the caller's, and the next service adopts them by process identity. A
//! request still waiting for its decision is ended by the restart, but a caller
//! that asks again after a handover (the CLI does) is decided by the next service
//! instead; Elsecall's own queued passes are asked for again on the next service's
//! timer. A run placed between members is cut short: its connection goes through
//! this service, which stops the command it ran here, and so is an offer on its way
//! to a member, which could admit a run nobody attaches to. Elsecall's own running
//! pass is waited for too, as it may be midway through work a later pass would have
//! to finish. Processes placed commands left running are not waited for: the
//! restart stops them, which their cap (10 minutes by default) would have done later,
//! and the next run in that copy starts without them.
use super::{Entry, Phase, Scheduler};
use crate::scheduling::request::Kind;
use std::sync::Weak;

/// What a caller does when a service handover ends its request before the
/// decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OnHandover {
    /// It asks the next service again, so the request outlasts the handover.
    AsksAgain,
    /// It fails, as callers that predate asking again do.
    Fails,
}

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
    /// Whether restarting the service now would leave it whole.
    pub outlasts_restart: bool,
}

impl Entry {
    /// Whether restarting the service now would leave this work whole; see the
    /// module documentation.
    fn outlasts_restart(&self) -> bool {
        // The next service asks for Elsecall's own work again itself.
        let asked_again = self.record.internal || self.on_handover == OnHandover::AsksAgain;
        match &self.phase {
            Phase::Active { .. } => {
                !(self.hosted || self.record.placed.is_some() || self.record.internal)
            }
            Phase::Placing(placing) => placing.in_flight.is_none() && asked_again,
            Phase::Waiting(_) => asked_again,
        }
    }
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
            || book.update_held > 0
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
                        outlasts_restart: entry.outlasts_restart(),
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

/// Closes new admission, and stops starting queued work, until the installer
/// finishes.
pub struct UpdateHold(Weak<Scheduler>);
impl Drop for UpdateHold {
    fn drop(&mut self) {
        if let Some(scheduler) = self.0.upgrade() {
            let mut book = scheduler.book();
            book.update_held -= 1;
            scheduler.pump(&mut book);
        }
    }
}
impl Scheduler {
    /// Closes admission for an update once nothing in flight would be cut short by
    /// the restart; work that outlasts it may still run or queue.
    pub fn hold_update_when_restartable(&self) -> Option<UpdateHold> {
        let mut book = self.book();
        if book.stopping
            || book.update_held > 0
            || !book.entries.values().all(Entry::outlasts_restart)
        {
            return None;
        }
        book.update_held += 1;
        Some(UpdateHold(self.this.clone()))
    }
}
