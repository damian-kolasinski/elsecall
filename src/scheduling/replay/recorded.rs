//! What a record lets replay score again, and scoring it under parameters.
//!
//! A scoring with inputs is scored by `placement::decide` itself, from each
//! candidate's recorded state and inputs; one recorded before inputs only by
//! `placement::pick`, from the recorded estimates. Either way, scoring under
//! the recorded parameters must first reproduce the recorded verdict exactly.
use super::Change;
use crate::scheduling::{
    capacity::Cores,
    load::{self, Gate},
    messages::MemberState,
    parameters::{Admission, Parameters},
    placement::{self, Contender, Estimate, Level, Observation, Scored, Wanted},
    record::{ArrivalEstimate, Candidate, Record},
    request::Placement,
    timestamp::Timestamp,
};
use serde::{Serialize, de::DeserializeOwned};
use std::time::Duration;

/// Which recorded scoring a decision was replayed from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    /// The scoring on arrival, which decided where the request went first.
    Arrival,
    /// The last scoring, before the request started or failed, for records
    /// from before arrival inputs were kept.
    LastScoring,
    /// The recorded estimates of the last scoring, for records from before
    /// inputs were kept: only the keep-local margin can change their choice.
    RecordedEstimates,
}

/// Why a decision could not be replayed.
#[derive(Clone, Debug, PartialEq)]
pub enum Unreplayable {
    /// The change needs inputs the record does not hold.
    RecordedBeforeInputs,
    /// Recorded before the scoring's own winner was: the request started on
    /// another member than its last scoring chose, as a waiting request can when
    /// the moving margin keeps it, so that choice cannot be checked.
    StartedElsewhereBeforeInputs,
    /// Scored under its recorded parameters, it did not reproduce its verdict.
    NotReproduced,
    /// A member's gate does not follow from its recorded load and thresholds.
    GateNotReproduced,
    /// The recorded parameters with the change are not valid.
    InvalidParameters(String),
}

/// One recorded scoring of a placement decision.
pub struct RecordedScoring {
    pub scored_at: Timestamp,
    /// The parameters it was decided with; the defaults for records from
    /// before parameters were recorded, which used them.
    pub parameters: Parameters,
    winner: Option<String>,
    scoring: Scoring,
}

enum Scoring {
    Inputs(WithInputsScoring),
    Estimates(EstimatesScoring),
}

/// A scoring whose record holds every input: scored by `placement::decide`.
struct WithInputsScoring {
    arrival: bool,
    size: Cores,
    hold_left: Duration,
    candidates: Vec<WithInputs>,
}

/// A scoring recorded before inputs were: only its choice between the
/// recorded completions can be repeated, by `placement::pick`.
struct EstimatesScoring {
    hold_left: Duration,
    calling_member: String,
    candidates: Vec<(String, Estimate)>,
}

struct WithInputs {
    observation: Observation,
    expected: Expected,
}

/// What the record says the scoring expected of a candidate.
enum Expected {
    /// Without its state and inputs, which the observation holds.
    Arrival(Box<ArrivalEstimate>),
    Last {
        estimate: Box<Estimate>,
        why: String,
    },
}

/// The verdict of one scoring, as replay compares them.
#[derive(Clone, Debug, PartialEq)]
pub struct Rescored {
    pub winner: Option<String>,
    pub reason: String,
    /// The calling member, when it was a candidate.
    pub caller: Option<String>,
    pub completions: Vec<(String, Option<Duration>)>,
    /// Which level of history each candidate's run estimate came from.
    pub run_levels: Vec<(String, Option<Level>)>,
}
impl Rescored {
    pub fn completion(&self, member: Option<&str>) -> Option<Duration> {
        let member = member?;
        self.completions
            .iter()
            .find(|(m, _)| m == member)
            .and_then(|(_, c)| *c)
    }

    pub fn run_level(&self, member: Option<&str>) -> Option<Level> {
        let member = member?;
        self.run_levels
            .iter()
            .find(|(m, _)| m == member)
            .and_then(|(_, l)| *l)
    }

    fn of(verdict: placement::Verdict) -> Self {
        Self {
            caller: verdict
                .candidates
                .iter()
                .find(|c| c.observation.caller)
                .map(|c| c.observation.state.member.clone()),
            completions: verdict
                .candidates
                .iter()
                .map(|c| {
                    (
                        c.observation.state.member.clone(),
                        c.estimate.completion_seconds,
                    )
                })
                .collect(),
            run_levels: verdict
                .candidates
                .iter()
                .map(|c| {
                    (
                        c.observation.state.member.clone(),
                        c.estimate.run_from.level(),
                    )
                })
                .collect(),
            winner: verdict.winner,
            reason: verdict.reason,
        }
    }
}

/// A candidate's member state as its record keeps it. The speed is in its inputs.
fn state_of(candidate: &Candidate) -> MemberState {
    MemberState {
        member: candidate.member.clone(),
        capacity: candidate.capacity,
        in_use: candidate.in_use,
        free: candidate.free,
        queued: candidate.queued,
        load: candidate.load.clone(),
        gate: candidate.gate,
        speed: None,
    }
}

/// `value` as it reads back from a record, where durations are fractional
/// seconds.
fn as_recorded<T: Serialize + DeserializeOwned>(value: &T) -> Option<T> {
    serde_json::from_str(&serde_json::to_string(value).ok()?).ok()
}

/// The gate a member's own service derives from its recorded load under
/// `admission`.
fn gate_under(observation: &Observation, admission: &Admission) -> Gate {
    let state = &observation.state;
    load::gate_of(
        state.load.as_ref(),
        observation.utilization,
        state.in_use,
        state.capacity.weighted_cores,
        admission,
    )
}

impl Expected {
    fn matches(&self, scored: &Scored) -> bool {
        match self {
            Expected::Arrival(recorded) => {
                let expected = ArrivalEstimate {
                    state: None,
                    inputs: None,
                    ..ArrivalEstimate::of(scored)
                };
                as_recorded(&expected).as_ref() == Some(&**recorded)
            }
            Expected::Last { estimate, why } => {
                as_recorded(&scored.estimate).as_ref() == Some(&**estimate) && scored.why == *why
            }
        }
    }
}

impl RecordedScoring {
    /// The scoring of `record` that replay repeats, or `None` when the record
    /// holds no placement decision.
    pub fn of(record: &Record) -> Option<Self> {
        if record.placement != Placement::Placeable {
            return None;
        }
        let decision = record.decision.as_ref()?;
        let parameters = decision.parameters.clone().unwrap_or_default();
        if let Some(arrival) = &decision.arrival
            && let (Some(size), Some(hold_left)) = (arrival.size, arrival.hold_left)
            && let Some(candidates) = arrival
                .candidates
                .iter()
                .map(|c| {
                    Some(WithInputs {
                        observation: Observation::from_inputs(c.state.clone()?, c.inputs.clone()?),
                        expected: Expected::Arrival(Box::new(ArrivalEstimate {
                            state: None,
                            inputs: None,
                            ..c.clone()
                        })),
                    })
                })
                .collect::<Option<Vec<_>>>()
        {
            return Some(Self {
                scored_at: arrival.scored_at,
                parameters,
                winner: arrival.winner.clone(),
                scoring: Scoring::Inputs(WithInputsScoring {
                    arrival: true,
                    size,
                    hold_left,
                    candidates,
                }),
            });
        }
        if let Some(scoring) = &decision.scoring
            && let Some(candidates) = decision
                .candidates
                .iter()
                .map(|c| {
                    Some(WithInputs {
                        observation: Observation::from_inputs(state_of(c), c.inputs.clone()?),
                        expected: Expected::Last {
                            estimate: Box::new(c.estimate.clone()?),
                            why: c.why.clone(),
                        },
                    })
                })
                .collect::<Option<Vec<_>>>()
        {
            return Some(Self {
                scored_at: scoring.scored_at,
                parameters,
                winner: scoring.winner.clone(),
                scoring: Scoring::Inputs(WithInputsScoring {
                    // A request that started at once was scored only on arrival.
                    arrival: decision
                        .arrival
                        .as_ref()
                        .is_some_and(|a| a.scored_at == scoring.scored_at),
                    size: scoring.size,
                    hold_left: scoring.hold_left,
                    candidates,
                }),
            });
        }
        if decision.candidates.is_empty() {
            return None;
        }
        let candidates = decision
            .candidates
            .iter()
            .map(|c| Some((c.member.clone(), c.estimate.clone()?)))
            .collect::<Option<Vec<_>>>()?;
        Some(Self {
            scored_at: decision.decided_at,
            parameters,
            winner: decision
                .candidates
                .iter()
                .find(|c| c.chosen)
                .map(|c| c.member.clone()),
            scoring: Scoring::Estimates(EstimatesScoring {
                hold_left: record.hold_limit.map_or(Duration::ZERO, |l| l.seconds),
                calling_member: record.calling_member.clone(),
                candidates,
            }),
        })
    }

    pub fn origin(&self) -> Origin {
        match &self.scoring {
            Scoring::Inputs(scoring) if scoring.arrival => Origin::Arrival,
            Scoring::Inputs(_) => Origin::LastScoring,
            Scoring::Estimates(_) => Origin::RecordedEstimates,
        }
    }

    /// Whether the record holds what `change` needs.
    pub fn replays(&self, change: &Change) -> bool {
        match self.scoring {
            Scoring::Inputs(_) => true,
            Scoring::Estimates(_) => change.margin_only(),
        }
    }

    /// Scores under the recorded parameters, which must reproduce the recorded
    /// winner and every candidate's estimate exactly; with `gates`, every
    /// member's recorded gate must also follow from its recorded load and
    /// thresholds, so that it can be derived again under changed ones.
    pub fn reproduce(&self, gates: bool) -> Result<Rescored, Unreplayable> {
        let rescored = match &self.scoring {
            Scoring::Inputs(scoring) => {
                if gates && !scoring.gates_follow() {
                    return Err(Unreplayable::GateNotReproduced);
                }
                let verdict = scoring.decide(&self.parameters, &self.parameters, None)?;
                if !scoring.reproduced_by(&verdict) {
                    return Err(Unreplayable::NotReproduced);
                }
                Rescored::of(verdict)
            }
            Scoring::Estimates(scoring) => {
                let rescored = scoring.pick(&self.parameters);
                if rescored.winner != self.winner {
                    return Err(Unreplayable::StartedElsewhereBeforeInputs);
                }
                rescored
            }
        };
        if rescored.winner == self.winner {
            Ok(rescored)
        } else {
            Err(Unreplayable::NotReproduced)
        }
    }

    /// Scores under `parameters`. With a `change` to the gate thresholds, every
    /// member's gate is derived again from its recorded load, under its own
    /// recorded thresholds with the change in place.
    pub fn score(
        &self,
        parameters: &Parameters,
        change: &Change,
    ) -> Result<Rescored, Unreplayable> {
        Ok(match &self.scoring {
            Scoring::Inputs(scoring) => Rescored::of(scoring.decide(
                &self.parameters,
                parameters,
                Some(change).filter(|c| c.changes_admission()),
            )?),
            Scoring::Estimates(scoring) => scoring.pick(parameters),
        })
    }
}

impl WithInputsScoring {
    /// Whether every member's recorded gate is the one its recorded load and
    /// thresholds give. A member that reported no thresholds is taken to use
    /// the defaults, which this checks.
    fn gates_follow(&self) -> bool {
        self.candidates.iter().all(|c| {
            let o = &c.observation;
            gate_under(o, &o.admission.clone().unwrap_or_default()) == o.state.gate
        })
    }

    /// Whether `verdict` is the recorded one, candidate by candidate.
    fn reproduced_by(&self, verdict: &placement::Verdict) -> bool {
        verdict.candidates.len() == self.candidates.len()
            && verdict
                .candidates
                .iter()
                .zip(&self.candidates)
                .all(|(scored, recorded)| recorded.expected.matches(scored))
    }

    /// Scores under `parameters`; with `gates`, after deriving every member's
    /// gate again under its own thresholds from `recorded` with the change.
    fn decide(
        &self,
        recorded: &Parameters,
        parameters: &Parameters,
        gates: Option<&Change>,
    ) -> Result<placement::Verdict, Unreplayable> {
        let observations = self
            .candidates
            .iter()
            .map(|c| {
                let mut observation = c.observation.clone();
                if let Some(change) = gates {
                    let own = Parameters {
                        admission: observation.admission.clone().unwrap_or_default(),
                        ..recorded.clone()
                    };
                    let admission = change
                        .apply(&own)
                        .map_err(Unreplayable::InvalidParameters)?
                        .admission;
                    observation.state.gate = gate_under(&observation, &admission);
                    observation.admission = Some(admission);
                }
                Ok(observation)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let wanted = Wanted {
            size: self.size,
            hold_left: self.hold_left,
            parameters: &parameters.placement,
        };
        Ok(placement::decide(observations, Vec::new(), &wanted))
    }
}

impl EstimatesScoring {
    fn pick(&self, parameters: &Parameters) -> Rescored {
        let Self {
            hold_left,
            calling_member,
            candidates,
        } = self;
        let contenders: Vec<Contender> = candidates
            .iter()
            .map(|(member, estimate)| Contender {
                member,
                caller: member == calling_member,
                eligible: estimate.eligible,
                completion: estimate.completion_seconds,
                copy: estimate.copy,
            })
            .collect();
        let (winner, reason) = placement::pick(&contenders, *hold_left, &parameters.placement);
        Rescored {
            winner,
            reason,
            caller: candidates
                .iter()
                .any(|(member, _)| member == calling_member)
                .then(|| calling_member.clone()),
            completions: candidates
                .iter()
                .map(|(member, estimate)| (member.clone(), estimate.completion_seconds))
                .collect(),
            run_levels: candidates
                .iter()
                .map(|(member, estimate)| (member.clone(), estimate.run_from.level()))
                .collect(),
        }
    }
}
