//! Offline readiness over the production planner, simulator and tuner.
use elsecall::{
    scheduling::timestamp::Timestamp,
    tuning_preflight::{self, Options},
    workload::trace::Trace,
};
use serde_json::{Value, json};
use std::{num::NonZeroUsize, time::Duration};
fn fixture() -> Trace {
    let start = Timestamp::parse_rfc3339("2026-11-03T00:00:00Z").unwrap();
    let mut trace:Trace=serde_json::from_value(json!({"trace_version":1,"since":start,"until":start.after(Duration::from_secs(60)),"incomplete":false,"master":"mini","members":[],"requests":[],"leftovers":[],"demand":[],"contention":[],"limits":[]})).unwrap();
    trace.members=serde_json::from_value(json!([{"member":"mini","errors":[],"scheduler_settings":[{"at":start,"capacity":{"weighted_cores":3,"reserved":1,"capacity":2},"parameters":{}}],"load":[{"at":start,"seconds":60,"cpu_mean":0,"cpu_max":0,"outside_mean":0,"weighted_cores":3,"capacity":2,"in_use_mean":0,"in_use_max":0,"queued_max":0,"interactive_queued_max":0,"saturated_seconds":0,"saturated_busy_seconds":0,"memory_pressure_seconds":0,"worst_memory_pressure":"normal","worst_thermal_state":"nominal","speed_min":1,"speed_max":1}]}])).unwrap();
    trace.demand=serde_json::from_value(json!([{"member":"mini","family":"work","runs":4,"fitted_runs":4,"declared_cores":2,"process_tree_cores":0.5,"effective_cores":0.5,"interval_cores":[0.5,0.5],"fitted_minutes":10,"idle_minutes":3,"outside_baseline_cores":0,"residual_rmse_cores":0,"status":"identified"}])).unwrap();
    trace.contention=serde_json::from_value(json!([{"member":"mini","family":"work","matched_runs":6,"unmatched_runs":0,"status":"observational_curve","points":[{"other_declared_cores":[0,0.5],"runs":3,"duration_ratio_median":1,"duration_ratio_range":[1,1]},{"other_declared_cores":[0.5,4],"runs":3,"duration_ratio_median":1,"duration_ratio_range":[1,1]}]}])).unwrap();
    for n in 0..4 {
        trace.requests.push(serde_json::from_value(json!({"run_id":format!("r-{:012x}",n+1),"kind":"run","label":"work","family":"work","placement":"pinned","deferrable":false,"size":2,"calling_member":"mini","member":"mini","nested":false,"requested_at":start,"unfinished":false,"verdict":"started","failure":null,"completed":true,"waited_ms":n*1000,"run_ms":1000,"sync_ms":0,"warmth":null,"predicted":null,"resources":{"started_at":start.after(Duration::from_millis(n*1000)),"ended_at":start.after(Duration::from_millis((n+1)*1000)),"cpu_ms":500,"size_basis":{"from":"declared"}},"placement_request":"pinned","urgency":{"interactive":{"hold_limit_ms":10000}},"hold_limit":{"seconds":10,"basis":"requested","expected_run_seconds":null},"nested_in":null,"shares_capacity_of":null,"workspace_recorded":true,"outcome":{"kind":"exited","exit_code":0},"decision":null})).unwrap());
    }
    trace
}

fn options() -> Options {
    Options {
        rehearsal_window: "explicit".parse().unwrap(),
        min_interactive: NonZeroUsize::new(4).unwrap(),
        compress: 6.0,
        limit: Duration::from_secs(900),
        sample: 3,
        iteration_budget: 64,
        busy_hours: None,
        observed_rehearsals: vec![],
        load_factor: Default::default(),
        seed: 42,
    }
}
#[test]
fn dry_run_ranks_candidates_and_checks_every_envelope_without_changing_input() {
    let source = fixture();
    let before = serde_json::to_vec(&source).unwrap();
    let result = tuning_preflight::check(source.clone(), options()).unwrap();
    assert_eq!(result["passed"], true, "{result}");
    assert_eq!(result["tune"]["evaluated_candidates"], 3);
    assert!(
        result["duration_estimate"]["full_iteration_seconds"]
            .as_f64()
            .unwrap()
            > 0.0
    );
    assert_eq!(before, serde_json::to_vec(&source).unwrap());
}
#[test]
fn collects_population_carry_in_load_and_each_demand_blocker_together() {
    let mut source = fixture();
    source.since = source.since.after(Duration::from_millis(1));
    source.requests[1].fact.requested_at = source.since;
    source.requests[2].fact.requested_at = source.since;
    source.demand[0].effective_cores = None;
    source.members[0].load.clear();
    let result = tuning_preflight::check(source, options()).unwrap();
    assert_eq!(result["passed"], false);
    let blockers = result["blockers"].to_string();
    for reason in [
        "insufficient_interactive_population",
        "active_carry_in",
        "missing_drain_load",
        "r-000000000002",
        "r-000000000003",
        "lacks identified H1 effective demand",
    ] {
        assert!(blockers.contains(reason), "{reason}: {result}");
    }
}
#[test]
fn run3_arrivals_keep_post_window_drain_load_and_reject_genuine_holes() {
    let mut source = fixture();
    let start = source.since;
    source.until = start.after(Duration::from_secs(180));
    let mut tail = source.members[0].load[0].clone();
    tail.at = start.after(Duration::from_secs(60));
    source.members[0].load.push(tail.clone());
    tail.at = start.after(Duration::from_secs(120));
    source.members[0].load.push(tail);
    let request = &mut source.requests[3];
    request.fact.run_id = "r-b85f54d674be".into();
    request.fact.requested_at = start.after(Duration::from_secs(50));
    request.fact.run_ms = Some(70_000);
    request.fact.waited_ms = Some(0);
    request.fact.resources.as_mut().unwrap().started_at = Some(request.fact.requested_at);
    request.fact.resources.as_mut().unwrap().ended_at = Some(start.after(Duration::from_secs(120)));
    let (selected, _) = elsecall::rehearsal::window::explicit(
        &source,
        start,
        start.after(Duration::from_secs(60)),
        NonZeroUsize::new(4).unwrap(),
    )
    .unwrap();
    assert_eq!(selected.members[0].load.len(), 3);
    assert_eq!(selected.until, start.after(Duration::from_secs(60)));
    elsecall::simulation::validate_trace(&selected).unwrap();
    source.members[0].load.remove(1);
    let error = elsecall::rehearsal::window::explicit(
        &source,
        start,
        start.after(Duration::from_secs(60)),
        NonZeroUsize::new(4).unwrap(),
    )
    .unwrap_err();
    let report = error.details.unwrap().to_string();
    assert!(report.contains("missing_drain_load"), "{report}");
    assert!(report.contains("r-b85f54d674be"), "{report}");
}
#[test]
fn run4_housekeeping_is_background_without_demand_but_history_is_unchanged() {
    let mut source = fixture();
    let mut internal = source.requests[0].clone();
    internal.fact.run_id = "r-9d040f76a3e8".into();
    internal.fact.label = Some("elsecall housekeeping".into());
    internal.fact.family = internal.fact.label.clone();
    source.requests.push(internal);
    source.members[0].load[0].cpu_mean = Some(0.5);
    source.members[0].load[0].cpu_max = Some(0.5);
    let (selected, _) = elsecall::rehearsal::window::explicit(
        &source,
        source.since,
        source.until,
        NonZeroUsize::new(4).unwrap(),
    )
    .unwrap();
    assert_eq!(selected.requests.len(), 4);
    assert_eq!(source.requests.len(), 5);
    let modeled = elsecall::simulation::simulate(selected.clone(), Default::default()).unwrap();
    // Measured CPU stays in the modeled minute after the unidentifiable internal
    // grant is removed: active replay plus inferred background totals 90 core-s.
    assert!(
        (modeled["metrics"]["members"]["mini"]["cpu_core_seconds"]
            .as_f64()
            .unwrap()
            - 90.0)
            .abs()
            < 0.001,
        "{modeled}"
    );
    assert_eq!(selected.members[0].load.len(), source.members[0].load.len());
    assert_eq!(
        tuning_preflight::check(source, options()).unwrap()["passed"],
        true
    );
}
#[test]
fn explicit_bounds_reject_empty_reversed_outside_and_malformed_windows() {
    let source = fixture();
    for (since, until) in [
        ("bad", "2026-11-03T00:00:10Z"),
        ("2026-11-03T00:00:10Z", "2026-11-03T00:00:10Z"),
        ("2026-11-03T00:00:11Z", "2026-11-03T00:00:10Z"),
        ("2026-11-02T00:00:00Z", "2026-11-03T00:00:10Z"),
    ] {
        assert!(elsecall::workload::bounds::within(&source, Some(since), Some(until)).is_err());
    }
}
#[test]
fn cli_preflight_is_offline_and_returns_failure_for_all_blockers() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("fleet.json");
    let mut trace = fixture();
    trace.demand.clear();
    trace.members[0].load.clear();
    let bytes = serde_json::to_vec(&trace).unwrap();
    std::fs::write(&path, &bytes).unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_elsecall"))
        .env("HOME", temp.path())
        .args(["tune", "preflight", "--trace"])
        .arg(&path)
        .args(["--rehearsal-window", "explicit", "--min-interactive", "4"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["data"]["passed"], false, "{result}");
    assert!(
        result["data"]["blockers"].as_array().unwrap().len() >= 5,
        "{result}"
    );
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 1);
}

#[test]
fn same_request_reports_terminal_reference_demand_and_hold_problems_together() {
    let mut source = fixture();
    let r = &mut source.requests[0];
    r.fact.unfinished = true;
    r.fact.completed = false;
    r.fact.run_ms = None;
    r.fact.label = Some("unobserved family".into());
    r.fact.family = r.fact.label.clone();
    r.fact.resources.as_mut().unwrap().ended_at = None;
    r.outcome = None;
    r.hold_limit = None;
    r.urgency = elsecall::scheduling::request::Urgency::Interactive {
        hold_limit_ms: None,
    };
    let result = tuning_preflight::check(source, options()).unwrap();
    let blockers = result["blockers"].to_string();
    for reason in [
        "missing_terminal_outcome",
        "No reference run duration",
        "lacks identified H1 effective demand",
        "resolved hold limit",
        "terminal command exits",
    ] {
        assert!(blockers.contains(reason), "{reason}: {result}");
    }
}
#[test]
fn sensitivity_unknowns_are_reported_without_blocking_a_scorable_point() {
    let mut source = fixture();
    source.contention[0]
        .points
        .iter_mut()
        .for_each(|p| p.duration_ratio_range[1] = 100.0);
    let result = tuning_preflight::check(source, options()).unwrap();
    assert_eq!(result["passed"], true, "{result}");
    assert!(result["blockers"].as_array().unwrap().is_empty());
    assert!(
        result["tune"]["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["upper"]["overhead"]["mean"].is_null())
    );
}

#[test]
fn every_unsupported_nested_interval_names_the_child_record() {
    let mut source = fixture();
    for n in 5..=6 {
        let mut child = source.requests[0].clone();
        child.fact.run_id = format!("r-{n:012x}");
        child.fact.nested = true;
        child.nested_in = Some(source.requests[0].fact.run_id.clone());
        child.shares_capacity_of = child.nested_in.clone();
        child.fact.resources.as_mut().unwrap().ended_at =
            Some(source.since.after(Duration::from_secs(30)));
        child.fact.run_ms = Some(30_000);
        source.requests.push(child);
    }
    let result = tuning_preflight::check(source, options()).unwrap();
    let blockers = result["blockers"].to_string();
    assert!(blockers.contains("r-000000000005"), "{result}");
    assert!(blockers.contains("r-000000000006"), "{result}");
    assert!(blockers.contains("nested interval"), "{result}");
}

#[test]
fn calibration_cannot_select_a_factor_without_observed_rehearsals() {
    let mut source = fixture();
    source.members[0].load[0].in_use_mean = 2.0 * 4.0 / 60.0;
    source.members[0].load[0].queued_max = 3;
    let target =
        elsecall::busy_hour_calibration::Target::observed(&source, source.since, source.until)
            .unwrap();
    let mut args = options();
    args.busy_hours = Some(target);
    let result = tuning_preflight::check(source, args).unwrap();
    assert_eq!(result["passed"], false, "{result}");
    assert_eq!(
        result["calibration"]["chosen_load_factor"],
        serde_json::Value::Null
    );
    assert_eq!(result["calibration"]["trials"].as_array().unwrap().len(), 0);
    assert_eq!(result["calibration"]["matched"], false);
    assert_eq!(result["calibration"]["selection"], "unavailable");
    assert!(result["calibration"]["distance"].is_null());
    assert!(result["calibration"]["deviations"].is_null());
    // Four serialized commands owe four compressed seconds of work, even
    // though all their planned arrivals coincide.
    assert!(result["duration_estimate"]["arm_seconds"].as_f64().unwrap() >= 4.0 / 6.0);
}
#[test]
fn load_factor_replays_closer_arrivals_through_the_production_scheduler() {
    let mut source = fixture();
    for (n, r) in source.requests.iter_mut().enumerate() {
        r.fact.requested_at = source.since.after(Duration::from_secs(n as u64 * 2));
        r.fact.waited_ms = Some(0);
        r.fact.resources.as_mut().unwrap().started_at = Some(r.fact.requested_at);
        r.fact.resources.as_mut().unwrap().ended_at =
            Some(r.fact.requested_at.after(Duration::from_secs(1)));
    }
    let idle = elsecall::simulation::simulate(source.clone(), Default::default()).unwrap();
    let busy = elsecall::simulation::simulate(
        source,
        elsecall::simulation::Options {
            load_factor: "4".parse().unwrap(),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(idle["outcomes"].as_array().unwrap().len(), 4);
    assert_eq!(busy["outcomes"].as_array().unwrap().len(), 4);
    assert!(
        busy["outcomes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["run_ms"] == 1000)
    );
    assert!(
        busy["metrics"]["scheduling_objective"]["overhead"]["mean"]
            .as_f64()
            .unwrap()
            > idle["metrics"]["scheduling_objective"]["overhead"]["mean"]
                .as_f64()
                .unwrap()
    );
}

#[test]
fn busy_reference_requires_minute_bounds_and_observed_coverage() {
    let mut source = fixture();
    assert!(
        elsecall::busy_hour_calibration::Target::observed(
            &source,
            source.since,
            source.since.after(Duration::from_secs(1))
        )
        .is_err()
    );
    source.members[0].load[0].cpu_mean = None;
    assert!(
        elsecall::busy_hour_calibration::Target::observed(&source, source.since, source.until)
            .is_err()
    );
}

#[test]
fn current_placement_cli_preflight_and_offline_tools_read_acme_rules() {
    let temp = tempfile::tempdir().unwrap();
    let mut trace = fixture();
    let mut remote = trace.members[0].clone();
    remote.member = "devbox".into();
    trace.members.push(remote);
    let mut demand = trace.demand[0].clone();
    demand.member = "devbox".into();
    trace.demand.push(demand);
    let mut contention = trace.contention[0].clone();
    contention.member = "devbox".into();
    trace.contention.push(contention);
    for r in &mut trace.requests {
        r.fact.label = Some("acme test Checkout".into());
    }
    let input = temp.path().join("trace.json");
    let bytes = serde_json::to_vec(&trace).unwrap();
    std::fs::write(&input, &bytes).unwrap();
    let rules = temp.path().join("rules.toml");
    let rule_bytes = include_bytes!("fixtures/acme.elsecall.toml");
    std::fs::write(&rules, rule_bytes).unwrap();
    for command in [
        vec![
            "tune",
            "preflight",
            "--rehearsal-window",
            "explicit",
            "--min-interactive",
            "4",
        ],
        vec!["simulate"],
        vec!["tune", "--budget", "2"],
    ] {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_elsecall"))
            .args(command)
            .args([
                "--trace",
                input.to_str().unwrap(),
                "--placement",
                "current",
                "--rules",
                rules.to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let result: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(
            result["data"]["placement_replay"]["mode"], "current",
            "{result}"
        );
        if result["data"].get("fidelity_evidence").is_some() {
            assert_eq!(result["data"]["trusted_for_tuning"], false);
            assert!(
                result["data"]["fidelity_evidence"]
                    .to_string()
                    .contains("current placement is a counterfactual")
            );
        }
        if result["data"].get("preflight_version").is_some() {
            assert_eq!(result["data"]["passed"], true, "{result}");
            assert_eq!(result["data"]["placement_split"]["placeable"], 4);
        }
    }
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_elsecall"))
        .args([
            "simulate",
            "--trace",
            input.to_str().unwrap(),
            "--fidelity",
            "--placement",
            "current",
            "--rules",
            rules.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(std::fs::read(input).unwrap(), bytes);
    assert_eq!(std::fs::read(rules).unwrap(), rule_bytes);
}
