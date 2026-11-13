//! Per-request normalized scheduling objective. No ratio of aggregate latencies.
use super::{engine::Trial, metrics::distribution};
use crate::{
    scheduling::{request::Kind, timestamp::Timestamp},
    workload::trace::{Trace, TraceOutcome},
};
use serde_json::{Value, json};

pub fn summarize(trace: &Trace, trial: &Trial, since: Timestamp, until: Timestamp) -> Value {
    let mut overheads = Vec::new();
    let mut slowdowns = Vec::new();
    let mut unknown = Vec::new();
    let mut unknown_slowdown = Vec::new();
    let mut requests = Vec::new();
    let mut failures = Vec::new();
    let mut baseline_failures = Vec::new();
    let mut missing_failures = Vec::new();
    let mut busy = 0;
    let mut verdicts = 0;
    for r in trace
        .requests
        .iter()
        .filter(|r| !r.fact.nested && r.fact.requested_at < until)
    {
        let kind = super::model::original_kind(trace, r);
        let in_window = r.fact.requested_at >= since;
        let o = trial.outcomes.iter().find(|o| o.run_id == r.fact.run_id);
        let failed_exit = matches!(r.outcome, Some(TraceOutcome::Exited { exit_code }) if exit_code != 0)
            || matches!(
                r.outcome,
                Some(TraceOutcome::Signaled { .. } | TraceOutcome::Cancelled)
            );
        if in_window && kind == Kind::Run {
            if failed_exit
                || r.fact.failure.is_some()
                || matches!(r.outcome, Some(TraceOutcome::Failed { .. }))
            {
                baseline_failures.push(&r.fact.run_id);
            }
            match o {
                Some(o) if o.terminal_at.is_some() => {
                    if o.failure.is_none() && r.outcome.is_none() {
                        missing_failures.push(&r.fact.run_id);
                    }
                    if o.failure.is_some() || (o.run_ms.is_some() && failed_exit) {
                        failures.push(&r.fact.run_id);
                    }
                }
                _ => missing_failures.push(&r.fact.run_id),
            }
        }
        let ideal = o
            .and_then(|o| o.ideal_ms)
            .filter(|ms| ms.is_finite() && *ms > 0.0);
        if kind == Kind::Run
            && o.is_some_and(|o| o.run_ms.is_some() && o.terminal_at.is_none_or(|at| at > since))
        {
            if let Some(ratio) = o
                .and_then(|o| o.run_ms.map(|ms| ms.saturating_sub(o.leftovers_wait_ms)))
                .zip(ideal)
                .map(|(ms, ideal)| ms as f64 / ideal)
            {
                slowdowns.push(ratio);
                if o.is_some_and(|o| o.uncertain) {
                    unknown_slowdown.push(&r.fact.run_id);
                }
            } else {
                unknown_slowdown.push(&r.fact.run_id);
            }
        }
        if !in_window || r.fact.deferrable {
            continue;
        }
        if let Some(o) = o {
            if o.member.is_some()
                || matches!(
                    o.failure.as_deref(),
                    Some(
                        "fleet_busy"
                            | "request_too_large"
                            | "member_unavailable"
                            | "config_sync_failed"
                    )
                )
            {
                verdicts += 1;
            }
            if o.failure.as_deref() == Some("fleet_busy") {
                busy += 1;
            }
        }
        // A refusal has no completed work/result runtime. Preserve it in the
        // population and make the objective unscorable, rather than drop it.
        let elapsed = o
            .filter(|o| o.run_ms.is_some())
            .and_then(|o| o.terminal_at)
            .map(|at| at.since(r.fact.requested_at).as_secs_f64() * 1000.0);
        let overhead = elapsed.zip(ideal).map(|(t, i)| (t - i) / i);
        if let Some(h) = overhead {
            overheads.push(h);
        } else {
            unknown.push(&r.fact.run_id);
        }
        requests.push(json!({"run_id":r.fact.run_id,"ideal_ms":ideal,"time_to_result_ms":elapsed,"overhead":overhead}));
    }
    let mut overhead = distribution(&overheads);
    let population_complete = !trace.incomplete
        && trace
            .rehearsal
            .as_ref()
            .is_none_or(|r| r.complete && r.planned == trace.requests.len());
    let scorable = !overheads.is_empty() && unknown.is_empty() && population_complete;
    if !scorable {
        overhead["mean"] = Value::Null;
        overhead["p90"] = Value::Null;
    }
    overhead["population"] = json!(requests.len());
    overhead["unknown_run_ids"] = json!(unknown);
    let slowdown = distribution(&slowdowns);
    let reserve = &trial.reserve;
    let admission_known = reserve.admission_unknown_member_seconds == 0.0;
    let reserve_pass = admission_known.then_some(reserve.admission_violating_member_seconds == 0.0);
    let busy_rate = (verdicts > 0).then(|| busy as f64 / verdicts as f64);
    let slowdown_pass = if slowdown["p90"].as_f64().is_some_and(|s| s > 1.25) {
        Some(false)
    } else if unknown_slowdown.is_empty() {
        slowdown["p90"].as_f64().map(|s| s <= 1.25)
    } else {
        None
    };
    let failure_count = missing_failures.is_empty().then_some(failures.len());
    json!({"scorable":scorable,"population_complete":population_complete,"overhead":overhead,"requests":requests,
        "reserve_violation_seconds":admission_known.then_some(reserve.admission_violating_member_seconds),
        "admission_reserve":{"violating_member_seconds":admission_known.then_some(reserve.admission_violating_member_seconds),"excess_core_seconds":admission_known.then_some(reserve.declared_excess_core_seconds),"unknown_member_seconds":reserve.admission_unknown_member_seconds},
        "attributable_reserve":{"violating_member_seconds":(reserve.unknown_member_seconds==0.0).then_some(reserve.violating_member_seconds),"excess_core_seconds":(reserve.unknown_member_seconds==0.0).then_some(reserve.excess_core_seconds),"unknown_member_seconds":reserve.unknown_member_seconds,"basis":"modelled effective demand plus inferred outside CPU; observed host attribution remains unknown"},
        "fleet_busy":{"numerator":busy,"denominator":verdicts,"rate":busy_rate,"limit":0.01},
        "contention_slowdown":{"distribution":slowdown,"p90_excess":slowdown["p90"].as_f64().map(|s|s-1.0),"unknown_run_ids":unknown_slowdown,"limit":1.25},
        "failures":{"count":failure_count,"run_ids":failures,"unknown_run_ids":missing_failures,"baseline_count":baseline_failures.len(),"recorded_baseline_run_ids":baseline_failures},
        "hard_limits":{"reserve":reserve_pass,"fleet_busy":busy_rate.map(|r|r<=0.01),"contention_slowdown":slowdown_pass,"failures":failure_count.map(|n|n<=baseline_failures.len())},
        "stopping_target_met":overhead["mean"].as_f64().zip(overhead["p90"].as_f64()).map(|(m,p)|m<=0.15 && p<=0.50)})
}

/// Baseline means the unchanged-settings trial, not a historical refusal count.
pub fn constrain_failures(metrics: &mut Value, baseline: Option<u64>, basis: &str) {
    let o = &mut metrics["scheduling_objective"];
    let count = o["failures"]["count"].as_u64();
    o["failures"]["baseline_count"] = json!(baseline);
    o["failures"]["baseline_basis"] = json!(basis);
    o["hard_limits"]["failures"] = json!(count.zip(baseline).map(|(n, b)| n <= b));
    let violated = o["hard_limits"]
        .as_object()
        .expect("limits")
        .values()
        .any(|v| v == false);
    let scorable = o["scorable"] == true;
    let mean = o["overhead"]["mean"].clone();
    if metrics["status"] != "invalid" {
        metrics["status"] = json!(if violated {
            "infeasible"
        } else if !scorable {
            "unscorable"
        } else {
            "validation_needed"
        });
        metrics["score"] = if violated { json!("infinity") } else { mean };
    }
    metrics["violation_vector"]["failed_run_excess"] =
        json!(count.zip(baseline).map(|(n, b)| n.saturating_sub(b)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rehearsal::recording::Recording;
    use std::collections::BTreeMap;
    fn fixture() -> Trace {
        let mut trace = crate::simulation::engine::tests::trace();
        trace.requests.truncate(2);
        let start = trace.since;
        for (i, r) in trace.requests.iter_mut().enumerate() {
            r.fact.size = crate::scheduling::capacity::Cores::from_millis(250);
            r.fact.requested_at = start;
            r.fact.member = Some("a".into());
            r.fact.deferrable = false;
            r.fact.waited_ms = Some(0);
            r.fact.run_ms = Some(if i == 0 { 1100 } else { 2400 });
            r.fact.resources.as_mut().unwrap().started_at = Some(start);
            r.fact.resources.as_mut().unwrap().ended_at =
                Some(start.after(std::time::Duration::from_millis(r.fact.run_ms.unwrap())));
        }
        trace.rehearsal = Some(Recording {
            session: "a".repeat(24),
            source_digest: crate::build_identity::Digest::of(b"fixture"),
            build: crate::build_identity::this_build().unwrap(),
            load_factor: Default::default(),
            compress: 1.0,
            seed: 0,
            complete: true,
            planned: 2,
            requests: Value::Null,
            aggregated_nested: vec![],
            load_complete: BTreeMap::new(),
            charge_floor_ms: BTreeMap::new(),
            duration_ms: BTreeMap::new(),
            demand: BTreeMap::new(),
            calibration: BTreeMap::new(),
            work_iterations: BTreeMap::new(),
            ideal_ms: trace
                .requests
                .iter()
                .enumerate()
                .map(|(i, r)| (r.fact.run_id.clone(), Some(1000.0 * (i + 1) as f64)))
                .collect(),
            uncontended_ms: trace
                .requests
                .iter()
                .map(|r| {
                    (
                        r.fact.run_id.clone(),
                        BTreeMap::from([("a".into(), 1000.0)]),
                    )
                })
                .collect(),
        });
        trace
    }
    #[test]
    fn overhead_is_a_mean_of_request_ratios_and_nearest_rank_p90() {
        let trace = fixture();
        let report = super::super::observed::summarize(&trace);
        let m = &report["scheduling_objective"];
        assert!((m["overhead"]["mean"].as_f64().unwrap() - 0.15).abs() < 1e-12);
        assert_eq!(m["overhead"]["p90"], 0.2);
        assert_eq!(m["contention_slowdown"]["distribution"]["p90"], 1.2);
        assert_eq!(m["hard_limits"]["contention_slowdown"], true);
        assert_eq!(m["failures"]["count"], 0);
        assert_eq!(m["reserve_violation_seconds"], 0.0);
        assert!(m["attributable_reserve"]["violating_member_seconds"].is_null());
    }
    #[test]
    fn unknown_outcomes_partial_populations_and_legacy_workers_cannot_claim_a_score() {
        let mut trace = fixture();
        trace.requests[0].outcome = None;
        assert!(
            super::super::observed::summarize(&trace)["scheduling_objective"]["failures"]["count"]
                .is_null()
        );
        trace = fixture();
        trace.rehearsal.as_mut().unwrap().complete = false;
        let m = super::super::observed::summarize(&trace);
        assert_eq!(m["scheduling_objective"]["scorable"], false);
        assert!(m["scheduling_objective"]["overhead"]["mean"].is_null());
        trace = fixture();
        trace.rehearsal.as_mut().unwrap().ideal_ms.clear();
        let m = super::super::observed::summarize(&trace);
        assert!(m["scheduling_objective"]["overhead"]["mean"].is_null());
        assert!(!trace.rehearsal.as_ref().unwrap().complete_for(&trace));
    }
    #[test]
    fn synthesized_leases_keep_their_own_population_and_do_not_consume_run_failure_budget() {
        let mut trace = fixture();
        let id = trace.requests[0].fact.run_id.clone();
        trace.requests[0].outcome = Some(TraceOutcome::Exited { exit_code: 7 });
        trace.rehearsal.as_mut().unwrap().requests =
            json!([{"job":{"run_id":id},"original_kind":"lease"}]);
        let m = super::super::observed::summarize(&trace);
        assert_eq!(m["objective"]["leases"], 1);
        assert_eq!(m["objective"]["runs"], 1);
        assert_eq!(m["scheduling_objective"]["failures"]["count"], 0);
        assert_eq!(
            m["scheduling_objective"]["contention_slowdown"]["distribution"]["count"],
            1
        );
    }
    #[test]
    fn source_refusal_does_not_become_a_command_failure_after_successful_admission() {
        let mut trace = fixture();
        trace.requests[0].outcome = Some(TraceOutcome::Failed {
            code: "fleet_busy".into(),
        });
        // Observed failure field is deliberately empty to model successful
        // candidate execution, rather than the source scheduling refusal.
        let m = super::super::observed::summarize(&trace);
        assert_eq!(m["scheduling_objective"]["failures"]["count"], 0);
        let mut candidate = m.clone();
        candidate["scheduling_objective"]["failures"]["count"] = json!(1);
        constrain_failures(&mut candidate, Some(0), "unchanged-settings simulation");
        assert_eq!(
            candidate["scheduling_objective"]["hard_limits"]["failures"],
            false
        );
    }
    #[test]
    fn missing_results_and_failed_commands_are_never_dropped() {
        let mut trace = fixture();
        trace.requests[0].outcome = Some(TraceOutcome::Exited { exit_code: 7 });
        let m = super::super::observed::summarize(&trace);
        assert_eq!(m["scheduling_objective"]["failures"]["count"], 1);
        assert_eq!(m["scheduling_objective"]["overhead"]["population"], 2);
        trace.requests[1].fact.run_ms = None;
        trace.requests[1].fact.member = None;
        trace.requests[1].fact.failure = Some("fleet_busy".into());
        let m = super::super::observed::summarize(&trace);
        let m = &m["scheduling_objective"];
        assert!(m["overhead"]["mean"].is_null());
        assert_eq!(m["overhead"]["population"], 2);
        assert_eq!(m["fleet_busy"]["rate"], 0.5);
        assert_eq!(m["hard_limits"]["fleet_busy"], false);
        assert_eq!(m["failures"]["count"], 2);
    }
}
