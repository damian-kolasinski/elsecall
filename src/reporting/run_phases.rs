//! Distributions of observed run boundaries. Legacy grant durations cannot
//! stand in for phases that were never measured.
use crate::{
    reporting::{
        facts::Fact,
        statistics::{Distribution, seconds},
    },
    scheduling::{request::Kind, run_phases::RemoteFailurePhase},
};
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Debug, Serialize)]
pub struct PhaseMetrics {
    pub local: PhaseTimes,
    pub remote: PhaseTimes,
    pub remote_failures: BTreeMap<RemoteFailurePhase, usize>,
}

#[derive(Debug, Serialize)]
pub struct PhaseTimes {
    pub admission_wait: Distribution,
    pub workspace_sync: Distribution,
    pub execution: Distribution,
    pub result_return: Distribution,
    pub request_to_result: Distribution,
}

impl PhaseTimes {
    fn of(facts: &[&Fact]) -> Self {
        let distribution = |duration: fn(&Fact) -> Option<u64>| {
            Distribution::of(
                facts
                    .iter()
                    .filter_map(|f| duration(f).map(seconds))
                    .collect(),
            )
        };
        Self {
            admission_wait: distribution(|f| f.waited_ms),
            workspace_sync: distribution(|f| f.sync_ms),
            execution: distribution(|f| f.run_phases?.execution_ms),
            result_return: distribution(|f| f.run_phases?.result_return_ms),
            request_to_result: distribution(|f| f.run_phases?.request_to_result_ms),
        }
    }
}

pub fn metrics(facts: &[Fact], member: Option<&str>) -> PhaseMetrics {
    let mut local = Vec::new();
    let mut remote = Vec::new();
    let mut remote_failures = BTreeMap::new();
    for fact in facts {
        if fact.kind != Kind::Run || fact.nested {
            continue;
        }
        let Some(ran_on) = fact
            .member
            .as_deref()
            .filter(|ran_on| member.is_none_or(|m| m == *ran_on))
        else {
            continue;
        };
        if ran_on == fact.calling_member {
            local.push(fact);
        } else {
            remote.push(fact);
            if let Some(phase) = fact.run_phases.and_then(|p| p.remote_failure_phase) {
                *remote_failures.entry(phase).or_default() += 1;
            }
        }
    }
    PhaseMetrics {
        local: PhaseTimes::of(&local),
        remote: PhaseTimes::of(&remote),
        remote_failures,
    }
}
