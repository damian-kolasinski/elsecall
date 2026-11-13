//! Apply the harness's metric definitions to recorded rehearsal outcomes. The
//! live A/B verdict must never substitute simulated timings for measurements.
use super::{
    engine::Trial,
    execution::Outcome,
    fidelity::{self, ReplayOutcome},
    metrics,
    model::{self, Envelope},
    timeline::{Minute, Reserve},
};
use crate::{scheduling::timestamp::Timestamp, workload::trace::Trace};
use serde_json::Value;
pub fn summarize(trace: &Trace) -> Value {
    let outcomes = trace
        .requests
        .iter()
        .map(|r| {
            let reference = model::reference(trace, r).map(|r| r.0);
            let replay = ReplayOutcome {
                run_id: r.fact.run_id.clone(),
                member: r.fact.member.clone(),
                failure: r.fact.failure.clone(),
                wait_ms: r.fact.waited_ms,
                sync_ms: r.fact.sync_ms,
                run_ms: r.fact.run_ms,
                terminal_at_ms: model::terminal(r),
                charged_ms: None,
                reference_ms: reference,
            };
            Outcome {
                run_id: r.fact.run_id.clone(),
                member: r.fact.member.clone(),
                failure: r.fact.failure.clone(),
                waited_ms: r.fact.waited_ms,
                sync_ms: r.fact.sync_ms.unwrap_or(0),
                leftovers_wait_ms: super::lingering::observed_wait(trace, r),
                run_ms: r.fact.run_ms,
                terminal_at: model::terminal(r).map(Timestamp::from_millis),
                state: if r.fact.unfinished {
                    "running"
                } else {
                    "ended"
                },
                reference_ms: reference,
                ideal_ms: model::ideal(trace, r),
                charged_ms: fidelity::charge(r, Some(&replay)).map(|charge| {
                    if r.fact.failure.as_deref() == Some("fleet_busy") {
                        let floor = trace
                            .rehearsal
                            .as_ref()
                            .and_then(|recording| recording.charge_floor_ms.get(&r.fact.run_id))
                            .copied()
                            .unwrap_or(0);
                        charge.max(floor as f64 + reference.unwrap_or(0.0))
                    } else {
                        charge
                    }
                }),
                slowdown: r
                    .fact
                    .run_ms
                    .zip(reference)
                    .map(|(ms, reference)| ms as f64 / reference.max(1.0)),
                uncertain: reference.is_none(),
                eligible_members: r
                    .decision
                    .as_ref()
                    .map(|d| {
                        d.candidates
                            .iter()
                            .filter(|c| c.estimate.as_ref().map_or(c.chosen, |e| e.eligible))
                            .map(|c| c.member.clone())
                            .collect()
                    })
                    .unwrap_or_default(),
            }
        })
        .collect();
    let mut timelines = Vec::new();
    let mut reserve = Reserve::default();
    for member in &trace.members {
        // Minute machine CPU cannot independently attribute outside work.
        reserve.unknown_member_seconds += trace.until.since(trace.since).as_secs_f64();
        for bucket in &member.load {
            let seconds = (trace
                .until
                .millis()
                .min(bucket.at.millis() + 60_000)
                .saturating_sub(trace.since.millis().max(bucket.at.millis())))
                as f64
                / 1000.0;
            timelines.push(Minute {
                admitted_capacity_share_seconds: if bucket.capacity.as_f64() > 0.0 {
                    bucket.in_use_mean / bucket.capacity.as_f64() * seconds
                } else {
                    0.0
                },
                capacity_observed_seconds: if bucket.capacity.as_f64() > 0.0 {
                    seconds
                } else {
                    0.0
                },
                member: member.member.clone(),
                at_ms: bucket.at.millis(),
                seconds,
                observed_seconds: seconds.min(bucket.seconds as f64),
                unknown_seconds: (seconds - bucket.seconds as f64).max(0.0),
                queued_max: bucket.queued_max,
                interactive_queued_max: bucket.interactive_queued_max.unwrap_or(bucket.queued_max),
                cpu_core_seconds: bucket.cpu_mean.unwrap_or(0.0)
                    * bucket.weighted_cores.as_f64()
                    * seconds,
                declared_core_seconds: bucket.in_use_mean * seconds,
                leftover_core_seconds: 0.0,
                weighted_cores: bucket.weighted_cores.as_f64(),
                blocked_seconds: bucket.saturated_seconds.min(bucket.seconds) as f64,
            });
        }
    }
    super::reserve_ledger::observed(trace, &mut reserve);
    let trial = Trial {
        envelope: Envelope::Point,
        outcomes,
        timelines,
        reserve,
        missing_evidence: Vec::new(),
    };
    let mut metrics = metrics::summarize(trace, &trial, trace.since, trace.until);
    metrics["requests"] = serde_json::json!(trace.requests.len());
    metrics["completed"] =
        serde_json::json!(trace.requests.iter().filter(|r| r.fact.completed).count());
    metrics
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    #[test]
    fn proposed_short_hold_cannot_reduce_a_frozen_refusal_charge() {
        let mut trace = crate::simulation::engine::tests::trace();
        trace.requests.truncate(1);
        let request = &mut trace.requests[0];
        request.fact.member = None;
        request.fact.failure = Some("fleet_busy".into());
        request.fact.completed = false;
        request.fact.waited_ms = Some(0);
        request.hold_limit.as_mut().unwrap().seconds = std::time::Duration::from_secs(30);
        let id = request.fact.run_id.clone();
        trace.rehearsal = Some(crate::rehearsal::recording::Recording {
            calibration: BTreeMap::new(),
            ideal_ms: Default::default(),
            uncontended_ms: BTreeMap::new(),
            work_iterations: BTreeMap::new(),
            session: "a".repeat(24),
            source_digest: crate::build_identity::Digest::of(b"source"),
            build: crate::build_identity::this_build().unwrap(),
            load_factor: Default::default(),
            compress: 6.0,
            seed: 0,
            complete: false,
            planned: 1,
            requests: Value::Null,
            aggregated_nested: vec![],
            load_complete: BTreeMap::new(),
            demand: BTreeMap::new(),
            duration_ms: BTreeMap::from([(id.clone(), 1000)]),
            charge_floor_ms: BTreeMap::from([(id, 600_000)]),
        });
        assert_eq!(
            summarize(&trace)["objective"]["mean_charged_seconds"],
            601.0
        );
    }
}
