//! Offline unattended-climb readiness. Uses the rehearsal planner and production
//! scheduler search; never obtains trust, dispatches work or changes settings.
use crate::{
    failure::Result,
    tuning::{self, TuneOptions},
    workload::trace::Trace,
};
use serde_json::{Value, json};
use std::time::Instant;

pub struct Options {
    pub observed_rehearsals: Vec<Trace>,
    pub busy_hours: Option<crate::busy_hour_calibration::Target>,
    pub load_factor: crate::replay_load::LoadFactor,
    pub rehearsal_window: crate::rehearsal::window::Busiest,
    pub min_interactive: std::num::NonZeroUsize,
    pub compress: f64,
    pub limit: std::time::Duration,
    pub sample: u8,
    pub iteration_budget: u16,
    pub seed: u64,
}

fn blocker(stage: &str, reason: impl Into<String>, ids: Value) -> Value {
    json!({"stage":stage,"reason":reason.into(),"run_ids":ids})
}
fn metrics(report: &Value, candidate: &Value, envelope: &str, blockers: &mut Vec<Value>) {
    let o = &report["scheduling_objective"];
    let ids = json!({"overhead":o["overhead"]["unknown_run_ids"],"contention":o["contention_slowdown"]["unknown_run_ids"],"failures":o["failures"]["unknown_run_ids"]});
    for path in [
        "/overhead/mean",
        "/overhead/p90",
        "/reserve_violation_seconds",
        "/fleet_busy/rate",
        "/contention_slowdown/distribution/p90",
        "/failures/count",
    ] {
        if o.pointer(path).and_then(Value::as_f64).is_none() {
            blockers.push(json!({"stage":"simulation","candidate":candidate,"envelope":envelope,"reason":format!("undefined_objective_metric: {path}"),"run_ids":ids}));
        }
    }
    for key in ["reserve", "fleet_busy", "contention_slowdown", "failures"] {
        if o["hard_limits"][key].as_bool().is_none() {
            blockers.push(json!({"stage":"simulation","candidate":candidate,"envelope":envelope,"reason":format!("undefined_hard_limit: {key}"),"run_ids":ids}));
        }
    }
    for field in ["overhead", "contention_slowdown", "failures"] {
        if o[field]["unknown_run_ids"]
            .as_array()
            .is_some_and(|ids| !ids.is_empty())
        {
            blockers.push(json!({"stage":"simulation","candidate":candidate,"envelope":envelope,"reason":format!("unknown_{field}_evidence"),"run_ids":o[field]["unknown_run_ids"],"evidence":report["uncertain_constraints"]}));
        }
    }
    if report["status"] == "invalid" {
        blockers.push(json!({"stage":"simulation","candidate":candidate,"envelope":envelope,"reason":"invalid_simulation","evidence":report["external_event_invariants"]}));
    }
}

fn summary_metrics(m: &Value) -> Value {
    let o = &m["scheduling_objective"];
    json!({"execution_members":m["members"].as_object().map(|members|members.iter().map(|(name,value)|(name.clone(),value["started_interactive_roots"].clone())).collect::<serde_json::Map<_,_>>()),"status":m["status"],"score":m["score"],"overhead":{"mean":o["overhead"]["mean"],"p90":o["overhead"]["p90"],"population":o["overhead"]["population"],"unknown_run_ids":o["overhead"]["unknown_run_ids"]},"reserve_violation_seconds":o["reserve_violation_seconds"],"fleet_busy":o["fleet_busy"],"contention_slowdown":{"p90":o["contention_slowdown"]["distribution"]["p90"],"unknown_run_ids":o["contention_slowdown"]["unknown_run_ids"]},"failures":o["failures"],"hard_limits":o["hard_limits"]})
}

pub fn check(trace: Trace, args: Options) -> Result<Value> {
    if !(2..=16).contains(&args.sample)
        || !(1..=1024).contains(&args.iteration_budget)
        || !args.compress.is_finite()
        || !(1.0..=1000.0).contains(&args.compress)
        || args.limit.is_zero()
        || args.limit.as_secs_f64() > 3600.0
    {
        return Err(crate::rehearsal::invalid(
            "Preflight requires sample 2–16, budget 1–1024, compress 1–1000 and a positive limit at most 1h.",
        ));
    }
    let started = Instant::now();
    crate::simulation::validate_trace(&trace)?;
    let mut blockers = Vec::new();
    let (mut workload, selection) = match crate::rehearsal::window::select(
        &trace,
        args.rehearsal_window.with_minimum(args.min_interactive),
    ) {
        Ok((workload, selection)) => (
            Some(workload),
            serde_json::to_value(selection).expect("selection serializes"),
        ),
        Err(error) => {
            let details = error.details.map(|v| *v).unwrap_or(Value::Null);
            if let Some(candidates) = details["candidates"].as_array() {
                let reasons: std::collections::BTreeSet<_> = candidates
                    .iter()
                    .filter_map(|c| c["blockers"].as_array())
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect();
                for reason in reasons {
                    blockers.push(blocker(
                        "workload",
                        reason,
                        json!(
                            trace
                                .requests
                                .iter()
                                .filter(|r| reason.contains(&r.fact.run_id))
                                .map(|r| &r.fact.run_id)
                                .collect::<Vec<_>>()
                        ),
                    ));
                }
            }
            blockers.push(blocker("workload", error.message, json!([])));
            (None, details)
        }
    };
    let placement_split = workload.as_ref().map(crate::replay_placement::split);
    let selected_valid = workload.is_some();
    // A failed workload check must not hide independently discoverable model,
    // search, compression or timing blockers. Diagnose the densest candidate
    // as well, without treating it as a usable selection.
    if workload.is_none()
        && let Some(candidate) = selection["candidates"].as_array().and_then(|candidates| {
            candidates.iter().max_by_key(|c| {
                (
                    c["interactive_roots"].as_u64().unwrap_or(0),
                    std::cmp::Reverse(c["since"].as_str().unwrap_or("")),
                )
            })
        })
    {
        let since =
            serde_json::from_value(candidate["since"].clone()).expect("selection timestamp");
        let until =
            serde_json::from_value(candidate["until"].clone()).expect("selection timestamp");
        workload = Some(crate::rehearsal::window::bound(&trace, since, until)?);
    }
    let mut search = Value::Null;
    let mut estimate = Value::Null;
    let mut calibration = Value::Null;
    let mut load_factor = args.load_factor;
    if let Some(workload) = workload {
        if selected_valid && let Some(target) = &args.busy_hours {
            let (chosen, report) = crate::busy_hour_calibration::calibrate_preflight(
                &trace,
                &workload,
                target,
                &args.observed_rehearsals,
                args.compress,
                args.seed,
            )?;
            calibration = report;
            if let Some(chosen) = chosen {
                load_factor = chosen;
            } else {
                blockers.push(blocker(
                    "calibration",
                    "busy_hours_not_matched_by_observed_rehearsals",
                    json!([]),
                ));
            }
        }
        let inputs = crate::rehearsal::inputs::Inputs::new(&workload);
        for request in workload
            .requests
            .iter()
            .filter(|r| !r.fact.nested && r.fact.requested_at >= workload.since)
        {
            for reason in inputs.problems(request, args.compress) {
                blockers.push(blocker("planning", reason, json!([request.fact.run_id])));
            }
        }
        for (id, reason) in crate::rehearsal::plan::nested_problems(&workload) {
            blockers.push(blocker("planning", reason, json!([id])));
        }
        let plan = crate::rehearsal::plan::Plan::with_load(&workload, args.compress, load_factor);
        if let Err(error) = &plan {
            blockers.push(blocker("planning", &error.message, json!([])));
        }
        let tune_started = Instant::now();
        match tuning::tune(
            workload.clone(),
            None,
            TuneOptions {
                load_factor,
                space: None,
                budget: usize::from(args.sample),
                seed: args.seed,
                top: usize::from(args.sample),
                day: None,
                held_out_day: None,
            },
        ) {
            Err(error) => blockers.push(blocker("tune", error.message, json!([]))),
            Ok(result) => {
                if let Some(candidates) = result["candidates"].as_array() {
                    for c in candidates {
                        metrics(&c["metrics"], &c["candidate"], "point", &mut blockers);
                    }
                }
                if result["evaluated_candidates"].as_u64().unwrap_or(0) < 2 {
                    blockers.push(blocker("tune", "candidate_sample_not_ranked", json!([])));
                }
                let search_seconds = result["performance"]["search_elapsed_seconds"]
                    .as_f64()
                    .unwrap_or(0.0);
                let sampled = result["evaluated_candidates"].as_f64().unwrap_or(1.0);
                let search_estimate = search_seconds / sampled * f64::from(args.iteration_budget);
                let last = workload
                    .requests
                    .iter()
                    .filter(|r| r.fact.requested_at >= workload.since)
                    .filter_map(|r| r.terminal_at())
                    .max()
                    .unwrap_or(workload.until);
                let arm_seconds = last.since(workload.since).as_secs_f64() / args.compress;
                let planned_seconds = plan
                    .as_ref()
                    .ok()
                    .map(|p| {
                        p.arrivals
                            .iter()
                            .map(|a| (a.at_ms + a.request.duration_ms) as f64 / 1000.0)
                            .fold(0.0, f64::max)
                    })
                    .unwrap_or(arm_seconds);
                let simulated_seconds = result["baseline"]["cohort_terminal_span_seconds"]
                    .as_f64()
                    .map(|seconds| seconds / args.compress)
                    .unwrap_or(arm_seconds);
                let arm_seconds = planned_seconds.max(simulated_seconds);
                if args.limit.is_zero()
                    || args.limit.as_secs() > 3600
                    || arm_seconds > args.limit.as_secs_f64()
                {
                    blockers.push(blocker("duration", format!("rehearsal_span_exceeds_limit: estimated {arm_seconds:.3}s, limit {:.3}s", args.limit.as_secs_f64()), json!([])));
                }
                estimate = json!({"full_iteration_seconds":search_estimate + 4.0*(arm_seconds+40.0)+15.0,"search_seconds":search_estimate,"arm_seconds":arm_seconds,"arms":4,"basis":"Estimate: full-budget search plus A/A and A/B (four compressed planned/point-drain arms), 40s preparation/restoration per arm and 15s safety allowance. No live calibration, sync or cleanup timing has been measured; actual iterations may take longer.","iteration_budget":args.iteration_budget,"sample_elapsed_seconds":tune_started.elapsed().as_secs_f64()});
                search = json!({"evaluated_candidates":result["evaluated_candidates"],"best_candidate":result["best"]["candidate"],"ranked_alternatives":result["alternatives"].as_array().map(|a|a.iter().map(|c|c["candidate"].clone()).collect::<Vec<_>>()),"candidates":result["candidates"].as_array().map(|a|a.iter().map(|c|json!({"candidate":c["candidate"],"settings":c["settings"],"classification":c["classification"],"point":summary_metrics(&c["metrics"]),"lower":summary_metrics(&c["envelopes"]["lower"]),"upper":summary_metrics(&c["envelopes"]["upper"])})).collect::<Vec<_>>()),"performance":result["performance"]});
            }
        }
    }
    Ok(
        json!({"preflight_version":1,"placement_split":placement_split,"load_factor":load_factor,"calibration":calibration,"source":{"digest":crate::build_identity::Digest::of(&serde_json::to_vec(&trace).expect("trace serializes")),"since":trace.since,"until":trace.until,"requests":trace.requests.len(),"members":trace.members.iter().map(|m|json!({"member":m.member,"load_minutes":m.load.len()})).collect::<Vec<_>>()},"passed":blockers.is_empty(),"offline":true,"read_only":true,"blockers":blockers,"selection":selection,"tune":search,"duration_estimate":estimate,"elapsed_seconds":started.elapsed().as_secs_f64(),"limits":["Offline readiness only. Live calibration, fidelity certification, A/A noise, A/B acceptance, fleet readiness and cleanup still require live preflight.","Only point evidence gates readiness and candidate ranking. Lower/upper envelopes are risk information and never block; observed A/B hard limits gate acceptance.","Internal housekeeping and fidelity drift work are background load, excluded only from synthetic rehearsal workloads."]}),
    )
}
