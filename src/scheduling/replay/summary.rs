//! What a replay reports: which decisions flip, what that would change by
//! estimate, and what could not be replayed and why.
use super::{
    Change,
    recorded::{Origin, Rescored, Unreplayable},
};
use crate::scheduling::{
    placement::Level,
    record::{Record, Verdict},
    run_id::RunId,
    timestamp::{Timestamp, seconds},
};
use serde::Serialize;
use std::{collections::BTreeMap, time::Duration};

/// One decision scored under its recorded parameters and under the change.
pub struct Replayed {
    pub origin: Origin,
    pub scored_at: Timestamp,
    pub recorded: Rescored,
    pub replayed: Rescored,
}

#[derive(Debug, Serialize)]
pub struct Replay {
    /// The changed values, as `section.key`.
    pub set: serde_json::Value,
    pub decisions: Decisions,
    pub effect: Effect,
    /// The decisions whose winner changes, oldest first: the latest
    /// `MAXIMUM_LISTED` of them; `decisions.flipped` counts them all.
    pub flips: Vec<Flip>,
    /// Whether more decisions flipped than `flips` lists.
    pub flips_truncated: bool,
    /// What replay does not account for.
    pub limits: Vec<&'static str>,
}

#[derive(Debug, Default, Serialize)]
pub struct Decisions {
    /// Placement decisions among the records read.
    pub placement: usize,
    pub replayed: usize,
    pub flipped: usize,
    pub replayed_from: ReplayedFrom,
    pub not_replayable: Vec<NotReplayable>,
}

#[derive(Debug, Default, Serialize)]
pub struct ReplayedFrom {
    pub arrival: usize,
    pub last_scoring: usize,
    pub recorded_estimates: usize,
}

#[derive(Debug, Serialize)]
pub struct NotReplayable {
    pub why: &'static str,
    pub count: usize,
    pub explanation: String,
    /// The latest `MAXIMUM_LISTED` decisions, except for those recorded before
    /// inputs, which are many and expected.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub run_ids: Vec<RunId>,
}

/// The most flips, and run IDs of each reason, one replay lists, so that its
/// answer stays well within what the CLI reads.
pub const MAXIMUM_LISTED: usize = 1000;

/// Aggregates over the replayed decisions.
#[derive(Debug, Default, Serialize)]
pub struct Effect {
    /// How many decisions each member won, recorded and replayed.
    pub winners: Vec<MemberCount>,
    /// Decisions no member could take in time.
    pub no_winner: Count,
    /// Decisions the calling member kept.
    pub kept_on_caller: Count,
    /// Decisions that failed with `fleet_busy`, and how many of them a member
    /// could take in time under the change.
    pub fleet_busy: FleetBusy,
    /// The replayed winner's expected completion against the recorded
    /// winner's, both under the changed parameters, over the flips.
    pub completion_difference_seconds: Difference,
}

#[derive(Debug, Serialize)]
pub struct MemberCount {
    pub member: String,
    pub recorded: usize,
    pub replayed: usize,
}

#[derive(Debug, Default, Serialize)]
pub struct Count {
    pub recorded: usize,
    pub replayed: usize,
}

#[derive(Debug, Default, Serialize)]
pub struct FleetBusy {
    pub recorded: usize,
    /// Of those, the ones whose replayed scoring had a winner as recorded: they
    /// failed later, as when the work they waited behind overran.
    pub had_a_winner: usize,
    /// Of those that had none, the ones that have one under the change.
    pub would_place: usize,
}

#[derive(Debug, Default, Serialize)]
pub struct Difference {
    /// Positive is later.
    pub total: f64,
    pub mean: Option<f64>,
    pub earlier: usize,
    pub later: usize,
    /// Flips where either winner has no expected completion, as when no
    /// member could take the work.
    pub unknown: usize,
}

#[derive(Debug, Serialize)]
pub struct Flip {
    pub run_id: RunId,
    pub label: Option<String>,
    pub requested_at: Timestamp,
    /// How the request ended up: started, or failed as with `fleet_busy`.
    pub verdict: Verdict,
    /// Where it ran.
    pub ran_on: Option<String>,
    pub replayed_from: Origin,
    pub scored_at: Timestamp,
    pub recorded: Choice,
    pub replayed: ReplayedChoice,
    /// `replayed.completion_seconds` minus
    /// `replayed.recorded_winner_completion_seconds`: positive is later.
    pub completion_difference_seconds: Option<f64>,
    /// What the recorded run took, measured from the scoring.
    pub actual: Option<Actual>,
}

#[derive(Debug, Serialize)]
pub struct Choice {
    pub winner: Option<String>,
    /// The winner's expected completion in that scoring.
    #[serde(with = "seconds::optional")]
    pub completion_seconds: Option<Duration>,
    /// Which level of history the winner's run estimate came from.
    pub run_level: Option<Level>,
    pub reason: String,
}

#[derive(Debug, Serialize)]
pub struct ReplayedChoice {
    #[serde(flatten)]
    pub choice: Choice,
    /// The recorded winner's expected completion under the changed parameters.
    #[serde(with = "seconds::optional")]
    pub recorded_winner_completion_seconds: Option<Duration>,
}

#[derive(Debug, Serialize)]
pub struct Actual {
    pub member: Option<String>,
    #[serde(with = "seconds::optional")]
    pub started_after_seconds: Option<Duration>,
    #[serde(with = "seconds::optional")]
    pub run_seconds: Option<Duration>,
    /// Until it ended, for work that completed.
    #[serde(with = "seconds::optional")]
    pub completion_seconds: Option<Duration>,
}

const NOT_SIMULATED: &str = "Queue effects are not simulated: a flipped decision would have changed the queues, waits, load and copies that later decisions saw, and each decision is re-scored against what it actually saw.";
const FIRST_SCORING: &str = "Each decision is replayed from its arrival scoring, where its request was sent first; records from before arrival inputs were kept replay their last scoring (replayed_from), whose winner a waiting request moved to only by the moving margin, so a last_scoring flip shows what that scoring preferred, not a move. How a waiting request would have moved when scored again is not replayed.";
const DIFFERENCE: &str = "completion_difference_seconds compares the replayed winner with the recorded winner, both estimated under the changed parameters; positive is later. actual is what the recorded run took.";
const ONE_MEMBER: &str = "Only this member's records: decisions made while another member was master are in that member's records.";
const GATES: &str = "admission settings derive every member's gate again from its recorded load, as if every member had set the value.";

fn seconds_between(later: Duration, earlier: Duration) -> f64 {
    ((later.as_secs_f64() - earlier.as_secs_f64()) * 1000.0).round() / 1000.0
}

pub struct Summary {
    set: serde_json::Value,
    gates: bool,
    decisions: Decisions,
    effect: Effect,
    winners: BTreeMap<String, (usize, usize)>,
    unreplayable: Vec<(Unreplayable, RunId)>,
    flips: Vec<Flip>,
}

impl Summary {
    pub fn new(change: &Change) -> Self {
        Self {
            set: serde_json::to_value(change).expect("settings serialize"),
            gates: change.changes_admission(),
            decisions: Decisions::default(),
            effect: Effect::default(),
            winners: BTreeMap::new(),
            unreplayable: Vec::new(),
            flips: Vec::new(),
        }
    }

    pub fn add(&mut self, record: &Record, result: Result<Replayed, Unreplayable>) {
        self.decisions.placement += 1;
        let replayed = match result {
            Ok(replayed) => replayed,
            Err(why) => return self.unreplayable.push((why, record.run_id.clone())),
        };
        self.decisions.replayed += 1;
        let from = &mut self.decisions.replayed_from;
        *match replayed.origin {
            Origin::Arrival => &mut from.arrival,
            Origin::LastScoring => &mut from.last_scoring,
            Origin::RecordedEstimates => &mut from.recorded_estimates,
        } += 1;
        let (before, after) = (&replayed.recorded, &replayed.replayed);
        for (rescored, recorded) in [(before, true), (after, false)] {
            let count = |count: &mut Count| {
                *if recorded {
                    &mut count.recorded
                } else {
                    &mut count.replayed
                } += 1;
            };
            match &rescored.winner {
                Some(winner) => {
                    let counts = self.winners.entry(winner.clone()).or_default();
                    *if recorded {
                        &mut counts.0
                    } else {
                        &mut counts.1
                    } += 1;
                    if rescored.caller.as_ref() == Some(winner) {
                        count(&mut self.effect.kept_on_caller);
                    }
                }
                None => count(&mut self.effect.no_winner),
            }
        }
        let verdict = record
            .decision
            .as_ref()
            .map_or(Verdict::Started, |d| d.verdict);
        if verdict == Verdict::FleetBusy {
            let busy = &mut self.effect.fleet_busy;
            busy.recorded += 1;
            match (&before.winner, &after.winner) {
                (Some(_), _) => busy.had_a_winner += 1,
                (None, Some(_)) => busy.would_place += 1,
                (None, None) => {}
            }
        }
        if before.winner == after.winner {
            return;
        }
        self.decisions.flipped += 1;
        let recorded_winner_now = after.completion(before.winner.as_deref());
        let replayed_completion = after.completion(after.winner.as_deref());
        let difference = match (replayed_completion, recorded_winner_now) {
            (Some(replayed), Some(recorded)) => Some(seconds_between(replayed, recorded)),
            _ => None,
        };
        let total = &mut self.effect.completion_difference_seconds;
        match difference {
            Some(d) => {
                total.total += d;
                if d < 0.0 {
                    total.earlier += 1;
                } else if d > 0.0 {
                    total.later += 1;
                }
            }
            None => total.unknown += 1,
        }
        self.flips.push(Flip {
            run_id: record.run_id.clone(),
            label: record.label.clone(),
            requested_at: record.requested_at,
            verdict,
            ran_on: record.member.clone(),
            replayed_from: replayed.origin,
            scored_at: replayed.scored_at,
            recorded: Choice {
                winner: before.winner.clone(),
                completion_seconds: before.completion(before.winner.as_deref()),
                run_level: before.run_level(before.winner.as_deref()),
                reason: before.reason.clone(),
            },
            replayed: ReplayedChoice {
                choice: Choice {
                    winner: after.winner.clone(),
                    completion_seconds: replayed_completion,
                    run_level: after.run_level(after.winner.as_deref()),
                    reason: after.reason.clone(),
                },
                recorded_winner_completion_seconds: recorded_winner_now,
            },
            completion_difference_seconds: difference,
            actual: record.started_at.map(|started| Actual {
                member: record.member.clone(),
                started_after_seconds: Some(started.since(replayed.scored_at)),
                run_seconds: record.run_ms.map(Duration::from_millis),
                completion_seconds: record
                    .ended_at
                    .filter(|_| record.outcome.as_ref().is_some_and(|o| o.completed()))
                    .map(|ended| ended.since(replayed.scored_at)),
            }),
        });
    }

    pub fn finish(mut self) -> Replay {
        self.effect.winners = self
            .winners
            .into_iter()
            .map(|(member, (recorded, replayed))| MemberCount {
                member,
                recorded,
                replayed,
            })
            .collect();
        let difference = &mut self.effect.completion_difference_seconds;
        let known = self.decisions.flipped - difference.unknown;
        difference.total = (difference.total * 1000.0).round() / 1000.0;
        difference.mean =
            (known > 0).then(|| (difference.total / known as f64 * 1000.0).round() / 1000.0);
        self.decisions.not_replayable = not_replayable(self.unreplayable);
        let mut limits = vec![NOT_SIMULATED, FIRST_SCORING, DIFFERENCE, ONE_MEMBER];
        if self.gates {
            limits.push(GATES);
        }
        let flips_truncated = self.flips.len() > MAXIMUM_LISTED;
        let mut flips = self.flips;
        flips.drain(..flips.len().saturating_sub(MAXIMUM_LISTED));
        Replay {
            set: self.set,
            decisions: self.decisions,
            effect: self.effect,
            flips,
            flips_truncated,
            limits,
        }
    }
}

/// The latest `MAXIMUM_LISTED` of `run_ids`.
fn latest(mut run_ids: Vec<RunId>) -> Vec<RunId> {
    run_ids.drain(..run_ids.len().saturating_sub(MAXIMUM_LISTED));
    run_ids
}

/// Unreplayable decisions grouped by why, in a stable order.
fn not_replayable(unreplayable: Vec<(Unreplayable, RunId)>) -> Vec<NotReplayable> {
    let mut groups: Vec<NotReplayable> = Vec::new();
    for (why, run_id) in unreplayable {
        let (code, explanation, listed) = match why {
            Unreplayable::RecordedBeforeInputs => (
                "recorded_before_inputs",
                "Recorded before decisions kept their inputs; only placement.keep_local_margin and placement.keep_local_margin_floor can be replayed for it.".to_owned(),
                false,
            ),
            Unreplayable::StartedElsewhereBeforeInputs => (
                "started_elsewhere_before_inputs",
                "Recorded before decisions kept their scoring's own winner, and it started on another member than its last scoring's estimates choose, as a waiting request kept by the moving margin does, so its choice cannot be checked or replayed.".to_owned(),
                true,
            ),
            Unreplayable::NotReproduced => (
                "not_reproduced",
                "Scored again under its recorded parameters, it did not reproduce its recorded decision, so it is not replayed. Its record or scoring has changed since; elsecall explain shows the record.".to_owned(),
                true,
            ),
            Unreplayable::GateNotReproduced => (
                "gate_not_reproduced",
                "A member's recorded gate does not follow from its recorded load and thresholds, as for a member on a version that reports none, so the gate cannot be derived again.".to_owned(),
                true,
            ),
            Unreplayable::InvalidParameters(why) => (
                "invalid_parameters",
                format!("Its recorded parameters with the changed values are not valid: {why}"),
                true,
            ),
        };
        match groups
            .iter_mut()
            .find(|g| g.why == code && g.explanation == explanation)
        {
            Some(group) => {
                group.count += 1;
                if listed {
                    group.run_ids.push(run_id);
                }
            }
            None => groups.push(NotReplayable {
                why: code,
                count: 1,
                explanation,
                run_ids: if listed { vec![run_id] } else { Vec::new() },
            }),
        }
    }
    for group in &mut groups {
        group.run_ids = latest(std::mem::take(&mut group.run_ids));
    }
    groups
}
