//! Work that is running: a run's command and end, releasing leases, shutdown.
use super::{Book, Entry, Phase, Scheduler, not_found, stopping};
use crate::{
    failure::Result,
    scheduling::{
        messages::{CommandStarted, Release, Released, RunEnded},
        placed::{PlacedReport, Reached, Side},
        processes,
        record::{MAXIMUM_CPU_MS, Outcome},
        request::{self, Kind},
        run_id::RunId,
        run_phases::{RemoteFailurePhase, RunPhases},
    },
};

impl Scheduler {
    /// Active run `id` requested by the process `peer`. The peer is compared by
    /// identity, so a later process that reuses a killed caller's PID is refused.
    fn own_run<'a>(&self, book: &'a mut Book, peer: u32, id: &RunId) -> Result<&'a mut Entry> {
        let peer = self.processes.describe(peer).map(|info| info.identity);
        match book.entries.get_mut(id) {
            Some(entry)
                if matches!(entry.phase, Phase::Active { .. })
                    && entry.record.kind == Kind::Run
                    && Some(entry.record.requested_by) == peer =>
            {
                Ok(entry)
            }
            _ => Err(not_found(id)),
        }
    }

    /// The caller of a run reports the process that runs its command, so the grant
    /// lasts as long as the command even if the caller is killed.
    pub fn command_started(&self, peer: u32, id: &RunId, started: CommandStarted) -> Result<()> {
        let mut book = self.book();
        self.own_run(&mut book, peer, id)?;
        let command = self
            .processes
            .describe(started.pid)
            .filter(|info| info.parent == peer)
            .ok_or_else(|| request::invalid("The command is not a child of the caller."))?
            .identity;
        if !self.bind(&mut book, id, command) {
            return Ok(());
        }
        let entry = book.entries.get_mut(id).expect("checked above");
        if let Phase::Active { holders } = &mut entry.phase {
            holders.insert(command);
        }
        self.persist(entry);
        Ok(())
    }

    /// The member a caller's own placed run runs on, for its caller to attach.
    pub fn placed_member(&self, peer: u32, id: &RunId) -> Result<String> {
        let mut book = self.book();
        let entry = self.own_run(&mut book, peer, id)?;
        match &entry.record.placed {
            Some(placed) if placed.side == Side::Caller && placed.reached == Reached::Placed => {
                Ok(placed.member.clone())
            }
            _ => Err(not_found(id)),
        }
    }

    /// What the caller of a placed run learned about it, for its record here.
    pub fn placed_report(&self, peer: u32, id: &RunId, report: PlacedReport) -> Result<()> {
        let mut book = self.book();
        let entry = self.own_run(&mut book, peer, id)?;
        let Some(placed) = entry
            .record
            .placed
            .as_mut()
            .filter(|p| p.side == Side::Caller)
        else {
            return Err(not_found(id));
        };
        if report.warnings.len() > 64 || report.warnings.iter().any(|w| w.len() > 1000) {
            return Err(request::invalid("Too many or too long warnings."));
        }
        placed.reached = report.reached;
        placed.sync = report.sync;
        placed.exit_code = report.exit_code;
        placed.signal = report.signal;
        if let Some(cpu_ms) = report.cpu_ms.filter(|ms| *ms < MAXIMUM_CPU_MS) {
            entry.record.cpu_ms = Some(cpu_ms);
        }
        placed.last_contact_at = Some(self.clock.now());
        for warning in report.warnings {
            if !entry.record.warnings.contains(&warning) {
                entry.record.warnings.push(warning);
            }
        }
        self.persist(entry);
        Ok(())
    }

    /// Only the nearest executing command in the caller's live lineage can
    /// assert observations. Requesters, siblings, other grants and reused PIDs
    /// cannot write them. The active grant supplies the immutable descriptor.
    pub fn observe_workload(
        &self,
        peer: u32,
        id: &RunId,
        observation: crate::workload_metadata::Observation,
    ) -> Result<()> {
        let lineage = processes::lineage(&*self.processes, peer);
        let mut book = self.book();
        let nearest = lineage.iter().find_map(|info| {
            // A nested request is a barrier even before its child is bound, or
            // while queued: descendants cannot fall through to an outer grant.
            if book.entries.values().any(|entry| entry.record.kind == Kind::Run
                && entry.record.requested_by == info.identity) {
                return Some(None);
            }
            book.bindings.runs_of(info.identity).find(|run| {
                book.entries.get(*run).is_some_and(|entry| {
                    entry.record.kind == Kind::Run
                        && matches!(&entry.phase, Phase::Active { holders } if holders.contains(&info.identity))
                })
            }).map(|run| Some(run.clone()))
        });
        if nearest.flatten().as_ref() != Some(id) {
            return Err(not_found(id));
        }
        let entry = book.entries.get_mut(id).expect("authorized active command");
        if entry.record.workload.is_none() {
            return Err(request::invalid(
                "This run did not declare workload metadata.",
            ));
        }
        observation.validate(entry.record.workload.as_ref())?;
        if entry.record.plan_observation.is_some() {
            return Err(request::invalid(
                "A terminal workload observation was already recorded.",
            ));
        }
        entry.record.plan_observation = Some(observation);
        self.persist(entry);
        Ok(())
    }

    pub fn run_ended(&self, peer: u32, id: &RunId, ended: RunEnded) -> Result<()> {
        let mut book = self.book();
        let entry = self.own_run(&mut book, peer, id)?;
        if let RunEnded::Exited {
            plan_observation: Some(observation),
            ..
        }
        | RunEnded::Signaled {
            plan_observation: Some(observation),
            ..
        } = &ended
        {
            // The local command reports through the lineage-bound endpoint;
            // only a placed caller relays the member's terminal assertion here.
            if !entry
                .record
                .placed
                .as_ref()
                .is_some_and(|p| p.side == Side::Caller)
                || entry.record.workload.is_none()
            {
                return Err(request::invalid(
                    "Only a placed caller can relay a workload observation.",
                ));
            }
            observation.validate(entry.record.workload.as_ref())?;
            if entry.record.plan_observation.is_some() {
                return Err(request::invalid(
                    "A terminal workload observation was already recorded.",
                ));
            }
        }
        let plan_observation = match &ended {
            RunEnded::Exited {
                plan_observation, ..
            }
            | RunEnded::Signaled {
                plan_observation, ..
            } => plan_observation.clone(),
            _ => None,
        };
        let failure_phase = ended.failure_phase();
        let outcome = match ended {
            RunEnded::Exited {
                exit_code,
                cpu_ms,
                run_phases,
                ..
            } => {
                Self::measured(entry, cpu_ms, run_phases);
                Outcome::Exited { exit_code }
            }
            RunEnded::Signaled {
                signal,
                cpu_ms,
                run_phases,
                ..
            } => {
                Self::measured(entry, cpu_ms, run_phases);
                Outcome::Signaled { signal }
            }
            RunEnded::Failed { code, .. }
                if !code.is_empty()
                    && code.len() <= 64
                    && code.bytes().all(|b| b.is_ascii_lowercase() || b == b'_') =>
            {
                Outcome::Failed { code }
            }
            RunEnded::PeerFailed { failure, .. } => {
                use crate::peer_diagnostics::{Kind, Phase};
                if failure.duration_ms > 86_400_000
                    || !matches!(
                        failure.kind,
                        Kind::Receive
                            | Kind::Send
                            | Kind::Timeout
                            | Kind::Authentication
                            | Kind::Protocol
                    )
                    || !matches!(
                        failure.phase,
                        Phase::Receive | Phase::Send | Phase::Authentication
                    )
                {
                    return Err(request::invalid("Invalid peer stream failure."));
                }
                let placed = entry
                    .record
                    .placed
                    .as_ref()
                    .filter(|p| p.side == Side::Caller)
                    .ok_or_else(|| {
                        request::invalid(
                            "Only a remotely placed caller can report a peer stream failure.",
                        )
                    })?;
                let attempt = crate::peer_diagnostics::Attempt {
                    at: self.clock.now(),
                    member: placed.member.clone(),
                    operation: "result_stream".into(),
                    attempt: 1,
                    phase: failure.phase,
                    failure_kind: Some(failure.kind),
                    duration_ms: failure.duration_ms,
                    refusal: None,
                };
                crate::peer_diagnostics::record(&attempt);
                Outcome::Failed {
                    code: "connection_lost".into(),
                }
            }
            RunEnded::Failed { .. } => return Err(request::invalid("Invalid failure code.")),
        };
        if entry.record.placed.is_some()
            && let Some(phase @ (RemoteFailurePhase::WorkspaceSync | RemoteFailurePhase::Execution)) =
                failure_phase
        {
            entry.record.run_phases = Some(RunPhases::failed(phase));
        }
        if let Some(observation) = plan_observation {
            entry.record.plan_observation = Some(observation);
        }
        self.end(&mut book, id, outcome);
        Ok(())
    }

    /// Keeps the caller's observed boundaries and CPU time. The member's own
    /// CPU measurement of a placed run may already be there.
    fn measured(entry: &mut Entry, cpu_ms: Option<u64>, run_phases: Option<RunPhases>) {
        if entry.record.cpu_ms.is_none() {
            entry.record.cpu_ms = cpu_ms.filter(|ms| *ms < MAXIMUM_CPU_MS);
        }
        entry.record.run_phases =
            run_phases.filter(|phases| phases.valid_completion(entry.record.placed.is_some()));
    }

    /// Releases one lease, or every lease held by a process in the caller's
    /// lineage. Any process of this user may release a lease by its ID.
    pub fn release(&self, peer: u32, release: Release) -> Result<Released> {
        let mut book = self.book();
        let ids: Vec<RunId> = match (release.run_id, release.holder_pid) {
            (Some(id), None) => match book.entries.get(&id) {
                Some(entry)
                    if entry.record.kind == Kind::Lease
                        && matches!(entry.phase, Phase::Active { .. }) =>
                {
                    vec![id]
                }
                Some(_) => return Err(request::invalid(format!("{id} is not an active lease."))),
                None if self.records.get(&id)?.is_some() => Vec::new(),
                None => return Err(not_found(&id)),
            },
            (None, Some(holder)) => {
                let lineage = processes::lineage(&*self.processes, peer);
                let holder = lineage
                    .iter()
                    .skip(1)
                    .find(|p| p.identity.pid == holder)
                    .ok_or_else(|| {
                        request::invalid(
                            "A lease holder must be the process that runs elsecall or one of its ancestors.",
                        )
                    })?
                    .identity;
                let mut ids: Vec<RunId> = book
                    .entries
                    .iter()
                    .filter(|(_, e)| {
                        e.record.kind == Kind::Lease
                            && matches!(&e.phase, Phase::Active { holders } if holders.contains(&holder))
                    })
                    .map(|(id, _)| id.clone())
                    .collect();
                ids.sort();
                ids
            }
            _ => {
                return Err(request::invalid(
                    "Release either one lease ID or one holder.",
                ));
            }
        };
        for id in &ids {
            self.end(&mut book, id, Outcome::Released);
        }
        Ok(Released { released: ids })
    }

    /// Fails every waiting request and refuses new ones, so the service can stop
    /// without waiting for hold limits or deadlines.
    pub fn shutdown(&self) {
        let mut book = self.book();
        book.stopping = true;
        let waiting: Vec<RunId> = book
            .entries
            .iter()
            .filter(|(_, e)| matches!(e.phase, Phase::Waiting(_) | Phase::Placing(_)))
            .map(|(id, _)| id.clone())
            .collect();
        for id in waiting {
            self.cancel_with(
                &mut book,
                &id,
                "The service stopped while the request waited.",
                stopping(),
            );
        }
    }
}
