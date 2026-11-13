//! Hydration of already-admitted state at a system boundary. This follows the
//! same Admission adoption used for restart recovery, without making another
//! scheduling decision under a later load gate or candidate configuration.
use super::{Entry, Phase, Scheduler};
use crate::{
    failure::{Failure, Result},
    scheduling::{
        messages::Grant,
        placed::{PlacedGrant, Side},
        processes,
        record::{Record, State},
    },
};
use std::{collections::BTreeSet, time::Duration};

impl Scheduler {
    /// Restores a trusted active snapshot whose processes already exist in the
    /// supplied process observer. Restore local capacity roots before their
    /// nested requests, and restore both sides of a remotely hosted run.
    /// Historical size and timestamps remain fixed even when a candidate would
    /// refuse this work today; it was admitted before the evaluation window.
    pub fn restore_active(&self, record: Record, expected_run: Option<Duration>) -> Result<Grant> {
        let invalid = |message| {
            Failure::new(
                "initial_state_missing",
                message,
                "Export the admitted grant and its complete active capacity family at the trace boundary.",
            )
        };
        let Some(started) = record.started_at else {
            return Err(invalid(
                "An initial active snapshot has no admission timestamp.",
            ));
        };
        if record.state != State::Running
            || record.ended_at.is_some()
            || record.outcome.is_some()
            || started > self.clock.now()
        {
            return Err(invalid(
                "The initial snapshot does not describe already-active work.",
            ));
        }
        let member = record
            .member
            .clone()
            .ok_or_else(|| invalid("An initial active snapshot has no execution member."))?;
        let caller_side = record
            .placed
            .as_ref()
            .is_some_and(|p| p.side == Side::Caller);
        let hosted = record
            .placed
            .as_ref()
            .is_some_and(|p| p.side == Side::Member);
        if caller_side && record.placed.as_ref().is_some_and(|p| p.copy.is_none()) {
            return Err(invalid(
                "An initial remote caller snapshot has no copy identity.",
            ));
        }
        if !caller_side && member != self.name() {
            return Err(invalid(
                "The initial execution snapshot belongs to another member.",
            ));
        }
        let holders: BTreeSet<_> = record.processes.iter().copied().collect();
        if holders
            .iter()
            .any(|process| !processes::alive(&*self.processes, *process))
        {
            return Err(invalid(
                "An initial snapshot's holder is missing from process observation.",
            ));
        }
        let mut book = self.book();
        if book.entries.contains_key(&record.run_id) || self.records.get(&record.run_id)?.is_some()
        {
            return Err(invalid(
                "An initial run ID was restored more than once on this member.",
            ));
        }
        if !caller_side
            && let Some(root) = &record.shares_capacity_of
            && !book.admission.is_live(root)
        {
            return Err(invalid(
                "An initial nested request's outer capacity grant is missing.",
            ));
        }
        let grant = Grant {
            run_id: record.run_id.clone(),
            member,
            size: record.size,
            nested_in: record.nested_in.clone(),
            waited_ms: record
                .waited_ms
                .unwrap_or_else(|| started.since(record.requested_at).as_millis() as u64),
            placement_note: record.placement_note.clone(),
            placed: record
                .placed
                .as_ref()
                .filter(|p| p.side == Side::Caller)
                .map(|p| PlacedGrant {
                    start_by: record
                        .requested_at
                        .after(record.hold_limit.map_or(Duration::ZERO, |h| h.seconds)),
                    copy: p.copy.clone().expect("caller snapshot has a copy identity"),
                    calling_member: record.calling_member.clone(),
                    synced_paths: Vec::new(),
                }),
            reused: false,
        };
        self.records.save(&record)?;
        if !caller_side {
            if let Some(root) = &record.shares_capacity_of {
                book.admission.nest(record.run_id.clone(), root);
            } else {
                book.admission.adopt(record.run_id.clone(), record.size);
            }
        }
        let id = record.run_id.clone();
        book.entries.insert(
            id.clone(),
            Entry {
                record,
                expected_run,
                phase: Phase::Active {
                    holders: holders.clone(),
                },
                hosted,
                auto: None,
            },
        );
        for holder in holders {
            if !self.bind(&mut book, &id, holder) {
                return Err(invalid(
                    "An initial snapshot's holder exited during restoration.",
                ));
            }
        }
        Ok(grant)
    }
}
