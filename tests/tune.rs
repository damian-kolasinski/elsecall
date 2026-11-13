//! End-to-end search over the real scheduler with virtual boundary adapters.
use elsecall::{
    scheduling::{capacity::Cores, timestamp::Timestamp},
    tuning::{self, Space, TuneOptions},
    workload::trace::Trace,
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, time::Duration};
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
fn options() -> TuneOptions {
    TuneOptions {
        space: Some(BTreeMap::from([(
            "label.work.size".into(),
            vec!["1".into(), "2".into()],
        )])),
        budget: 3,
        load_factor: Default::default(),
        seed: 42,
        top: 2,
        day: None,
        held_out_day: None,
    }
}
#[test]
fn finds_and_proposes_known_capacity_optimum_within_budget() {
    let result = tuning::tune(fixture(), None, options()).unwrap();
    assert_eq!(result["tuning_version"], 2);
    assert_eq!(result["baseline"]["simulation_version"], 2);
    assert_eq!(result["best"]["settings"]["label.work.size"], "1");
    assert_eq!(result["evaluated_candidates"], 3);
    assert_eq!(
        result["baseline"]["metrics"]["objective"]["overhead"]["mean"],
        1.5
    );
    assert_eq!(
        result["best"]["evaluation"]["metrics"]["objective"]["overhead"]["mean"],
        0.5
    );
    assert_eq!(result["best"]["evaluation"]["metrics"]["score"], 0.5);
    assert_eq!(
        result["baseline"]["metrics"]["scheduling_objective"]["reserve_violation_seconds"],
        0.0
    );

    assert_eq!(
        result["baseline"]["metrics"]["objective"]["mean_charged_seconds"],
        2.5
    );
    assert_eq!(
        result["best"]["evaluation"]["metrics"]["objective"]["mean_charged_seconds"],
        1.5
    );
    assert!(
        result["proposed_change"]["consumer_elsecall_toml"]
            .as_str()
            .unwrap()
            .contains("size = \"1\"")
    );
    assert_eq!(result["proposal_status"], "untrusted");
    assert_eq!(result["accepted_win"], false);
    assert_eq!(
        result["parameter_sensitivity"]["label.work.size"]["coverage"],
        "complete"
    );
    let mut again = tuning::tune(fixture(), None, options()).unwrap();
    let mut result = result;
    result.as_object_mut().unwrap().remove("performance");
    again.as_object_mut().unwrap().remove("performance");
    assert_eq!(result, again);
}
#[test]
fn service_validation_and_bounds_reject_invalid_spaces() {
    for (key, values) in [
        ("member.mini.reserved_cores", vec!["0"]),
        ("label.work.size", vec!["0"]),
        ("admission.saturated_utilization", vec!["1.1"]),
        ("hold_limit", vec!["25h"]),
        ("placement.run_prior", vec!["0s"]),
        ("estimates.minimum_runs", vec!["101"]),
    ] {
        let mut options = options();
        options.space = Some(Space::from([(
            key.into(),
            values.into_iter().map(String::from).collect(),
        )]));
        assert!(tuning::tune(fixture(), None, options).is_err(), "{key}");
    }
    for budget in [0, 1025] {
        let mut o = options();
        o.budget = budget;
        assert!(tuning::tune(fixture(), None, o).is_err());
    }
    let mut o = options();
    o.budget = 1;
    let r = tuning::tune(fixture(), None, o).unwrap();
    assert_eq!(r["evaluated_candidates"], 1);
    assert_eq!(
        r["parameter_sensitivity"]["label.work.size"]["coverage"],
        "budget_limited"
    );
}
#[test]
fn held_out_regression_is_flagged_and_does_not_change_search() {
    let tuning = fixture();
    let mut held = fixture();
    let shift = Duration::from_secs(86400);
    held.since = held.since.after(shift);
    held.until = held.until.after(shift);
    for m in &mut held.members {
        for s in &mut m.scheduler_settings {
            s.at = s.at.after(shift);
        }
        for l in &mut m.load {
            l.at = l.at.after(shift);
        }
    }
    // This day's tasks really consume the full usable capacity: smaller declarations
    // cannot buy time by violating the effective-demand reserve constraint.
    for d in &mut held.demand {
        d.effective_cores = Some(2.0);
        d.interval_cores = Some([2.0, 2.0]);
    }
    for r in &mut held.requests {
        r.fact.requested_at = r.fact.requested_at.after(shift);
        let u = r.fact.resources.as_mut().unwrap();
        u.started_at = u.started_at.map(|t| t.after(shift));
        u.ended_at = u.ended_at.map(|t| t.after(shift));
    }
    let r = tuning::tune(tuning, Some(held), options()).unwrap();
    assert_eq!(r["best"]["settings"]["label.work.size"], "1");
    assert_eq!(r["wins_only_on_tuning_trace"], true);
    assert_eq!(r["held_out"]["wins"], false);
    assert_eq!(r["accepted_win"], false);
    assert!(tuning::tune(fixture(), Some(fixture()), options()).is_err());
}
#[test]
fn cli_is_offline_and_never_writes_proposed_files() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("trace.json");
    let spec = dir.path().join("space.json");
    let trace = serde_json::to_vec(&fixture()).unwrap();
    std::fs::write(&input, &trace).unwrap();
    std::fs::write(&spec, r#"{"label.work.size":["1","2"]}"#).unwrap();
    let result = std::process::Command::new(env!("CARGO_BIN_EXE_elsecall"))
        .current_dir(dir.path())
        .env("HOME", dir.path())
        .args(["tune", "--trace"])
        .arg(&input)
        .arg("--space")
        .arg(&spec)
        .args(["--budget", "3", "--seed", "42"])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stdout)
    );
    let v: Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(v["data"]["best"]["settings"]["label.work.size"], "1");
    assert_eq!(std::fs::read(&input).unwrap(), trace);
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
}
#[test]
fn default_space_preserves_all_recorded_reserve_floors() {
    let mut t = fixture();
    let mut snapshot = t.members[0].scheduler_settings[0].clone();
    snapshot.capacity.reserved = Cores::from_millis(1500);
    snapshot.capacity.capacity = Cores::from_millis(1500);
    snapshot.at = snapshot.at.after(Duration::from_secs(1));
    t.members[0].scheduler_settings.push(snapshot);
    let mut o = options();
    o.space = None;
    o.budget = 1;
    let r = tuning::tune(t, None, o).unwrap();
    assert_eq!(
        r["space"]["member.mini.reserved_cores"],
        json!(["1.5", "2", "2.5"])
    );
}

#[test]
fn budget_is_hard_even_when_no_neighbor_improves() {
    let mut options = options();
    options.budget = 2;
    options.space = Some(Space::from([(
        "placement.keep_local_margin".into(),
        vec!["0.1".into(), "0.2".into(), "0.3".into()],
    )]));
    let r = tuning::tune(fixture(), None, options).unwrap();
    assert_eq!(r["evaluated_candidates"], 2);
}
#[test]
fn changed_service_projection_uses_synced_member_overlays() {
    let mut options = options();
    options.space = Some(Space::from([(
        "placement.keep_local_margin".into(),
        vec!["0".into()],
    )]));
    // Exercise proposal generation even when the unchanged pinned schedule wins.
    let r = tuning::tune(fixture(), None, options).unwrap();
    assert_eq!(r["best"]["settings"], json!({}));
    assert_eq!(r["proposed_change"]["changes"], json!([]));
}
