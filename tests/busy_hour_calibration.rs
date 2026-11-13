#![cfg(target_os = "macos")]
//! Real rehearsal services in disposable homes; only machine/discovery observations are scripted.
mod members;
mod processes;
use elsecall::{
    busy_hour_calibration::{Target, calibrate},
    replay_placement::{Mode, ReplayPlacement},
    scheduling::timestamp::Timestamp,
    workload::trace::Trace,
};
use members::{TestMember, enrol};
use serde_json::json;
use std::{
    fs,
    path::Path,
    process::Stdio,
    time::{Duration, Instant},
};

fn workload() -> Trace {
    let since = Timestamp::parse_rfc3339("2026-11-03T00:00:00Z").unwrap();
    let mut trace: Trace = serde_json::from_value(json!({
        "trace_version":1,"incomplete":false,"since":since,
        "until":since.after(Duration::from_secs(60)),"master":"alpha",
        "members":[],"requests":[],"leftovers":[],"demand":[],"contention":[],"limits":[]
    }))
    .unwrap();
    for member in ["alpha", "bravo"] {
        trace.members.push(serde_json::from_value(json!({
            "member":member,"errors":[],
            "scheduler_settings":[{"at":since,"capacity":{"weighted_cores":4,"reserved":0,"capacity":4},"parameters":{}}],
            "load":[{"at":since,"seconds":60,"cpu_mean":0,"cpu_max":0,"outside_mean":0,
                "weighted_cores":4,"capacity":4,"in_use_mean":0,"in_use_max":0,
                "queued_max":0,"interactive_queued_max":0,"saturated_seconds":0,"saturated_busy_seconds":0,
                "memory_pressure_seconds":0,"worst_memory_pressure":"normal","worst_thermal_state":"nominal",
                "speed_min":1,"speed_max":1}]
        })).unwrap());
        trace.demand.push(serde_json::from_value(json!({
            "member":member,"family":"build *","runs":10,"fitted_runs":10,
            "declared_cores":1,"process_tree_cores":0.25,"effective_cores":0.25,"interval_cores":[0.25,0.25],
            "fitted_minutes":10,"idle_minutes":3,"outside_baseline_cores":0,"residual_rmse_cores":0,"status":"identified"
        })).unwrap());
    }
    for (n, offset) in [0, 40_000].into_iter().enumerate() {
        let at = since.after(Duration::from_millis(offset));
        trace.requests.push(serde_json::from_value(json!({
            "run_id":format!("r-{:012x}",n+1),"kind":"run","label":"build fixture","family":"build *",
            "placement":"pinned","deferrable":false,"size":1,"calling_member":"alpha","member":"alpha",
            "nested":false,"requested_at":at,"unfinished":false,"verdict":"started","failure":null,
            "completed":true,"waited_ms":0,"run_ms":4000,"sync_ms":null,"warmth":null,"predicted":null,
            "resources":{"started_at":at,"ended_at":at.after(Duration::from_secs(4)),"cpu_ms":1000,"size_basis":{"from":"declared"}},
            "placement_request":"pinned","urgency":{"interactive":{"hold_limit_ms":10000}},
            "hold_limit":{"seconds":10,"basis":"requested","expected_run_seconds":null},
            "nested_in":null,"shares_capacity_of":null,"workspace":{"workspace":"a".repeat(64),"repository":"b".repeat(64)},
            "workspace_recorded":true,"pre_run_attempts":[],"pre_run_attempts_complete":true,
            "outcome":{"kind":"exited","exit_code":0},"decision":null
        })).unwrap());
    }
    elsecall::simulation::validate_trace(&trace).unwrap();
    trace
}

fn rehearsal_command(
    alpha: &TestMember,
    input: &Path,
    output: &Path,
    rules: Option<&Path>,
) -> std::process::Command {
    let mut command = processes::command(env!("CARGO_BIN_EXE_elsecall"));
    command
        .env("HOME", alpha.home.path())
        .current_dir(alpha.home.path())
        .args([
            "rehearse",
            "--trace",
            input.to_str().unwrap(),
            "--output",
            output.to_str().unwrap(),
            "--compress",
            "2",
            "--seed",
            "42",
            "--load-factor",
            "2",
            "--limit",
            "60s",
        ]);
    if let Some(rules) = rules {
        command.args(["--placement", "current", "--rules", rules.to_str().unwrap()]);
    }
    command
}

/// Build a minute-aligned reference from boundary observations, keeping waits and
/// queues at zero for this quiet fixture. Share matching is independent of host timing.
fn reference(observed: &Trace) -> Target {
    let mut reference = workload();
    for member in &mut reference.members {
        let recorded = observed
            .members
            .iter()
            .find(|m| m.member == member.member)
            .unwrap();
        let mut seconds = 0.0;
        let mut share = 0.0;
        for row in &recorded.load {
            let from = row.at.millis().max(observed.since.millis());
            let to = (row.at.millis() + 60_000).min(observed.until.millis());
            let weight = (to.saturating_sub(from) as f64 / 1000.0).min(row.seconds as f64);
            seconds += weight;
            share += row.in_use_mean / row.capacity.as_f64() * weight;
        }
        member.load[0].in_use_mean = share / seconds * member.load[0].capacity.as_f64();
        member.load[0].in_use_max = member.load[0].capacity;
    }
    Target::observed(&reference, reference.since, reference.until).unwrap()
}

fn exercise(mode: Mode) {
    let temp = tempfile::tempdir().unwrap();
    let load = temp.path().join("load.json");
    // Current placement must actually execute remotely, rather than merely being
    // eligible. The caller's positive capacity cannot fit this fixture's grant.
    fs::write(&load, json!({"performance_cores":1}).to_string()).unwrap();
    let placement = "[placement]\nrun_prior='1s'\nsync_prior_cold='0s'\nsync_prior_warm='0s'\ncold_run_factor=1\nkeep_local_margin=0\nkeep_local_margin_floor='0s'\n";
    let alpha = if mode == Mode::Current {
        TestMember::start_with(
            "alpha",
            &format!("[capacity]\nreserved_cores=0.5\n{placement}"),
            &[("ELSECALL_MACHINE_LOAD", &format!("file:{}", load.display()))],
        )
    } else {
        TestMember::start_with("alpha", placement, &[])
    };
    let bravo = TestMember::start_with("bravo", placement, &[]);
    enrol(&alpha, &bravo);
    let rules_path = temp.path().join("rules.toml");
    fs::write(
        &rules_path,
        "[[labels]]\nmatch='build *'\nplacement='placeable'\n",
    )
    .unwrap();
    let rules = (mode == Mode::Current).then_some(rules_path.as_path());
    let original = workload();
    let input = temp.path().join("workload.json");
    fs::write(&input, serde_json::to_vec(&original).unwrap()).unwrap();
    let mut replay = original.clone();
    ReplayPlacement {
        placement: mode,
        rules: rules.map(Path::to_path_buf),
    }
    .apply(&mut replay)
    .unwrap();
    let replay = elsecall::rehearsal::window::bound(&replay, replay.since, replay.until).unwrap();
    let output = temp.path().join("complete.json");
    let result = rehearsal_command(&alpha, &input, &output, rules)
        .output()
        .unwrap();
    assert!(result.status.success(), "{result:?}");
    let data = members::json(&result)["data"].clone();
    assert_eq!(data["recording_complete"], true, "{data}");
    assert_eq!(data["cleanup_complete"], true, "{data}");
    let observed: Trace = serde_json::from_slice(&fs::read(&output).unwrap()).unwrap();
    let host = if mode == Mode::Current {
        "bravo"
    } else {
        "alpha"
    };
    assert!(
        observed
            .requests
            .iter()
            .all(|r| r.fact.member.as_deref() == Some(host)),
        "{data}"
    );
    let target = reference(&observed);
    let (factor, report) =
        calibrate(&replay, &target, std::slice::from_ref(&observed), 2.0, 42).unwrap();
    assert_eq!(f64::from(factor.unwrap()), 2.0, "{report}");
    assert_eq!(report["trials"][0]["observed"], true);
    assert_eq!(report["selection"], "matched");
    assert_eq!(report["distance"], 0.0);
    eprintln!("{mode:?} offline calibration: {report}");
    // Exercise selection through preflight too: its exported workload contains
    // selection metadata absent from the source passed to the real rehearsal.
    let preflight_options = || elsecall::tuning_preflight::Options {
        observed_rehearsals: vec![observed.clone()],
        busy_hours: Some(target.clone()),
        load_factor: Default::default(),
        rehearsal_window: "explicit".parse().unwrap(),
        min_interactive: std::num::NonZeroUsize::new(2).unwrap(),
        compress: 2.0,
        limit: Duration::from_secs(60),
        sample: 2,
        iteration_budget: 2,
        seed: 42,
    };
    let preflight = elsecall::tuning_preflight::check(replay.clone(), preflight_options()).unwrap();
    assert_eq!(
        preflight["calibration"]["chosen_load_factor"], 2.0,
        "{preflight}"
    );

    // An observed arm remains selectable when every band cannot be satisfied.
    // Move just the reference queue peak beyond this quiet arm's band.
    let mut divergent_reference = workload();
    divergent_reference.members[0].load[0].queued_max = 20;
    divergent_reference.members[0].load[0].interactive_queued_max = Some(20);
    let nearest_target = Target::observed(
        &divergent_reference,
        divergent_reference.since,
        divergent_reference.until,
    )
    .unwrap();
    let mut options = preflight_options();
    options.busy_hours = Some(nearest_target);
    let nearest = elsecall::tuning_preflight::check(replay.clone(), options).unwrap();
    assert_eq!(
        nearest["calibration"]["chosen_load_factor"], 2.0,
        "{nearest}"
    );
    assert_eq!(nearest["calibration"]["selection"], "nearest_match");
    assert_eq!(nearest["calibration"]["matched"], false);
    assert!(nearest["calibration"]["distance"].as_f64().unwrap() > 0.0);
    assert!(
        nearest["calibration"]["deviations"]["queue_max"]["deviation"]
            .as_f64()
            .unwrap()
            < 0.0
    );
    assert_eq!(nearest["load_factor"], 2.0);
    assert!(
        !nearest["blockers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["stage"] == "calibration")
    );

    let shares = &report["trials"][0]["shape"]["member_admitted_capacity_share"];
    assert!(shares[host].as_f64().unwrap() > 0.0, "{report}");
    if mode == Mode::Current {
        assert_eq!(shares["alpha"], 0.0);
        assert_eq!(data["placement_replay"]["split"]["placeable"], 2);
        let mut different_rules = replay.clone();
        different_rules
            .placement_replay
            .as_mut()
            .unwrap()
            .rules_digest =
            elsecall::build_identity::Digest::of(b"different rules with the same placement");
        let error =
            elsecall::tuning_preflight::check(different_rules, preflight_options()).unwrap_err();
        assert!(
            error.message.contains("workload, compression or seed"),
            "{error:?}"
        );
        // Current rules are part of experiment identity; historical arms cannot
        // be pooled merely because their request counts happen to match.
        let historical =
            elsecall::rehearsal::window::bound(&original, original.since, original.until).unwrap();
        assert!(
            calibrate(
                &historical,
                &target,
                std::slice::from_ref(&observed),
                2.0,
                42
            )
            .is_err()
        );
    }
    let mut different_work = replay.clone();
    different_work.demand[0].effective_cores = Some(0.3);
    different_work.demand[0].interval_cores = Some([0.3, 0.3]);
    let error = elsecall::tuning_preflight::check(different_work, preflight_options()).unwrap_err();
    assert!(
        error.message.contains("workload, compression or seed"),
        "{error:?}"
    );
    let mut missing = observed.clone();
    missing.requests.pop(); // even a true completeness flag cannot cover lost work
    assert!(calibrate(&replay, &target, &[missing], 2.0, 42).is_err());

    let interrupted_path = temp.path().join("interrupted.json");
    let child = rehearsal_command(&alpha, &input, &interrupted_path, rules)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    // Stop between arrivals after the first new request has drained.
    let deadline = Instant::now() + processes::WAIT;
    loop {
        let runs = alpha.ok(&["runs"]);
        if runs["runs"].as_array().unwrap().iter().any(|r| {
            r["state"] == "ended"
                && observed
                    .requests
                    .iter()
                    .all(|old| r["run_id"] != old.fact.run_id)
                && r["label"]
                    .as_str()
                    .is_some_and(|l| l.starts_with("rehearsal:"))
        }) {
            break;
        }
        assert!(Instant::now() < deadline, "{runs}");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGTERM) }, 0);
    let stopped = child.wait_with_output().unwrap();
    let stopped = members::json(&stopped);
    assert_eq!(stopped["data"]["recording_complete"], false, "{stopped}");
    assert_eq!(stopped["data"]["cleanup_complete"], true, "{stopped}");
    let interrupted: Trace = serde_json::from_slice(&fs::read(interrupted_path).unwrap()).unwrap();
    assert!(calibrate(&replay, &target, &[interrupted], 2.0, 42).is_err());
}

#[test]
fn current_rule_placeable_arms_select_from_observed_fleet_metrics_and_reject_interruption() {
    exercise(Mode::Current);
}

#[test]
fn historical_local_arms_still_calibrate_and_reject_interruption() {
    exercise(Mode::Historical);
}
