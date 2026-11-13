//! Runs another member placed here. The member admits a placed run at once or
//! refuses it, so the request can wait on its master; it refuses while its own
//! interactive requests wait. Its own deferrable work, which waits behind
//! interactive work, never keeps placed work out. An admitted run keeps its
//! capacity until its runner ends it.
use super::{Entry, Phase, Scheduler};
use crate::{
    failure::{Failure, Result},
    scheduling::{
        capacity::Cores,
        leftovers::Leftovers,
        placed::{OfferReply, PlacedRecord, PlacementOffer, Reached, Side, SyncSummary, Warmth},
        processes::ProcessIdentity,
        quote::{self, HeldWork, Quote, WaitingWork},
        record::{
            Decision, HoldBasis, HoldLimitRecord, Outcome, Record, SizeBasis, State, Verdict,
        },
        request::{self, Kind, Placement, Urgency},
        run_id::RunId,
        run_phases::{RemoteFailurePhase, RunPhases},
    },
};
use std::{collections::BTreeSet, time::Duration};

/// How a hosted run ended, as its runner saw it.
pub enum HostedEnd {
    Exited {
        code: i32,
        cpu_ms: Option<u64>,
    },
    Signaled {
        signal: i32,
        cpu_ms: Option<u64>,
    },
    Failed {
        code: String,
        reached: Reached,
        exit: Option<HostedExit>,
    },
}
#[derive(Clone, Copy)]
pub enum HostedExit {
    Code(i32),
    Signal(i32),
}

impl Scheduler {
    /// Considers a run `from` (this member's master) offers. `copy_free` says
    /// whether its workspace's copy is free here.
    pub fn accept_placed(
        &self,
        from: &str,
        offer: &PlacementOffer,
        copy_free: bool,
        copy: &str,
        location: &str,
        warmth: Warmth,
    ) -> Result<OfferReply> {
        offer.workspace.validate()?;
        if let Some(label) = &offer.label
            && (label.is_empty()
                || label.chars().count() > 200
                || label.chars().any(char::is_control))
        {
            return Err(request::invalid("The offered label is invalid."));
        }
        if offer.size == Cores::ZERO
            || Duration::from_millis(offer.start_within_ms) > request::MAXIMUM_HOLD_LIMIT
        {
            return Err(request::invalid(
                "The offered size or hold limit is invalid.",
            ));
        }
        // A size the caller learned rather than declared is never a reason to
        // refuse: it shrinks to what this member can hold, as the master expects.
        let basis = offer.size_basis.unwrap_or(SizeBasis::Declared);
        let size = match basis {
            SizeBasis::Learned { .. } if self.capacity.capacity > Cores::ZERO => {
                offer.size.min(self.capacity.capacity)
            }
            _ => offer.size,
        };
        let mut book = self.book();
        let state = self.state_of(&book);
        if !book.admission.fits(size) {
            let failure = Failure::new(
                "request_too_large",
                format!(
                    "Size {} is larger than {}'s capacity of {} cores.",
                    size,
                    self.name(),
                    self.capacity.capacity
                ),
                "Request a smaller --size, or lower reserved_cores on that member.",
            );
            return Ok(OfferReply::TooLarge { state, failure });
        }
        let queued = book.admission.queued();
        let waiting = queued.interactive + queued.deferrable;
        // Deferrable work, such as housekeeping, waits behind interactive work and
        // never keeps placed interactive work out.
        if book.stopping
            || book.hosting_held > 0
            || queued.interactive > 0
            || !self.gate(&book).admits()
            || size > book.admission.free()
        {
            let report = self.situation(&book, size, Duration::ZERO, waiting);
            return Ok(OfferReply::Busy { state, report });
        }
        if !copy_free {
            return Ok(OfferReply::CopyBusy { state });
        }
        if book.entries.contains_key(&offer.run_id) || self.records.get(&offer.run_id)?.is_some() {
            return Err(request::invalid("A run with this ID already exists here."));
        }
        let now = self.clock.now();
        let service = self
            .processes
            .describe(std::process::id())
            .map(|info| info.identity)
            .unwrap_or(ProcessIdentity {
                pid: std::process::id(),
                started_us: 0,
            });
        let member = self.name();
        let reason = format!("Placed by {from}; admitted at once.");
        let mut entry = Entry {
            record: Record {
                run_id: offer.run_id.clone(),
                kind: Kind::Run,
                synthetic: crate::rehearsal::synthetic(offer.label.as_deref()),
                internal: false,
                peer_io: None,
                label: offer.label.clone(),
                workload: offer.workload.clone(),
                plan_observation: None,
                placement: Placement::Member(member.clone()),
                urgency: Urgency::Interactive {
                    hold_limit_ms: Some(offer.start_within_ms),
                },
                size,
                size_basis: Some(basis),
                calling_member: from.into(),
                workspace_recorded: true,
                workspace: Some(crate::scheduling::record::WorkspaceIdentity::of(
                    from,
                    &offer.workspace,
                )),
                member: Some(member.clone()),
                placement_note: Some(format!("Placed here by {from}, the master.")),
                nested_in: None,
                shares_capacity_of: None,
                state: State::Running,
                outcome: None,
                requested_at: now,
                started_at: Some(now),
                ended_at: None,
                waited_ms: Some(0),
                run_ms: None,
                run_phases: None,
                cpu_ms: None,
                hold_limit: Some(HoldLimitRecord {
                    seconds: Duration::from_millis(offer.start_within_ms),
                    basis: HoldBasis::Requested,
                    expected_run_seconds: None,
                }),
                decision: None,
                warnings: Vec::new(),
                requested_by: service,
                processes: Vec::new(),
                placed: Some(PlacedRecord {
                    side: Side::Member,
                    calling_member: from.into(),
                    member,
                    caller_workspace: offer.workspace.root.clone(),
                    repository: Some(offer.workspace.common_dir.clone()),
                    copy: Some(copy.into()),
                    location: Some(location.into()),
                    warmth: Some(warmth),
                    reached: Reached::Placed,
                    sync: None,
                    exit_code: None,
                    signal: None,
                    last_contact_at: Some(now),
                    leftovers: None,
                    leftovers_wait_ms: None,
                    interrupted: None,
                }),
                settings: None,
                reacquired: 0,
                config_sync: None,
                pre_run_history: Some(Default::default()),
                fallbacks: Vec::new(),
            },
            expected_run: offer.expected_run_ms.map(Duration::from_millis),
            phase: Phase::Active {
                holders: BTreeSet::new(),
            },
            hosted: true,
            auto: None,
        };
        let decision: Decision = self.decision(&book, &entry, Verdict::Started, &reason);
        entry.record.decision = Some(decision);
        book.admission.adopt(offer.run_id.clone(), size);
        self.persist(&mut entry);
        book.entries.insert(offer.run_id.clone(), entry);
        Ok(OfferReply::Accepted {
            workload_contract: Some(crate::workload_metadata::VERSION),
            state: self.state_of(&book),
            copy: copy.into(),
            location: location.into(),
            warmth: Some(warmth),
        })
    }

    /// This member's quote for its master's placement decision, without copies:
    /// admission state, load, and the work holding and waiting for its capacity.
    pub fn quote_state(&self) -> Quote {
        let book = self.book();
        let now = self.clock.now();
        let holding = self
            .holding(&book)
            .into_iter()
            .map(|work| {
                let running = now.since(work.started_at.unwrap_or(now));
                HeldWork {
                    run_id: work.run_id,
                    kind: work.kind,
                    label: work.label,
                    size: work.size,
                    running_ms: quote::millis(running),
                    expected_remaining_ms: work
                        .expected_run
                        .map(|run| quote::millis(run.saturating_sub(running))),
                }
            })
            .collect();
        let waiting = book
            .admission
            .waiting()
            .map(|(id, size)| WaitingWork {
                size,
                expected_run_ms: book
                    .entries
                    .get(id)
                    .and_then(|e| e.expected_run)
                    .map(quote::millis),
            })
            .collect();
        Quote {
            state: self.state_of(&book),
            utilization: book.load.cpu_utilization(),
            holding,
            waiting,
            copies: Vec::new(),
            admission: Some(self.parameters().admission.clone()),
        }
    }

    fn hosted<'a>(book: &'a mut super::Book, id: &RunId) -> Option<&'a mut Entry> {
        book.entries.get_mut(id).filter(|entry| entry.hosted)
    }

    pub fn hosted_synced(&self, id: &RunId, sync: SyncSummary) {
        let mut book = self.book();
        if let Some(entry) = Self::hosted(&mut book, id) {
            if let Some(placed) = &mut entry.record.placed {
                placed.reached = Reached::Synced;
                placed.sync = Some(sync);
                placed.last_contact_at = Some(self.clock.now());
            }
            self.persist(entry);
        }
    }

    pub fn hosted_warnings(&self, id: &RunId, warnings: &[String]) {
        let mut book = self.book();
        if let Some(entry) = Self::hosted(&mut book, id) {
            for warning in warnings.iter().take(64) {
                if !entry.record.warnings.contains(warning) {
                    entry.record.warnings.push(warning.clone());
                }
            }
            self.persist(entry);
        }
    }

    /// The hosted command started as `pid`: work it requests here nests in the run,
    /// and a restart can find and stop it.
    pub fn hosted_started(&self, id: &RunId, pid: u32) {
        let mut book = self.book();
        let Some(identity) = self.processes.describe(pid).map(|info| info.identity) else {
            return;
        };
        if Self::hosted(&mut book, id).is_none() || !self.bind(&mut book, id, identity) {
            return;
        }
        if let Some(entry) = Self::hosted(&mut book, id) {
            if let Phase::Active { holders } = &mut entry.phase {
                holders.insert(identity);
            }
            if let Some(placed) = &mut entry.record.placed {
                placed.reached = Reached::Started;
            }
            self.persist(entry);
        }
    }

    /// What the hosted command left running, as it runs and once it ended. The
    /// run usually ended by then, so its saved record is updated.
    pub fn hosted_leftovers(&self, id: &RunId, leftovers: Leftovers) {
        let mut book = self.book();
        if leftovers.is_running() {
            book.lingering.insert(id.clone());
        } else {
            book.lingering.remove(id);
        }
        if let Some(entry) = Self::hosted(&mut book, id) {
            if let Some(placed) = &mut entry.record.placed {
                placed.leftovers = Some(leftovers);
            }
            self.persist(entry);
            return;
        }
        // Under the book's lock, so the record cannot end meanwhile.
        match self.records.get(id) {
            Ok(Some(mut record)) => {
                let Some(placed) = record.placed.as_mut().filter(|p| p.side == Side::Member) else {
                    return;
                };
                placed.leftovers = Some(leftovers);
                self.save(&record);
            }
            Ok(None) => (),
            Err(error) => super::log("leftovers_unrecorded", &error),
        }
    }

    /// The hosted run waited for an earlier run's leftovers in its copy.
    pub fn hosted_waited_for_leftovers(&self, id: &RunId, waited: Duration) {
        let mut book = self.book();
        if let Some(entry) = Self::hosted(&mut book, id) {
            if let Some(placed) = &mut entry.record.placed {
                placed.leftovers_wait_ms = Some(waited.as_millis() as u64);
            }
            self.persist(entry);
        }
    }

    pub fn hosted_ended(
        &self,
        id: &RunId,
        end: HostedEnd,
        failure_phase: Option<RemoteFailurePhase>,
    ) {
        let mut book = self.book();
        let Some(entry) = Self::hosted(&mut book, id) else {
            return;
        };
        entry.record.run_phases = failure_phase.map(RunPhases::failed);
        if let HostedEnd::Exited { cpu_ms, .. } | HostedEnd::Signaled { cpu_ms, .. } = &end {
            entry.record.cpu_ms = *cpu_ms;
        }
        let (outcome, reached, exit) = match end {
            HostedEnd::Exited { code, .. } => (
                Outcome::Exited { exit_code: code },
                Reached::Finished,
                Some(HostedExit::Code(code)),
            ),
            HostedEnd::Signaled { signal, .. } => (
                Outcome::Signaled { signal },
                Reached::Finished,
                Some(HostedExit::Signal(signal)),
            ),
            HostedEnd::Failed {
                code,
                reached,
                exit,
            } => (Outcome::Failed { code }, reached, exit),
        };
        if let Some(placed) = &mut entry.record.placed {
            placed.reached = reached;
            match exit {
                Some(HostedExit::Code(code)) => placed.exit_code = Some(code),
                Some(HostedExit::Signal(signal)) => placed.signal = Some(signal),
                None => (),
            }
        }
        self.end(&mut book, id, outcome);
    }
}
