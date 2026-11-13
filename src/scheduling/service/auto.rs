//! Placeable requests on the master: each goes to the member with the earliest
//! expected completion, and is reconsidered every `reconsider_every` (a second by
//! default) until it starts.
//!
//! A request is always somewhere: in this member's own queue, in the queue of
//! requests this master offers to another member, or, while no member can take
//! it within its hold limit, undecided. Each second the master asks every other
//! member for a quote and scores the request again, oldest first. Committed work
//! moves only when its current place can no longer start it in time, or another
//! member can start it now and completes earlier by the moving margin, so a
//! request does not flap between near equals. When its hold limit passes, it
//! fails with a `fleet_busy` that covers every member.
//!
//! A member whose pre-run configuration sync failed is left out of the request's
//! placement from then on, and the request is placed again at once, within the
//! same hold limit. The caller needs no sync, so it always remains a candidate.
use super::{Book, Entry, Phase, Scheduler, Waiting, placing::Placing, running_estimates};
use crate::{
    failure::Failure,
    scheduling::{
        admission::Class,
        busy::{self, MemberSituation, RunningOn},
        estimates,
        learning::{self, RunHistory, ratios::SpeedRatios},
        loss_history::LossHistory,
        placed::WorkspaceRef,
        placement::{
            self, Ahead, Copy, Expectation, Held, Learned, LearnedRun, Observation, Start,
            Unreachable, Wanted,
        },
        pre_run_sync::{Fallback, PreRunSyncRecord, SyncFailure},
        quote::{self, MAXIMUM_WORKSPACES, Quote, QuoteRequest},
        record::{Arrival, Candidate, Decision, Scoring, Verdict},
        request::Kind,
        run_id::RunId,
        sync_history::{self, SyncHistory},
        timestamp::Timestamp,
    },
};
use futures_util::future::join_all;
use std::{collections::BTreeSet, sync::Weak, time::Duration};
use tokio::time::Instant;

/// A member that could not be asked is not asked again for a new request this
/// soon, so an unreachable member does not delay every placement by the quote
/// timeout; the second-by-second reconsideration still asks it.
const UNREACHABLE_REUSE: Duration = Duration::from_secs(10);
/// How long a new request waits for quotes. A member that answers later is
/// scored on its earlier answer, and its new one serves the next reconsideration.
const NEW_REQUEST_WAIT: Duration = Duration::from_millis(750);
/// Placed-run records read for sync history.
const SYNC_RECORDS_READ: u32 = 500;

/// What a placeable request carries while it waits on the master.
pub(super) struct Auto {
    pub workspace: WorkspaceRef,
    pub runs: RunHistory,
    pub syncs: SyncHistory,
    pub losses: LossHistory,
    /// The latest scoring, for the decision record and `fleet_busy`.
    pub verdict: Option<(placement::Verdict, Scoring)>,
    /// What the first scoring expected of each candidate.
    pub arrival: Option<Arrival>,
    /// Members whose pre-run configuration sync failed for this request.
    pub excluded: BTreeSet<String>,
}

/// Where a waiting placeable request is now.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Place {
    Undecided,
    Here,
    Member(String),
}

impl Scheduler {
    /// History for a request's estimates: its label's runs, its family's (the
    /// runs of the label rule it matched), recent runs of every label for speed
    /// ratios, every recent sync, and the placed runs other members lost.
    pub(super) fn auto_history(
        &self,
        label: Option<&str>,
        family: Option<&str>,
        now: Timestamp,
    ) -> (RunHistory, SyncHistory, LossHistory) {
        let trust = &self.parameters().estimates;
        let runs = match label {
            Some(label) => self.run_history(Kind::Run, label, family, now),
            None => RunHistory::default(),
        };
        // Placed runs as far back as either history looks; each takes its own.
        let placed_since = Timestamp::from_millis(
            now.millis()
                .saturating_sub(trust.recency.max(trust.loss_window).as_millis() as u64),
        );
        let (syncs, losses) = match self.records.placed_since(placed_since, SYNC_RECORDS_READ) {
            Ok(records) => (
                SyncHistory::new(sync_history::samples(&records, now, trust.recency), trust),
                LossHistory::new(&records, now, trust.loss_window),
            ),
            Err(error) => {
                super::log("sync_history_unavailable", &error);
                (SyncHistory::default(), LossHistory::default())
            }
        };
        (runs, syncs, losses)
    }

    /// History of `label`'s requests of `kind`: its own, its family's (the
    /// requests of the label rule it matched) and speed ratios between members,
    /// which runs teach.
    pub(super) fn run_history(
        &self,
        kind: Kind,
        label: &str,
        family: Option<&str>,
        now: Timestamp,
    ) -> RunHistory {
        let trust = &self.parameters().estimates;
        let since = Timestamp::from_millis(
            now.millis()
                .saturating_sub(trust.recency.as_millis() as u64),
        );
        let samples =
            |kind: Kind, read: crate::failure::Result<Vec<crate::scheduling::record::Record>>| {
                match read {
                    Ok(records) => learning::samples_of(kind, &records, now, trust.recency),
                    Err(error) => {
                        super::log("run_history_unavailable", &error);
                        Vec::new()
                    }
                }
            };
        let own = samples(
            kind,
            self.records
                .history(label, since, learning::records_read(trust)),
        );
        let kin = family.map_or_else(Vec::new, |family| {
            samples(
                kind,
                self.records.family_history(
                    family,
                    label,
                    trust.short_run,
                    since,
                    learning::records_read(trust),
                ),
            )
        });
        let ratios = self.ratios.get(Instant::now(), || {
            let fleet = samples(
                Kind::Run,
                self.records
                    .labelled_since(since, learning::RATIO_RECORDS_READ),
            );
            SpeedRatios::learn(&fleet, trust)
        });
        RunHistory::new(label, family, own, kin, ratios, trust)
    }

    /// Asks every other member for a quote about `new`, a new request's
    /// workspace, and the workspaces of waiting placeable requests. For a new
    /// request, a member that recently could not be asked is skipped.
    pub(super) async fn refresh(&self, new: Option<&WorkspaceRef>) {
        let standing = self.members.standing();
        let own = self.name();
        let (mut workspaces, skipped) = {
            let book = self.book();
            let waiting: Vec<WorkspaceRef> = book
                .entries
                .values()
                .filter(|e| !matches!(e.phase, Phase::Active { .. }))
                .filter_map(|e| e.auto.as_ref().map(|a| a.workspace.clone()))
                .collect();
            let skipped: Vec<String> = standing
                .members
                .iter()
                .filter(|m| new.is_some() && book.quotes.failed_recently(m, UNREACHABLE_REUSE))
                .cloned()
                .collect();
            (waiting, skipped)
        };
        workspaces.sort_by(|a, b| a.root.cmp(&b.root));
        // The new request's workspace first, so a bound never drops it.
        workspaces.splice(0..0, new.cloned());
        let mut seen = std::collections::HashSet::new();
        workspaces.retain(|w| seen.insert(w.root.clone()));
        workspaces.truncate(MAXIMUM_WORKSPACES);
        let request = QuoteRequest {
            workspaces: workspaces.clone(),
        };
        let others: Vec<String> = standing.members.into_iter().filter(|m| *m != own).collect();
        let asked: Vec<&String> = others.iter().filter(|m| !skipped.contains(m)).collect();
        let asked_at = Instant::now();
        let wait = if new.is_some() {
            NEW_REQUEST_WAIT
        } else {
            Duration::MAX
        };
        let quotes = join_all(asked.iter().map(|m| {
            let quote = self.members.quote(m, &request);
            async move { tokio::time::timeout(wait, quote).await.ok() }
        }))
        .await;
        let mut book = self.book();
        book.quotes.retain(&others);
        for (member, quote) in asked.into_iter().zip(quotes) {
            // Not answered in time for a new request: keep the earlier answer.
            if let Some(quote) = quote {
                book.quotes
                    .record(member, asked_at, quote.map(|q| q.believed(&workspaces)));
            }
        }
        drop(book);
        self.learn_running();
    }

    /// Queues a new placeable request and places it as well as it can be now.
    pub(super) fn place_auto(&self, book: &mut Book, mut entry: Entry, placing: Placing) {
        let id = entry.record.run_id.clone();
        entry.phase = Phase::Placing(Box::new(placing));
        self.persist(&mut entry);
        book.entries.insert(id.clone(), entry);
        self.settle(book, &id);
    }

    /// Scores every waiting placeable request again, oldest first.
    pub(super) fn reconsider(&self, book: &mut Book) {
        let mut waiting: Vec<(Timestamp, RunId)> = book
            .entries
            .iter()
            .filter(|(_, e)| e.auto.is_some() && !matches!(e.phase, Phase::Active { .. }))
            .map(|(id, e)| (e.record.requested_at, id.clone()))
            .collect();
        waiting.sort();
        for (_, id) in waiting {
            self.settle(book, &id);
        }
        self.pump(book);
    }

    fn place_of(&self, entry: &Entry) -> Option<Place> {
        match &entry.phase {
            Phase::Waiting(_) => Some(Place::Here),
            Phase::Placing(placing) => Some(match &placing.target {
                Some(target) => Place::Member(target.clone()),
                None => Place::Undecided,
            }),
            Phase::Active { .. } => None,
        }
    }

    /// Scores one waiting placeable request and moves it if it should move.
    fn settle(&self, book: &mut Book, id: &RunId) {
        let Some((verdict, scoring)) = self.score(book, id) else {
            return;
        };
        let margin = |current| self.parameters().placement.moving_margin(current);
        let own = self.name();
        let Some(entry) = book.entries.get_mut(id) else {
            return;
        };
        let Some(place) = self.place_of(entry) else {
            return;
        };
        let in_flight = matches!(&entry.phase, Phase::Placing(p) if p.in_flight.is_some());
        let too_large = verdict
            .candidates
            .iter()
            .all(|c| matches!(c.estimate.start, Start::Never { .. }));
        let winner = verdict.winner.clone();
        let next = match (&place, winner) {
            (_, None) => None,
            (Place::Undecided, Some(winner)) => Some(winner),
            (Place::Here, Some(winner)) if winner == own => None,
            (Place::Member(current), Some(winner)) if *current == winner => None,
            (place, Some(winner)) => {
                let current = match place {
                    Place::Here => own.as_str(),
                    Place::Member(member) => member.as_str(),
                    Place::Undecided => unreachable!("matched above"),
                };
                let current = verdict.scored(current).map(|c| &c.estimate);
                let better = verdict.scored(&winner).map(|c| &c.estimate);
                let moves = match (current, better) {
                    (_, None) => false,
                    (None, Some(_)) => true,
                    (Some(current), Some(better)) => {
                        !current.eligible
                            || (better.start == Start::Now
                                && match (current.completion_seconds, better.completion_seconds) {
                                    (Some(now), Some(then)) => then + margin(now) <= now,
                                    _ => false,
                                })
                    }
                };
                moves.then_some(winner)
            }
        };
        if let Some(auto) = entry.auto.as_mut() {
            auto.arrival
                .get_or_insert_with(|| Arrival::of(&verdict, &scoring));
            auto.verdict = Some((verdict, scoring));
        }
        if too_large && place == Place::Undecided {
            let failure = Failure::new(
                "request_too_large",
                format!(
                    "Size {} is larger than every reachable member's capacity.",
                    entry.record.size
                ),
                "Request a smaller --size, or lower reserved_cores on a member.",
            );
            let decision =
                self.auto_decision(entry, Verdict::RequestTooLarge, None, &failure.message);
            return self.fail_with(book, id, failure, decision);
        }
        match next {
            _ if in_flight => (),
            Some(member) if member == own => self.move_here(book, id),
            Some(member) => self.move_to(book, id, member),
            None => (),
        }
    }

    /// Moves a waiting placeable request into this member's own queue.
    fn move_here(&self, book: &mut Book, id: &RunId) {
        let Some(entry) = book.entries.get_mut(id) else {
            return;
        };
        let placeholder = Phase::Active {
            holders: Default::default(),
        };
        let Phase::Placing(placing) = std::mem::replace(&mut entry.phase, placeholder) else {
            return;
        };
        let placing = *placing;
        if let Some(target) = &placing.target
            && let Some(queue) = book.placing.get_mut(target)
        {
            queue.retain(|queued| queued != id);
        }
        entry.phase = Phase::Waiting(Waiting {
            requester: placing.requester,
            holder: placing.requester,
            expires_at: Some(placing.expires_at),
            responder: placing.responder,
            timer: Some(placing.timer),
        });
        let size = entry.record.size;
        self.persist(entry);
        book.admission.enqueue(id.clone(), size, Class::Interactive);
    }

    /// Moves a waiting placeable request to the queue of requests offered to
    /// `member`.
    fn move_to(&self, book: &mut Book, id: &RunId, member: String) {
        let Some(entry) = book.entries.get_mut(id) else {
            return;
        };
        let Some(workspace) = entry.auto.as_ref().map(|a| a.workspace.clone()) else {
            return;
        };
        let placeholder = Phase::Active {
            holders: Default::default(),
        };
        let placing = match std::mem::replace(&mut entry.phase, placeholder) {
            Phase::Waiting(waiting) => {
                book.admission.withdraw(id);
                Placing {
                    requester: waiting.requester,
                    target: Some(member.clone()),
                    workspace,
                    expires_at: waiting
                        .expires_at
                        .expect("interactive work has a hold limit"),
                    responder: waiting.responder,
                    timer: waiting.timer.expect("interactive work has a timer"),
                    refusal: None,
                    in_flight: None,
                    config_sync: None,
                    offered: false,
                    pre_run_recorded: false,
                }
            }
            Phase::Placing(placing) => {
                let mut placing = *placing;
                if let Some(target) = &placing.target
                    && let Some(queue) = book.placing.get_mut(target)
                {
                    queue.retain(|queued| queued != id);
                }
                placing.target = Some(member.clone());
                placing.refusal = None;
                placing.config_sync = None;
                placing.offered = false;
                placing
            }
            active @ Phase::Active { .. } => {
                entry.phase = active;
                return;
            }
        };
        entry.phase = Phase::Placing(Box::new(placing));
        self.persist(entry);
        self.queue_for(book, id, member);
    }

    /// The pre-run sync to `target`, where placement sent placeable request `id`,
    /// failed. Placement leaves `target` out and places the request again at
    /// once, within its hold limit; the caller, which needs no sync, always
    /// remains. Only if no member left could ever take it does the run fail
    /// with `config_sync_failed`.
    pub(super) fn fall_back(
        &self,
        book: &mut Book,
        id: &RunId,
        target: &str,
        failure: SyncFailure,
        since: Instant,
    ) {
        let Some(auto) = book.entries.get_mut(id).and_then(|e| e.auto.as_mut()) else {
            return;
        };
        auto.excluded.insert(target.to_owned());
        let Some((verdict, scoring)) = self.score(book, id) else {
            return;
        };
        // A member not asked yet, or not answering now, may still take it.
        let left = !verdict.unreachable.is_empty()
            || verdict
                .candidates
                .iter()
                .any(|c| !matches!(c.estimate.start, Start::Never { .. }));
        if !left {
            // The decision shows the candidates that were left: none.
            if let Some(auto) = book.entries.get_mut(id).and_then(|e| e.auto.as_mut()) {
                auto.verdict = Some((verdict, scoring));
            }
            return self.fail_config_sync(book, id, target, failure, since);
        }
        let failed_sync = PreRunSyncRecord::failed(
            target,
            quote::millis(since.elapsed()),
            failure.cause.clone(),
        );
        let Some(Entry {
            phase: Phase::Placing(placing),
            ..
        }) = book.entries.get_mut(id)
        else {
            return;
        };
        // Undecided until scored again, which is now.
        placing.target = None;
        placing.in_flight = None;
        placing.refusal = None;
        placing.config_sync = None;
        placing.offered = false;
        if let Some(queue) = book.placing.get_mut(target) {
            queue.retain(|queued| queued != id);
        }
        self.settle(book, id);
        let own = self.name();
        let Some(entry) = book.entries.get_mut(id) else {
            // Settling failed it.
            return self.pump(book);
        };
        let moved_to = match self.place_of(entry) {
            Some(Place::Here) => Some(own),
            Some(Place::Member(member)) => Some(member),
            Some(Place::Undecided) | None => None,
        };
        let reason = match (&moved_to, entry.auto.as_ref().and_then(|a| a.verdict.as_ref())) {
            (Some(member), Some((verdict, _))) if verdict.winner.as_ref() == Some(member) => {
                verdict.reason.clone()
            }
            (Some(member), _) => format!("{member} could start it first."),
            (None, _) => "No remaining member could start it in time yet; it waits for one within its hold limit.".into(),
        };
        entry.record.fallbacks.push(Fallback {
            failed_sync,
            decided_at: self.clock.now(),
            moved_to,
            reason,
        });
        self.persist(entry);
        self.pump(book);
    }

    /// What every member looks like for request `id`, and its verdict.
    pub(super) fn score(&self, book: &Book, id: &RunId) -> Option<(placement::Verdict, Scoring)> {
        let entry = book.entries.get(id)?;
        let auto = entry.auto.as_ref()?;
        let expires_at = match &entry.phase {
            Phase::Waiting(waiting) => waiting.expires_at?,
            Phase::Placing(placing) => placing.expires_at,
            Phase::Active { .. } => return None,
        };
        let now = self.clock.now();
        let own = self.name();
        let mut observations = vec![self.observe_here(book, id, &own, now)];
        let mut unreachable = Vec::new();
        for member in self.members.standing().members {
            if member == own || auto.excluded.contains(&member) {
                continue;
            }
            match book.quotes.answer(&member) {
                Some(Ok(quote)) => observations.push(self.observe_member(
                    book,
                    id,
                    &member,
                    quote,
                    &auto.workspace,
                )),
                Some(Err(failure)) => unreachable.push(Unreachable {
                    member,
                    why: unreachable_why(failure),
                    peer_io: failure
                        .details
                        .as_ref()
                        .and_then(|d| serde_json::from_value(d["peer_io"].clone()).ok()),
                }),
                None => unreachable.push(Unreachable {
                    member,
                    why: "Not asked yet.".into(),
                    peer_io: None,
                }),
            }
        }
        placement::learn(
            &mut observations,
            &auto.workspace,
            &auto.runs,
            &auto.syncs,
            &auto.losses,
        );
        let wanted = Wanted {
            size: entry.record.size,
            hold_left: expires_at.since(now),
            parameters: &self.parameters().placement,
        };
        let verdict = placement::decide(observations, unreachable, &wanted);
        let scoring = Scoring {
            scored_at: now,
            size: wanted.size,
            hold_left: wanted.hold_left,
            winner: verdict.winner.clone(),
        };
        Some((verdict, scoring))
    }

    fn observe_here(&self, book: &Book, id: &RunId, own: &str, now: Timestamp) -> Observation {
        let holding = self
            .holding(book)
            .into_iter()
            .map(|work| {
                let elapsed = now.since(work.started_at.unwrap_or(now));
                Held {
                    size: work.size,
                    elapsed: Some(elapsed),
                    expectation: expectation(
                        work.expected_run.map(|run| run.saturating_sub(elapsed)),
                        book.running.of(own, &work.run_id),
                    ),
                }
            })
            .collect();
        let ahead = book
            .admission
            .waiting_interactive()
            .take_while(|(queued, _)| *queued != id)
            .map(|(queued, size)| Ahead {
                size,
                run: book.entries.get(queued).and_then(|e| e.expected_run),
            })
            .collect();
        let mut state = self.state_of(book);
        state.member = own.to_owned();
        Observation {
            state,
            caller: true,
            utilization: book.load.cpu_utilization(),
            holding,
            ahead,
            copy: Copy::Caller,
            learned: Learned::default(),
            admission: Some(self.parameters().admission.clone()),
        }
    }

    fn observe_member(
        &self,
        book: &Book,
        id: &RunId,
        member: &str,
        quote: &Quote,
        workspace: &WorkspaceRef,
    ) -> Observation {
        // The member's own queue goes first; then what this master offers it
        // before this request.
        let mut ahead: Vec<Ahead> = quote
            .waiting
            .iter()
            .map(|w| Ahead {
                size: w.size,
                run: w.expected_run_ms.map(Duration::from_millis),
            })
            .collect();
        if let Some(queue) = book.placing.get(member) {
            ahead.extend(
                queue
                    .iter()
                    .take_while(|queued| *queued != id)
                    .filter_map(|queued| book.entries.get(queued))
                    .map(|e| Ahead {
                        size: e.record.size,
                        run: e.expected_run,
                    }),
            );
        }
        let mut state = quote.state.clone();
        state.member = member.to_owned();
        Observation {
            state,
            caller: false,
            utilization: quote.utilization,
            holding: quote
                .holding
                .iter()
                .map(|h| Held {
                    size: h.size,
                    elapsed: Some(running_estimates::command_elapsed(
                        book,
                        member,
                        &h.run_id,
                        Duration::from_millis(h.running_ms),
                    )),
                    expectation: expectation(
                        h.expected_remaining_ms.map(Duration::from_millis),
                        book.running.of(member, &h.run_id),
                    ),
                })
                .collect(),
            ahead,
            copy: match book.quotes.copy(member, &workspace.root) {
                Some(copy) if copy.unavailable => Copy::Unavailable,
                Some(copy) => Copy::Remote {
                    warmth: copy.warmth,
                    busy: copy.busy,
                },
                // Not asked about yet: assume the worst.
                None => Copy::Remote {
                    warmth: crate::scheduling::placed::Warmth::Cold,
                    busy: false,
                },
            },
            learned: Learned::default(),
            admission: quote.admission.clone(),
        }
    }

    /// The decision record for a placeable request, from its latest scoring.
    pub(super) fn auto_decision(
        &self,
        entry: &Entry,
        outcome: Verdict,
        chosen: Option<&str>,
        reason: &str,
    ) -> Decision {
        let now = self.clock.now();
        let parameters = Some(self.parameters().clone());
        let arrival = entry.auto.as_ref().and_then(|a| a.arrival.clone());
        let reason = &with_fallbacks(reason, &entry.record.fallbacks);
        let Some((verdict, scoring)) = entry.auto.as_ref().and_then(|a| a.verdict.as_ref()) else {
            return Decision {
                decided_at: now,
                verdict: outcome,
                reason: reason.into(),
                candidates: Vec::new(),
                unreachable: Vec::new(),
                parameters,
                scoring: None,
                arrival,
            };
        };
        Decision {
            decided_at: now,
            verdict: outcome,
            reason: reason.into(),
            candidates: verdict
                .candidates
                .iter()
                .map(|scored| {
                    let member = scored.observation.state.member.as_str();
                    Candidate::scored(scored, Some(member) == chosen, now)
                })
                .collect(),
            unreachable: verdict.unreachable.clone(),
            parameters,
            scoring: Some(scoring.clone()),
            arrival,
        }
    }

    /// The reason recorded when a placeable request starts on `member`.
    pub(super) fn start_reason(entry: &Entry, member: &str) -> String {
        match entry.auto.as_ref().and_then(|a| a.verdict.as_ref()) {
            Some((verdict, _)) if verdict.winner.as_deref() == Some(member) => {
                verdict.reason.clone()
            }
            _ => format!("{member} could start it first."),
        }
    }

    /// Ends waiting work with a failure and its decision, answering its caller.
    pub(super) fn fail_with(
        &self,
        book: &mut Book,
        id: &RunId,
        failure: Failure,
        decision: Decision,
    ) {
        let failure = failure.for_run(id);
        let outcome = crate::scheduling::record::Outcome::Failed {
            code: failure.code.clone(),
        };
        if let Some(responder) = self
            .close(book, id, outcome, Some(decision))
            .and_then(Phase::responder)
        {
            let _ = responder.send(Err(failure));
        }
        self.pump(book);
    }

    /// The hold limit of a placeable request passed: `fleet_busy` across members.
    pub(super) fn expire_auto(&self, book: &mut Book, id: &RunId) {
        let verdict = self.score(book, id);
        let Some(entry) = book.entries.get_mut(id) else {
            return;
        };
        if let (Some(auto), Some((verdict, scoring))) = (entry.auto.as_mut(), verdict) {
            auto.arrival
                .get_or_insert_with(|| Arrival::of(&verdict, &scoring));
            auto.verdict = Some((verdict, scoring));
        }
        let entry = &book.entries[id];
        let limit = entry
            .record
            .hold_limit
            .map_or(Duration::ZERO, |l| l.seconds);
        let own = self.name();
        let now = self.clock.now();
        let mut running: Vec<RunningOn> = self
            .holding(book)
            .into_iter()
            .map(|work| RunningOn::of(&own, work, now))
            .collect();
        running.extend(book.quotes.holding().map(|(member, h)| RunningOn {
            member: member.to_owned(),
            run_id: h.run_id.clone(),
            kind: h.kind,
            label: h.label.clone(),
            size: h.size,
            running_seconds: h.running_ms / 1000,
            expected_remaining_seconds: h.expected_remaining_ms.map(|ms| ms / 1000),
        }));
        let mut members: Vec<MemberSituation> = Vec::new();
        if let Some((verdict, _)) = entry.auto.as_ref().and_then(|a| a.verdict.as_ref()) {
            for scored in &verdict.candidates {
                let estimate = &scored.estimate;
                members.push(MemberSituation {
                    member: scored.observation.state.member.clone(),
                    why: scored.why.clone(),
                    retry: match &estimate.start {
                        Start::Now | Start::AfterWait => estimate
                            .wait_seconds
                            .map(|wait| wait + estimate.sync_seconds),
                        Start::Blocked { .. } => Some(estimates::BLOCKED_RETRY),
                        Start::Never { .. } => None,
                    },
                });
            }
            for unreachable in &verdict.unreachable {
                members.push(MemberSituation {
                    member: unreachable.member.clone(),
                    why: format!("Unreachable: {}", unreachable.why),
                    retry: None,
                });
            }
        }
        for fallback in &entry.record.fallbacks {
            members.push(MemberSituation {
                member: fallback.failed_sync.member.clone(),
                why: format!("Left out: {}", sync_failed(&fallback.failed_sync)),
                retry: None,
            });
        }
        let failure = busy::fleet_busy_across(limit, members, running);
        let decision = self.auto_decision(entry, Verdict::FleetBusy, None, &failure.message);
        self.fail_with(book, id, failure, decision);
    }
}

/// Reconsiders waiting placeable requests every second, asking members for fresh
/// quotes first, while there are any.
pub(super) async fn reconsider(scheduler: Weak<Scheduler>) {
    let every = match scheduler.upgrade() {
        Some(scheduler) => scheduler.parameters().placement.reconsider_every,
        None => return,
    };
    let mut interval = tokio::time::interval(every);
    loop {
        interval.tick().await;
        let Some(scheduler) = scheduler.upgrade() else {
            return;
        };
        scheduler.reconsider_waiting().await;
    }
}

/// Why a failed pre-run sync left its member out, by its cause code only.
fn sync_failed(sync: &PreRunSyncRecord) -> String {
    match &sync.outcome {
        crate::scheduling::pre_run_sync::Recorded::Failed { cause } => {
            format!("its synced configuration could not be brought up to date ({cause}).")
        }
        _ => "its synced configuration could not be brought up to date.".into(),
    }
}

/// `reason`, followed by the members placement left out after their pre-run
/// sync failed, if any.
fn with_fallbacks(reason: &str, fallbacks: &[Fallback]) -> String {
    let mut text = reason.to_owned();
    for fallback in fallbacks {
        text.push_str(&format!(
            " {} was left out: {}",
            fallback.failed_sync.member,
            sync_failed(&fallback.failed_sync)
        ));
    }
    text
}

/// What is expected of running work: what is left of its member's expected
/// run time, else what history expects of its whole run, else nothing.
fn expectation(remaining: Option<Duration>, learned: Option<&LearnedRun>) -> Expectation {
    match (remaining, learned) {
        (Some(remaining), _) => Expectation::Member { remaining },
        (None, Some(learned)) => Expectation::Learned(learned.clone()),
        (None, None) => Expectation::Unknown,
    }
}

/// Why a member could not be asked, in this master's words: a member's own
/// message is not quoted, since it may hold its paths.
fn unreachable_why(failure: &Failure) -> String {
    match failure.code.as_str() {
        "member_unavailable" => "It did not answer.".into(),
        "not_master" | "member_rejected" | "member_unverified" => {
            "It does not accept this member's placement authority.".into()
        }
        code if !code.is_empty()
            && code.len() <= 64
            && code.bytes().all(|b| b.is_ascii_lowercase() || b == b'_') =>
        {
            format!("It could not give a quote ({code}).")
        }
        _ => "It could not give a quote.".into(),
    }
}
