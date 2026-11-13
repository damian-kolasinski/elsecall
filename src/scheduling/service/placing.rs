//! Requests this master places on a required member. They wait here, in arrival
//! order per member; a task offers them to the member, which admits one at once or
//! refuses it, until each starts or reaches its hold limit. Before each offer the
//! task brings the member's synced configuration up to date, so a run never starts
//! on stale configuration. When that fails, a placeable request is placed again
//! without the member, and a request required there fails.
use super::{Book, Entry, Phase, Scheduler};
use crate::{
    failure::Failure,
    scheduling::{
        messages::{Grant, MemberState},
        placed::{
            OfferReply, PlacedGrant, PlacedRecord, PlacementOffer, Reached, Side, Warmth,
            WorkspaceRef,
        },
        pre_run_sync::{self, Done, Failed, PreRunSyncRecord, SyncFailure},
        processes::ProcessIdentity,
        quote::{self, HeldWork},
        record::{Candidate, Decision, Outcome, SizeBasis, Verdict},
        run_id::RunId,
        timestamp::Timestamp,
    },
};
use std::{collections::BTreeSet, sync::Weak, time::Duration};
use tokio::time::Instant;
use tokio::{sync::oneshot, task::AbortHandle};

/// How soon a refused request is offered again.
const RETRY: Duration = Duration::from_millis(500);

pub(super) struct Placing {
    pub requester: ProcessIdentity,
    /// The member it is offered to; `None` while a placeable request is
    /// undecided because no member can take it in time.
    pub target: Option<String>,
    pub workspace: WorkspaceRef,
    /// The hold limit, on the wall clock; the sync must finish before it too.
    pub expires_at: Timestamp,
    pub responder: oneshot::Sender<Result<Grant, Failure>>,
    pub timer: AbortHandle,
    /// What the member last answered, for the decision and `fleet_busy`.
    pub refusal: Option<(MemberState, Option<Failure>)>,
    /// The target's configuration is being synced, or an offer is on its way to
    /// it; the request does not move until the answer is in, so no admission is
    /// made for a request that left.
    pub in_flight: Option<InFlight>,
    /// The configuration sync before the latest offer to the target.
    pub config_sync: Option<Done>,
    /// An offer to the target was attempted; until then, a hold limit that
    /// passes during the sync is the sync's doing.
    pub offered: bool,
    /// The current pre-run result/hold cutoff has already reached durable history.
    pub pre_run_recorded: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum InFlight {
    /// The target is being brought up to date with the synced configuration,
    /// since the instant given.
    Syncing { since: Instant },
    /// The offer is on its way.
    Offered,
}

/// How the sync before an offer ended, for the rest of the offer loop.
enum AfterSync {
    /// Offer the request.
    Offer(PlacementOffer),
    /// The member could not be reached: as if the offer could not reach it.
    Unavailable(Failure),
}

impl Scheduler {
    fn record_pre_run_attempt(
        &self,
        book: &mut Book,
        id: &RunId,
        target: &str,
        since: Instant,
        cause: Option<String>,
    ) {
        let Some(entry) = book.entries.get_mut(id) else {
            return;
        };
        let Phase::Placing(placing) = &mut entry.phase else {
            return;
        };
        if placing.pre_run_recorded {
            return;
        }
        placing.pre_run_recorded = true;
        let duration_ms = quote::millis(since.elapsed());
        let history = entry
            .record
            .pre_run_history
            .get_or_insert_with(|| pre_run_sync::History {
                complete: false,
                ..Default::default()
            });
        history.push(pre_run_sync::Attempt {
            member: target.to_owned(),
            started_at: Timestamp::from_millis(
                self.clock.now().millis().saturating_sub(duration_ms),
            ),
            duration_ms,
            cause,
        });
    }

    /// Queues `entry` for `target` and makes sure someone offers it.
    pub(super) fn place(
        &self,
        book: &mut Book,
        mut entry: Entry,
        target: String,
        workspace: WorkspaceRef,
        expires_at: Timestamp,
        responder: oneshot::Sender<Result<Grant, Failure>>,
    ) {
        let id = entry.record.run_id.clone();
        entry.phase = Phase::Placing(Box::new(Placing {
            requester: entry.record.requested_by,
            target: Some(target.clone()),
            workspace,
            expires_at,
            responder,
            timer: self.timer(&id, expires_at.since(self.clock.now())),
            refusal: None,
            in_flight: None,
            config_sync: None,
            offered: false,
            pre_run_recorded: false,
        }));
        self.persist(&mut entry);
        book.entries.insert(id.clone(), entry);
        self.queue_for(book, &id, target);
    }

    /// Adds `id` to the requests offered to `target`, in arrival order, and makes
    /// sure a task offers them.
    pub(super) fn queue_for(&self, book: &mut Book, id: &RunId, target: String) {
        let requested_at = |id: &RunId| book.entries.get(id).map(|e| e.record.requested_at);
        let at = requested_at(id);
        let queue = book.placing.get(&target).cloned().unwrap_or_default();
        let position = queue
            .iter()
            .position(|queued| requested_at(queued) > at)
            .unwrap_or(queue.len());
        book.placing
            .entry(target.clone())
            .or_default()
            .insert(position, id.clone());
        if book.offering.insert(target.clone()) {
            tokio::spawn(offer(self.this.clone(), target));
        }
    }

    fn offer_for(&self, book: &Book, id: &RunId) -> Option<PlacementOffer> {
        let entry = book.entries.get(id)?;
        let Phase::Placing(placing) = &entry.phase else {
            return None;
        };
        Some(PlacementOffer {
            run_id: id.clone(),
            size: entry.record.size,
            label: entry.record.label.clone(),
            workload: entry.record.workload.clone(),
            workspace: placing.workspace.clone(),
            start_within_ms: placing.expires_at.since(self.clock.now()).as_millis() as u64,
            size_basis: entry.record.size_basis,
            expected_run_ms: entry.expected_run.map(quote::millis),
        })
    }

    /// The member admitted the request: it starts there.
    fn placed(
        &self,
        book: &mut Book,
        id: &RunId,
        state: MemberState,
        copy: String,
        location: String,
        warmth: Option<Warmth>,
    ) {
        let Some(mut entry) = book.entries.remove(id) else {
            return;
        };
        let Phase::Placing(placing) = std::mem::replace(
            &mut entry.phase,
            Phase::Active {
                holders: BTreeSet::new(),
            },
        ) else {
            unreachable!("checked by the caller")
        };
        placing.timer.abort();
        let target = placing
            .target
            .clone()
            .expect("only requests with a target are offered");
        if let Some(queue) = book.placing.get_mut(&target) {
            queue.retain(|queued| queued != id);
        }
        entry.phase = Phase::Active {
            holders: BTreeSet::from([placing.requester]),
        };
        let synced_paths = placing
            .config_sync
            .as_ref()
            .map(|done| done.synced.current_paths().to_vec())
            .unwrap_or_default();
        entry.record.config_sync = placing.config_sync.as_ref().map(Done::record);
        if let Some(SizeBasis::Learned { .. }) = entry.record.size_basis {
            // The member shrank a learned size to its capacity, as offered.
            entry.record.size = entry.record.size.min(state.capacity.capacity);
        }
        entry.record.decision = Some(if entry.auto.is_some() {
            let reason = Self::start_reason(&entry, &target);
            self.auto_decision(&entry, Verdict::Started, Some(&target), &reason)
        } else {
            let reason = format!("{target} admitted it at once, as required.");
            decision(
                &entry,
                Verdict::Started,
                &reason,
                &state,
                true,
                self.parameters(),
                self.clock.now(),
            )
        });
        entry.record.placed = Some(PlacedRecord {
            side: Side::Caller,
            calling_member: self.name(),
            member: target.clone(),
            caller_workspace: placing.workspace.root.clone(),
            repository: Some(placing.workspace.common_dir.clone()),
            copy: Some(copy.clone()),
            location: Some(location),
            warmth,
            reached: Reached::Placed,
            sync: None,
            exit_code: None,
            signal: None,
            last_contact_at: Some(self.clock.now()),
            leftovers: None,
            leftovers_wait_ms: None,
            interrupted: None,
        });
        let grant = self.begin(
            &mut entry,
            target,
            Some(PlacedGrant {
                start_by: placing.expires_at,
                copy,
                calling_member: self.name(),
                synced_paths,
            }),
        );
        book.entries.insert(id.clone(), entry);
        if placing.responder.send(Ok(grant)).is_err() {
            self.end(
                book,
                id,
                Outcome::Cancelled {
                    reason: "The caller disconnected before the grant arrived.".into(),
                },
            );
        }
    }

    /// Fails a placed request with what the member last said about itself.
    fn fail_placing(
        &self,
        book: &mut Book,
        id: &RunId,
        verdict: Verdict,
        failure: Failure,
        state: Option<MemberState>,
    ) {
        let failure = failure.for_run(id);
        let decision = match (book.entries.get(id), &state) {
            (Some(entry), Some(state)) => decision(
                entry,
                verdict,
                &failure.message,
                state,
                false,
                self.parameters(),
                self.clock.now(),
            ),
            // Without the member's state, there is no candidate to describe.
            (Some(_), None) => Decision {
                decided_at: self.clock.now(),
                verdict,
                reason: failure.message.clone(),
                candidates: Vec::new(),
                unreachable: Vec::new(),
                parameters: Some(self.parameters().clone()),
                scoring: None,
                arrival: None,
            },
            (None, _) => return,
        };
        let outcome = Outcome::Failed {
            code: failure.code.clone(),
        };
        if let Some(responder) = self
            .close(book, id, outcome, Some(decision))
            .and_then(Phase::responder)
        {
            let _ = responder.send(Err(failure));
        }
    }

    /// Fails a placed request whose target could not be brought up to date with
    /// the synced configuration, recording the attempt.
    pub(super) fn fail_config_sync(
        &self,
        book: &mut Book,
        id: &RunId,
        target: &str,
        SyncFailure { failure, cause }: SyncFailure,
        since: Instant,
    ) {
        if let Some(entry) = book.entries.get_mut(id) {
            entry.record.config_sync = Some(PreRunSyncRecord::failed(
                target,
                quote::millis(since.elapsed()),
                cause,
            ));
        }
        self.fail_before_offer(book, id, Verdict::ConfigSyncFailed, failure);
    }

    /// Fails a placed request before it could be offered, placeable or not.
    fn fail_before_offer(&self, book: &mut Book, id: &RunId, verdict: Verdict, failure: Failure) {
        let Some(entry) = book.entries.get_mut(id) else {
            return;
        };
        if entry.auto.is_some() {
            let failure = failure.for_run(id);
            let decision = self.auto_decision(entry, verdict, None, &failure.message);
            return self.fail_with(book, id, failure, decision);
        }
        let state = match &entry.phase {
            Phase::Placing(placing) => placing.refusal.as_ref().map(|(state, _)| state.clone()),
            _ => None,
        };
        self.fail_placing(book, id, verdict, failure, state);
    }

    /// The hold limit of a placed request passed.
    pub(super) fn expire_placing(&self, book: &mut Book, id: &RunId) {
        let interrupted = book.entries.get(id).and_then(|entry| match &entry.phase {
            Phase::Placing(placing) => match (&placing.target, placing.in_flight) {
                (Some(target), Some(InFlight::Syncing { since })) => Some((target.clone(), since)),
                _ => None,
            },
            _ => None,
        });
        if let Some((target, since)) = interrupted {
            self.record_pre_run_attempt(book, id, &target, since, Some("hold_limit_passed".into()));
        }
        let Some(Entry {
            phase: Phase::Placing(placing),
            record,
            auto,
            ..
        }) = book.entries.get(id)
        else {
            return;
        };
        // Only a sync that kept the request from its first offer to the target is
        // to blame; a request already offered there waited for room, as before.
        if let Some(InFlight::Syncing { since }) = placing.in_flight
            && !placing.offered
            && let Some(target) = placing.target.clone()
        {
            let limit = record.hold_limit.map_or(0, |l| l.seconds.as_secs());
            let failure = pre_run_sync::timed_out(&target, limit);
            return self.fail_config_sync(book, id, &target, failure, since);
        }
        let Some(target) = placing.target.clone().filter(|_| auto.is_none()) else {
            return self.expire_auto(book, id);
        };
        let limit = record.hold_limit.map_or(0, |l| l.seconds.as_secs());
        let (state, report) = match &placing.refusal {
            Some((state, report)) => (Some(state.clone()), report.clone()),
            None => (None, None),
        };
        let failure = match report {
            Some(report) => Failure {
                message: format!(
                    "{target} had no room within the {limit} s hold limit: {}",
                    report.message
                ),
                ..report
            },
            None => Failure::new(
                "fleet_busy",
                format!(
                    "{target} did not admit the request within the {limit} s hold limit; another run was using this workspace's copy there."
                ),
                "Retry once the other run of this workspace ends, or pass a longer --hold-limit.",
            ),
        };
        self.fail_placing(book, id, Verdict::FleetBusy, failure, state);
    }
}

/// A decision naming the required member and its state.
fn decision(
    entry: &Entry,
    verdict: Verdict,
    reason: &str,
    state: &MemberState,
    chosen: bool,
    parameters: &crate::scheduling::parameters::Parameters,
    now: Timestamp,
) -> Decision {
    Decision {
        decided_at: now,
        verdict,
        reason: reason.into(),
        candidates: vec![Candidate {
            member: state.member.clone(),
            chosen,
            why: if chosen {
                "The required member.".into()
            } else {
                reason.into()
            },
            capacity: state.capacity,
            in_use: state.in_use,
            free: state.free,
            queued: state.queued,
            load: state.load.clone(),
            cpu_utilization_window: None,
            gate: state.gate,
            expected_run_seconds: entry.expected_run,
            expected_completion: entry.expected_run.map(|run| now.after(run)),
            estimate: None,
            inputs: None,
        }],
        unreachable: Vec::new(),
        parameters: Some(parameters.clone()),
        scoring: None,
        arrival: None,
    }
}

/// Offers the requests waiting for `target`, oldest first, until none is left. A
/// request refused for room blocks those behind it, so large requests are not
/// starved; one refused because its copy is in use lets the next one try.
async fn offer(scheduler: Weak<Scheduler>, target: String) {
    loop {
        let (members, queue) = {
            let Some(scheduler) = scheduler.upgrade() else {
                return;
            };
            let mut book = scheduler.book();
            let queue: Vec<RunId> = book
                .placing
                .get(&target)
                .map(|q| q.iter().cloned().collect())
                .unwrap_or_default();
            if queue.is_empty() {
                book.placing.remove(&target);
                book.offering.remove(&target);
                book.offer_retry.remove(&target);
                return;
            }
            (scheduler.members.clone(), queue)
        };
        let mut refused = false;
        for id in &queue {
            // First the target's synced configuration, for a request still queued
            // here; the request stays put meanwhile.
            let since = Instant::now();
            let (expires_at, pre_run, clock) = {
                let Some(scheduler) = scheduler.upgrade() else {
                    return;
                };
                let mut book = scheduler.book();
                match book.entries.get_mut(id).map(|e| &mut e.phase) {
                    Some(Phase::Placing(placing))
                        if placing.target.as_deref() == Some(target.as_str()) =>
                    {
                        placing.in_flight = Some(InFlight::Syncing { since });
                        placing.pre_run_recorded = false;
                        // A sync from an earlier attempt says nothing about this one.
                        placing.config_sync = None;
                        (
                            placing.expires_at,
                            scheduler.pre_run.clone(),
                            scheduler.clock.clone(),
                        )
                    }
                    _ => continue,
                }
            };
            // The sync runs as its own task, so a hold limit that passes stops the
            // waiting, never a sync halfway through; it then finishes on its own.
            let sync = tokio::spawn(pre_run.bring_up_to_date_for(&target, id));
            let synced = tokio::time::timeout(expires_at.since(clock.now()), sync).await;
            // Then the offer, built just before it is sent; a member that could not
            // be reached for the sync is handled as one that could not be offered.
            let after = {
                let Some(scheduler) = scheduler.upgrade() else {
                    return;
                };
                let mut book = scheduler.book();
                let cause = match &synced {
                    Ok(Ok(Ok(_))) => None,
                    Ok(Ok(Err(cause))) => Some(pre_run_sync::cause_code(cause)),
                    Ok(Err(_)) => Some("internal_error".into()),
                    Err(_) => Some("hold_limit_passed".into()),
                };
                scheduler.record_pre_run_attempt(&mut book, id, &target, since, cause);
                let Some(Entry {
                    phase: Phase::Placing(placing),
                    auto,
                    ..
                }) = book.entries.get_mut(id)
                else {
                    // Withdrawn, or failed at its hold limit, meanwhile.
                    continue;
                };
                let placeable = auto.is_some();
                let synced = match synced {
                    Ok(Ok(synced)) => synced,
                    Ok(Err(_)) => Err(Failure::new(
                        "internal_error",
                        "The configuration sync stopped unexpectedly.",
                        "Run elsecall doctor.",
                    )),
                    // The hold limit passed: expiry decides what to blame.
                    Err(_) => {
                        scheduler.expire_placing(&mut book, id);
                        continue;
                    }
                };
                placing.in_flight = Some(InFlight::Offered);
                placing.offered = true;
                match synced.map_err(|cause| pre_run_sync::failed(&target, &cause)) {
                    Ok(synced) => {
                        placing.config_sync = Some(Done {
                            member: target.clone(),
                            duration_ms: quote::millis(since.elapsed()),
                            synced,
                        });
                        match scheduler.offer_for(&book, id) {
                            Some(offer) => AfterSync::Offer(offer),
                            None => continue,
                        }
                    }
                    Err(Failed::Unavailable(failure)) => AfterSync::Unavailable(failure),
                    // The target is fine; this member can no longer place.
                    Err(Failed::NotMaster(failure)) => {
                        scheduler.fail_before_offer(
                            &mut book,
                            id,
                            Verdict::MemberUnavailable,
                            failure,
                        );
                        continue;
                    }
                    // Placeable work is placed again without the target; work
                    // required there fails.
                    Err(Failed::Sync(failure)) if placeable => {
                        scheduler.fall_back(&mut book, id, &target, failure, since);
                        continue;
                    }
                    Err(Failed::Sync(failure)) => {
                        scheduler.fail_config_sync(&mut book, id, &target, failure, since);
                        continue;
                    }
                }
            };
            let reply = match after {
                AfterSync::Offer(offer) => members.offer(&target, &offer).await,
                AfterSync::Unavailable(failure) => Err(failure),
            };
            let Some(scheduler) = scheduler.upgrade() else {
                return;
            };
            let mut book = scheduler.book();
            let Some(Entry {
                phase: Phase::Placing(placing),
                auto,
                record,
                expected_run,
                ..
            }) = book.entries.get_mut(id)
            else {
                // Withdrawn meanwhile; an admission the member made lapses there
                // when nobody attaches.
                continue;
            };
            let reply = match reply {
                Ok(OfferReply::Accepted {
                    workload_contract, ..
                }) if record.workload.is_some()
                    && workload_contract != Some(crate::workload_metadata::VERSION) =>
                {
                    Err(Failure::new(
                        "workload_contract_unsupported",
                        "The member did not acknowledge the workload metadata contract.",
                        "Install matching Elsecall versions before placing structured work.",
                    ))
                }
                other => other,
            };
            placing.in_flight = None;
            let placeable = auto.is_some();
            let admitted = HeldWork {
                run_id: id.clone(),
                kind: record.kind,
                label: record.label.clone(),
                size: record.size,
                running_ms: 0,
                expected_remaining_ms: expected_run.map(quote::millis),
            };
            match &reply {
                Ok(OfferReply::Accepted { state, .. }) => {
                    book.quotes.heard(&target, state, Some(admitted))
                }
                Ok(
                    OfferReply::Busy { state, .. }
                    | OfferReply::CopyBusy { state }
                    | OfferReply::TooLarge { state, .. },
                ) => book.quotes.heard(&target, state, None),
                Err(failure) => book.quotes.lost(&target, failure.clone()),
            }
            let retry_checkpoint = matches!(
                &reply,
                Ok(OfferReply::Busy { .. } | OfferReply::CopyBusy { .. })
            ) || placeable
                && matches!(&reply, Ok(OfferReply::TooLarge { .. }) | Err(_));
            if retry_checkpoint && let Some(entry) = book.entries.get(id) {
                scheduler.save(&entry.record);
            }
            let Some(Entry {
                phase: Phase::Placing(placing),
                ..
            }) = book.entries.get_mut(id)
            else {
                continue;
            };
            match reply {
                Ok(OfferReply::Accepted {
                    state,
                    copy,
                    location,
                    warmth,
                    ..
                }) => scheduler.placed(&mut book, id, state, copy, location, warmth),
                Ok(OfferReply::CopyBusy { state }) => {
                    placing.refusal = Some((state, None));
                    refused = true;
                }
                Ok(OfferReply::Busy { state, report }) => {
                    placing.refusal = Some((state, Some(report)));
                    refused = true;
                    break;
                }
                // Placement chooses again, without that member.
                Ok(OfferReply::TooLarge { state, .. }) if placeable => {
                    placing.refusal = Some((state, None));
                    refused = true;
                }
                Err(_) if placeable => refused = true,
                Ok(OfferReply::TooLarge { state, failure }) => scheduler.fail_placing(
                    &mut book,
                    id,
                    Verdict::RequestTooLarge,
                    failure,
                    Some(state),
                ),
                Err(failure) => {
                    let failure = match failure.code.as_str() {
                        "member_unavailable" | "member_rejected" | "member_unverified"
                        | "not_master" => {
                            Failure::new("member_unavailable", failure.message, failure.hint)
                        }
                        _ => failure,
                    };
                    scheduler.fail_placing(&mut book, id, Verdict::MemberUnavailable, failure, None)
                }
            }
        }
        if refused {
            let retry = Instant::now() + RETRY;
            if let Some(scheduler) = scheduler.upgrade() {
                scheduler.book().offer_retry.insert(target.clone(), retry);
            }
            tokio::time::sleep_until(retry).await;
            if let Some(scheduler) = scheduler.upgrade() {
                scheduler.book().offer_retry.remove(&target);
            }
        }
    }
}
