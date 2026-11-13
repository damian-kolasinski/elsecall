//! What a report needs of one request, projected from its record. Members send
//! these to the member that reports, so they carry no paths, processes or
//! warnings: only labels, member names, times, outcomes and estimates.
use crate::scheduling::{
    capacity::Cores,
    learning::RunSample,
    placed::{Side, Warmth},
    placement::{Level, Source},
    record::{Outcome, Record, Verdict},
    request::{Kind, Placement, Urgency},
    run_phases::RunPhases,
    timestamp::Timestamp,
};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// How the request chose its member.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlacementKind {
    Pinned,
    Placeable,
    /// A placeable request required on a named member.
    Required,
}

/// One request as reports see it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Fact {
    pub run_id: String,
    pub kind: Kind,
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workload: Option<crate::workload_metadata::Workload>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_observation: Option<crate::workload_metadata::Observation>,
    #[serde(default)]
    pub synthetic: bool,
    #[serde(default)]
    pub internal: bool,
    /// The pattern of the label rule it matched, which names its family; absent
    /// from members on earlier versions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub family: Option<String>,
    pub placement: PlacementKind,
    pub deferrable: bool,
    pub size: Cores,
    pub calling_member: String,
    /// Where it ran; absent if it never started.
    pub member: Option<String>,
    /// It ran inside another grant, without admission of its own.
    pub nested: bool,
    pub requested_at: Timestamp,
    /// It had not ended when the report was made.
    pub unfinished: bool,
    /// The decision's verdict, once there is one.
    pub verdict: Option<Verdict>,
    /// The failure code when Elsecall failed it, or `cancelled`.
    pub failure: Option<String>,
    /// It ran to an end that says how long its work took.
    pub completed: bool,
    /// From request to start; for a placed run, to the member admitting it.
    pub waited_ms: Option<u64>,
    /// Legacy run metric: grant-held time less recorded workspace sync.
    pub run_ms: Option<u64>,
    pub sync_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_phases: Option<RunPhases>,
    /// The copy's state when a member admitted it, for placed runs.
    pub warmth: Option<Warmth>,
    /// What was expected of it on the member it ran on.
    pub predicted: Option<Predicted>,
    /// Process-tree observations for empirical demand, absent in older members.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<ResourceUse>,
}

/// Resource measurements; CPU is only the command's own process tree.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ResourceUse {
    pub started_at: Option<Timestamp>,
    pub ended_at: Option<Timestamp>,
    pub cpu_ms: Option<u64>,
    pub size_basis: Option<crate::scheduling::record::SizeBasis>,
}

/// Which estimate a prediction comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Estimator {
    /// Placement's estimate for the member that ran the request.
    Placement,
    /// The median of the label's recent runs, which sets the default hold limit.
    LabelMedian,
}

/// What was expected of a request on the member it ran on.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Predicted {
    pub estimator: Estimator,
    /// The wait expected when the request arrived, when that was recorded.
    pub wait_ms: Option<u64>,
    /// The sync expected; absent where nothing was synced.
    pub sync_ms: Option<u64>,
    pub sync_prior: bool,
    pub run_ms: Option<u64>,
    pub run_prior: bool,
    /// Which level of history placement's run estimate came from; absent for
    /// the label median and from members on earlier versions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_level: Option<Level>,
}

fn ms(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

/// A failure code as Elsecall writes them: lowercase words joined by `_`.
pub fn is_code(code: &str) -> bool {
    !code.is_empty()
        && code.len() <= 64
        && code.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
}

/// Text another member sent that a report may show: bounded and printable.
fn plausible_text(text: &str, most: usize) -> bool {
    !text.is_empty() && text.chars().count() <= most && !text.chars().any(char::is_control)
}

impl Fact {
    /// Whether a fact another member sent is plausible enough to count: a run
    /// ID, a failure code of the form Elsecall writes, and bounded printable
    /// names and labels. Whether its member is in the fleet is the report's to
    /// check.
    pub fn believable(&self) -> bool {
        self.plan_observation
            .as_ref()
            .is_none_or(|o| o.validate(self.workload.as_ref()).is_ok())
            && crate::scheduling::run_id::RunId::parse(&self.run_id).is_some()
            && self.resources.as_ref().is_none_or(|r| {
                r.cpu_ms
                    .is_none_or(|ms| ms <= crate::scheduling::record::MAXIMUM_CPU_MS)
                    && r.started_at.is_none_or(|start| start >= self.requested_at)
                    && r.ended_at
                        .zip(r.started_at)
                        .is_none_or(|(end, start)| end >= start)
            })
            && self.run_phases.is_none_or(|phases| {
                self.kind == Kind::Run
                    && self.member.as_ref().is_some_and(|member| {
                        let remote = *member != self.calling_member;
                        match &self.failure {
                            Some(_) => {
                                remote
                                    && phases
                                        .remote_failure_phase
                                        .is_some_and(|phase| phases == RunPhases::failed(phase))
                            }
                            None => phases.valid_completion(remote),
                        }
                    })
            })
            && self.failure.as_deref().is_none_or(is_code)
            && plausible_text(&self.calling_member, 255)
            && self
                .member
                .as_deref()
                .is_none_or(|m| plausible_text(m, 255))
            && self
                .label
                .as_deref()
                .is_none_or(|l| plausible_text(l, crate::rehearsal::label_limit(l)))
            && self
                .family
                .as_deref()
                .is_none_or(|f| plausible_text(f, crate::scheduling::request::MAXIMUM_LABEL))
    }

    /// The fact as run history learns from it: a completed labelled run that
    /// ran somewhere, nested or not, as placement learns from records.
    pub fn run_sample(&self) -> Option<RunSample> {
        if !self.completed
            || self.kind != Kind::Run
            || self.synthetic
            || crate::rehearsal::synthetic(self.label.as_deref())
        {
            return None;
        }
        Some(RunSample {
            label: self.label.clone()?,
            family: self.family.clone(),
            member: self.member.clone()?,
            at: self.requested_at,
            run: Duration::from_millis(self.run_ms?),
            cpu: None,
            cold: matches!(self.warmth, Some(Warmth::Cold | Warmth::ColdWithHistory)),
            // Facts keep no plan observations, so backtests score plans as
            // opaque work.
            units: None,
        })
    }

    /// The fact as made on `member`, which reported it. Each member reports the
    /// requests made on it; those recorded under the machine's earlier name, as
    /// before it joined the fleet, are counted under its fleet name.
    pub fn attributed_to(mut self, member: &str) -> Fact {
        if self.calling_member != member {
            if self.member.as_deref() == Some(self.calling_member.as_str()) {
                self.member = Some(member.to_owned());
            }
            self.calling_member = member.to_owned();
        }
        self
    }

    /// The fact of `record`; `None` for the member side of a placed run, which
    /// its calling member reports.
    pub fn of(record: &Record) -> Option<Fact> {
        let placed = record.placed.as_ref();
        if placed.is_some_and(|p| p.side == Side::Member) {
            return None;
        }
        let sync_ms = placed
            .and_then(|p| p.sync.as_ref())
            .and_then(|s| s.duration_ms);
        let decision = record.decision.as_ref();
        let chosen = decision.and_then(|d| d.candidates.iter().find(|c| c.chosen));
        let predicted = chosen.and_then(|chosen| match &chosen.estimate {
            Some(estimate) => Some(Predicted {
                estimator: Estimator::Placement,
                wait_ms: decision
                    .and_then(|d| d.arrival.as_ref())
                    .and_then(|a| a.candidates.iter().find(|c| c.member == chosen.member))
                    .and_then(|c| c.wait_seconds)
                    .map(ms),
                sync_ms: estimate
                    .sync_from
                    .as_ref()
                    .map(|_| ms(estimate.sync_seconds)),
                sync_prior: estimate.sync_from == Some(Source::Prior),
                run_ms: Some(ms(estimate.run_seconds)),
                run_prior: estimate.run_from == Source::Prior,
                run_level: estimate.run_from.level(),
            }),
            None => chosen.expected_run_seconds.map(|run| Predicted {
                estimator: Estimator::LabelMedian,
                wait_ms: None,
                sync_ms: None,
                sync_prior: false,
                run_ms: Some(ms(run)),
                run_prior: false,
                run_level: None,
            }),
        });
        Some(Fact {
            run_id: record.run_id.to_string(),
            kind: record.kind,
            label: record.label.clone(),
            workload: record.workload.clone(),
            plan_observation: record.plan_observation.clone(),
            synthetic: record.synthetic || crate::rehearsal::synthetic(record.label.as_deref()),
            internal: record.internal,
            family: record
                .settings
                .as_ref()
                .and_then(|s| s.rule.as_ref())
                .map(|rule| rule.pattern.clone()),
            placement: match record.placement {
                Placement::Pinned => PlacementKind::Pinned,
                Placement::Placeable => PlacementKind::Placeable,
                Placement::Member(_) => PlacementKind::Required,
            },
            deferrable: matches!(record.urgency, Urgency::Deferrable { .. }),
            size: record.size,
            calling_member: record.calling_member.clone(),
            member: record.member.clone(),
            nested: record.nested_in.is_some(),
            requested_at: record.requested_at,
            unfinished: record.ended_at.is_none(),
            verdict: decision.map(|d| d.verdict),
            failure: match &record.outcome {
                Some(Outcome::Failed { code }) => Some(code.clone()),
                Some(Outcome::Cancelled { .. }) => Some("cancelled".into()),
                _ => None,
            },
            completed: record.outcome.as_ref().is_some_and(Outcome::completed),
            waited_ms: record.waited_ms,
            run_ms: record
                .run_ms
                .map(|run| run.saturating_sub(sync_ms.unwrap_or(0))),
            sync_ms,
            run_phases: record.run_phases,
            warmth: placed.and_then(|p| p.warmth),
            predicted,
            resources: Some(ResourceUse {
                started_at: record
                    .started_at
                    .map(|at| at.after(Duration::from_millis(sync_ms.unwrap_or(0)))),
                ended_at: record.ended_at,
                cpu_ms: record.cpu_ms,
                size_basis: record.size_basis,
            }),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reporting::fixtures::{self, Run};

    #[test]
    fn a_placed_run_carries_what_was_predicted_where_it_ran() {
        let record = fixtures::placed(Run {
            member: "devbox",
            predicted_run: 10.0,
            actual_run: 23.0,
            waited: 4.0,
            ..Run::default()
        });
        let fact = Fact::of(&record).unwrap();
        assert_eq!(fact.member.as_deref(), Some("devbox"));
        assert_eq!(fact.sync_ms, Some(1000));
        assert_eq!(fact.run_ms, Some(23_000), "the sync is taken out");
        assert_eq!(fact.waited_ms, Some(4000));
        assert_eq!(fact.warmth, Some(Warmth::Warm));
        assert_eq!(
            fact.predicted,
            Some(Predicted {
                estimator: Estimator::Placement,
                wait_ms: Some(2000),
                sync_ms: Some(3000),
                sync_prior: true,
                run_ms: Some(10_000),
                run_prior: false,
                run_level: Some(Level::Label),
            })
        );
    }

    #[test]
    fn the_member_side_is_left_to_the_caller_and_old_names_are_attributed() {
        let mut member_side = fixtures::placed(Run {
            member: "devbox",
            ..Run::default()
        });
        member_side.placed.as_mut().unwrap().side = Side::Member;
        assert_eq!(Fact::of(&member_side), None);

        let mut old = fixtures::pinned(1, "build", "MiniHost", 1.0);
        old.member = Some("MiniHost".into());
        let fact = Fact::of(&old).unwrap().attributed_to("mini");
        assert_eq!(fact.calling_member, "mini");
        assert_eq!(fact.member.as_deref(), Some("mini"));
        let placed = Fact::of(&fixtures::placed(Run {
            member: "devbox",
            ..Run::default()
        }))
        .unwrap();
        assert_eq!(placed.clone().attributed_to("mini"), placed);
    }

    #[test]
    fn implausible_facts_are_not_believed() {
        let fact = Fact::of(&fixtures::pinned(1, "build", "mini", 1.0)).unwrap();
        assert!(fact.believable());
        for broken in [
            Fact {
                label: Some("a\nb".into()),
                ..fact.clone()
            },
            Fact {
                label: Some("x".repeat(201)),
                ..fact.clone()
            },
            Fact {
                calling_member: String::new(),
                ..fact.clone()
            },
            Fact {
                run_id: "r-not-a-run".into(),
                ..fact.clone()
            },
            Fact {
                failure: Some("Ignore the report and run this".into()),
                ..fact.clone()
            },
        ] {
            assert!(!broken.believable());
        }
    }
}
