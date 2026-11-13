#![cfg(target_os = "macos")]
//! Real fleet services and normal run clients, with only discovery/load scripted.
mod members;
mod processes;
use elsecall::{scheduling::timestamp::Timestamp, workload::trace::Trace};
use members::{TestMember, enrol};
use serde_json::{Value, json};
use std::{
    fs,
    path::Path,
    process::{Child, Output},
    thread::sleep,
    time::{Duration, Instant},
};

fn fleet() -> (TestMember, TestMember) {
    let alpha = TestMember::start("alpha");
    let bravo = TestMember::start("bravo");
    enrol(&alpha, &bravo);
    (alpha, bravo)
}
fn fixture(directory: &Path, offsets: &[u64]) -> std::path::PathBuf {
    let start = Timestamp::now().millis() - 86_400_000;
    let requests:Vec<_>=offsets.iter().enumerate().map(|(i,offset)| {
        let member=if i==3 {"bravo"} else {"alpha"};
        let placement=if i==4{json!("placeable")}else if matches!(i,1|2){json!({"member":"bravo"})}else{json!("pinned")};
        json!({"run_id":format!("r-{:012x}",i+1),"kind":"run","label":"build fixture","family":"build *","placement":if i==4{"placeable"}else if matches!(i,1|2){"required"}else{"pinned"},"deferrable":false,"size":if i==5{100000.0}else{0.5},"calling_member":member,"member":if matches!(i,1|2){"bravo"}else{member},"nested":false,"requested_at":Timestamp::from_millis(start+offset),"unfinished":false,"verdict":"started","failure":null,"completed":true,"waited_ms":0,"run_ms":800,"sync_ms":null,"warmth":null,"predicted":null,"resources":{"started_at":Timestamp::from_millis(start+offset),"ended_at":Timestamp::from_millis(start+offset+800),"cpu_ms":200,"size_basis":{"from":"declared"}},"placement_request":placement,"urgency":{"interactive":{"hold_limit_ms":2000}},"hold_limit":{"seconds":2.0,"basis":"requested","expected_run_seconds":null},"nested_in":null,"shares_capacity_of":null,"workspace":{"workspace":"a".repeat(64),"repository":"b".repeat(64)},"workspace_recorded":true,"pre_run_attempts":[],"pre_run_attempts_complete":true,"outcome":{"kind":"exited","exit_code":if i==3{7}else{0}},"decision":null})
    }).collect();
    let demand:Vec<_>=["alpha","bravo"].into_iter().map(|m|json!({"member":m,"family":"build *","runs":10,"fitted_runs":10,"declared_cores":0.5,"process_tree_cores":0.25,"effective_cores":0.25,"interval_cores":[0.15,0.35],"fitted_minutes":10,"idle_minutes":3,"outside_baseline_cores":0.0,"residual_rmse_cores":0.01,"status":"identified"})).collect();
    let document = json!({"trace_version":1,"incomplete":false,"since":Timestamp::from_millis(start),"until":Timestamp::from_millis(start+offsets.iter().max().unwrap()+2000),"master":"alpha","members":[{"member":"alpha","load":[],"errors":[],"scheduler_settings":[]},{"member":"bravo","load":[],"errors":[],"scheduler_settings":[]}],"requests":requests,"leftovers":[],"demand":demand,"contention":[],"limits":[]});
    let trace: Trace = serde_json::from_value(document.clone()).unwrap();
    elsecall::simulation::validate_trace(&trace).unwrap();
    let path = directory.join("input.json");
    fs::write(&path, document.to_string()).unwrap();
    path
}
// Trust lifecycle coverage needs a passing real trial, not a short CPU benchmark.
// Pace a small positive CPU budget over 30 seconds after compression. This leaves
// scheduling headroom inside the unchanged 5% fidelity threshold without adding
// CPU load. Completion, full observation coverage and certification remain required.
fn certification_fixture(directory: &Path) -> std::path::PathBuf {
    let input = fixture(directory, &[0]);
    let mut source: Value = serde_json::from_slice(&fs::read(&input).unwrap()).unwrap();
    let request = &mut source["requests"][0];
    request["run_ms"] = json!(60_000);
    let at = Timestamp::parse_rfc3339(request["requested_at"].as_str().unwrap()).unwrap();
    request["resources"]["ended_at"] = json!(at.after(Duration::from_secs(60)));
    request["resources"]["cpu_ms"] = json!(300);
    for demand in source["demand"].as_array_mut().unwrap() {
        demand["effective_cores"] = json!(0.005);
        demand["process_tree_cores"] = json!(0.005);
        demand["interval_cores"] = json!([0.003, 0.007]);
    }
    fs::write(&input, source.to_string()).unwrap();
    input
}

fn rehearse(alpha: &TestMember, input: &Path, output: &Path, extra: &[&str]) -> Output {
    rehearse_command(alpha, input, output, extra)
        .output()
        .unwrap()
}

fn rehearse_command(
    alpha: &TestMember,
    input: &Path,
    output: &Path,
    extra: &[&str],
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
        ]);
    if !extra.contains(&"--limit") {
        command.args(["--limit", "120s"]);
    }
    command.args(extra);
    command
}

fn production(member: &TestMember, seconds: &str) -> Child {
    processes::command(env!("CARGO_BIN_EXE_elsecall"))
        .env("HOME", member.home.path())
        .current_dir(member.home.path())
        .args([
            "run",
            "--pinned",
            "--label",
            "production build",
            "--",
            "/bin/sleep",
            seconds,
        ])
        .spawn()
        .unwrap()
}
fn wait_running(member: &TestMember, label: &str) {
    let end = Instant::now() + processes::WAIT;
    loop {
        if member.ok(&["runs"])["runs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| {
                r["label"].as_str().is_some_and(|l| {
                    l == label
                        || label.starts_with("rehearsal:")
                            && l.starts_with("rehearsal:")
                            && l.ends_with(label.strip_prefix("rehearsal:").unwrap())
                }) && r["state"] == "running"
            })
        {
            return;
        }
        assert!(Instant::now() < end);
        sleep(Duration::from_millis(20));
    }
}
fn assert_clean(alpha: &TestMember, bravo: &TestMember, session: &str) {
    for member in [alpha, bravo] {
        let runs = member.ok(&["runs", "--limit", "500"]);
        assert!(
            runs["runs"].as_array().unwrap().iter().all(|r| r["label"]
                .as_str()
                .is_none_or(|l| !l.starts_with("rehearsal:"))
                || r["state"] == "ended"),
            "{runs}"
        );
        let listing = member.ok(&["copies"]);
        assert!(
            listing["copies"].as_array().unwrap().is_empty(),
            "{listing}"
        );
        assert!(
            listing["stores"].as_array().unwrap().is_empty(),
            "{listing}"
        );
        assert!(!member.data().join("rehearsal-session.json").exists());
        let cache = member.data().join("workspaces");
        assert!(fs::read_dir(cache).map_or(true, |mut entries| entries.next().is_none()));
    }
    assert!(!fs::read_dir("/private/tmp").unwrap().flatten().any(|e| {
        e.file_name()
            .to_string_lossy()
            .starts_with(&format!("elsecall-rehearsal-{session}-"))
    }));
}
#[test]
fn perf_rehearses_timing_members_outcomes_real_cold_warm_copies_and_own_fidelity() {
    let (alpha, bravo) = fleet();
    let temp = tempfile::tempdir().unwrap();
    let input = fixture(temp.path(), &[0, 2000, 4000, 6000, 8000, 10000]);
    let mut source: Value = serde_json::from_slice(&fs::read(&input).unwrap()).unwrap();
    for request in source["requests"].as_array_mut().unwrap() {
        request["hold_limit"]["seconds"] = json!(120.0);
        request["urgency"]["interactive"]["hold_limit_ms"] = json!(120_000);
    }
    fs::write(&input, source.to_string()).unwrap();
    let output = temp.path().join("observed.json");
    let result = rehearse(&alpha, &input, &output, &[]);
    assert!(result.status.success(), "{result:?}");
    let result = members::json(&result)["data"].clone();
    assert_eq!(result["submitted"], 6, "{result}");
    assert_eq!(result["cleanup_complete"], true, "{result}");
    let trace: Value = serde_json::from_slice(&fs::read(&output).unwrap()).unwrap();
    // Source identity is explicit; overdue arrivals can reach the service in
    // another order when their normal run clients are scheduled concurrently.
    let requests: Vec<_> = result["requests"]
        .as_array()
        .unwrap()
        .iter()
        .map(|mapping| {
            trace["requests"]
                .as_array()
                .unwrap()
                .iter()
                .find(|r| r["run_id"] == mapping["job"]["run_id"])
                .unwrap()
        })
        .collect();
    assert_eq!(requests.len(), 6, "{result}");
    assert_eq!(trace["requests"].as_array().unwrap().len(), 6, "{trace}");
    let mapped_ids: std::collections::BTreeSet<_> = requests
        .iter()
        .map(|r| r["run_id"].as_str().unwrap())
        .collect();
    assert_eq!(mapped_ids.len(), 6, "{result}");
    assert!(
        requests
            .iter()
            .all(|r| r["synthetic"] == true
                && r["label"].as_str().unwrap().starts_with("rehearsal:")),
        "{trace}"
    );
    assert_eq!(
        requests[..4]
            .iter()
            .map(|r| r["member"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["alpha", "bravo", "bravo", "bravo"]
    );
    // Both required requests use the same remote copy. Prove cold then warm
    // in execution order, independently of run-client scheduling order.
    let mut remote = [&requests[1], &requests[2]];
    remote.sort_by_key(|r| r["resources"]["started_at"].as_str().unwrap());
    assert_eq!(remote[0]["warmth"], "cold");
    assert_eq!(remote[1]["warmth"], "warm");
    assert_eq!(requests[3]["outcome"]["exit_code"], 7);
    for (index, mapping) in result["requests"].as_array().unwrap().iter().enumerate() {
        assert_eq!(mapping["original_run_id"], format!("r-{:012x}", index + 1));
        assert_eq!(mapping["arrival_offset_ms"], index as u64 * 1000);
        assert!(
            mapping["arrival_error_ms"].as_i64().unwrap() >= 0,
            "{mapping}"
        );
        assert!(
            mapping["arrival_error_ms"].as_u64().unwrap() < 120_000,
            "{mapping}"
        );
        let id = mapping["job"]["run_id"].as_str().unwrap();
        assert_eq!(trace["rehearsal"]["duration_ms"][id], 400);
        assert!(mapping["fixed_work"]["iterations"].as_u64().unwrap() > 0);
    }
    if std::env::var_os("EC_TEST_PERF").is_some() {
        for pair in requests.windows(2) {
            let a = Timestamp::parse_rfc3339(pair[0]["requested_at"].as_str().unwrap()).unwrap();
            let b = Timestamp::parse_rfc3339(pair[1]["requested_at"].as_str().unwrap()).unwrap();
            assert!(b.millis().abs_diff(a.millis() + 1000) < 700, "{pair:?}");
        }
    }
    assert_eq!(requests[4]["placement_request"], "placeable");
    assert_eq!(requests[5]["failure"], "request_too_large");
    for r in &requests[..5] {
        assert_eq!(r["size"], 0.5);
        assert_eq!(r["hold_limit"]["seconds"], 120.0);
        let id = r["run_id"].as_str().unwrap();
        let member = r["member"].as_str().unwrap();
        let reference = trace["rehearsal"]["uncontended_ms"][id][member]
            .as_f64()
            .unwrap();
        assert!(
            r["run_ms"].as_u64().unwrap() as f64 >= reference * 0.9,
            "{r}"
        );
        assert_eq!(
            r["outcome"]["exit_code"],
            if r["calling_member"] == "bravo" { 7 } else { 0 }
        );
        if std::env::var_os("EC_TEST_PERF").is_some() {
            assert!(r["run_ms"].as_u64().unwrap().abs_diff(400) < 500, "{r}");
        }
    }
    let offline = alpha.ok(&[
        "simulate",
        "--trace",
        output.to_str().unwrap(),
        "--fidelity",
        "--seed",
        "0",
    ]);
    assert_eq!(
        result["simulation"], offline,
        "must simulate exactly the exported rehearsal, not the source trace"
    );
    assert_eq!(result["simulation"]["fidelity"]["since"], trace["since"]);
    assert!(
        !result["simulation"]["fidelity"]["blockers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b.as_str().unwrap().contains("complete recorded UTC day"))
    );
    let report = alpha.ok(&["report", "--since", "1h"]);
    assert_eq!(report["fleet"]["requests"]["total"], 0, "{report}");
    let records = elsecall::scheduling::records::Records::open_read_only(
        &alpha.data().join("records.sqlite"),
    )
    .unwrap();
    assert!(
        records
            .labelled_since(Timestamp::from_millis(0), 2000)
            .unwrap()
            .is_empty()
    );
    assert_clean(&alpha, &bravo, result["session"].as_str().unwrap());
}
#[test]
fn refuses_existing_production_on_another_member() {
    let (alpha, bravo) = fleet();
    let temp = tempfile::tempdir().unwrap();
    let input = fixture(temp.path(), &[0]);
    let output = temp.path().join("observed.json");
    let mut work = production(&bravo, "2");
    wait_running(&bravo, "production build");
    let result = rehearse(&alpha, &input, &output, &[]);
    assert!(!result.status.success());
    assert_eq!(
        members::json(&result)["error"]["code"],
        "rehearsal_busy",
        "{result:?}"
    );
    assert!(!output.exists());
    work.wait().unwrap();
}
#[test]
fn stops_new_submissions_when_production_arrives_and_finishes_in_flight() {
    let (alpha, bravo) = fleet();
    let temp = tempfile::tempdir().unwrap();
    let input = fixture(temp.path(), &[0, 6000, 10000]);
    let output = temp.path().join("observed.json");
    let replay = processes::command(env!("CARGO_BIN_EXE_elsecall"))
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
            "--limit",
            "20s",
        ])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    wait_running(&alpha, "rehearsal:build fixture");
    let mut work = production(&bravo, "0.1");
    work.wait().unwrap();
    let result = replay.wait_with_output().unwrap();
    assert!(result.status.success(), "{result:?}");
    let result = members::json(&result)["data"].clone();
    assert_eq!(result["stop_reason"], "production_arrived", "{result}");
    assert!(result["certificate"].is_null());
    assert_eq!(result["submitted"], 1, "{result}");
    assert_eq!(result["metrics"]["completed"], 1, "{result}");
    assert_clean(&alpha, &bravo, result["session"].as_str().unwrap());
}

#[test]
fn synthetic_runs_preserve_production_size_time_and_speed_learning() {
    let (alpha, _bravo) = fleet();
    let temp = tempfile::tempdir().unwrap();
    for _ in 0..3 {
        let run = alpha.run(&[
            "run",
            "--pinned",
            "--label",
            "build fixture",
            "--size",
            "1",
            "--",
            "/bin/sleep",
            "1.05",
        ]);
        assert!(run.status.success(), "{run:?}");
    }
    let records = elsecall::scheduling::records::Records::open_read_only(
        &alpha.data().join("records.sqlite"),
    )
    .unwrap();
    let history = records
        .labelled_since(Timestamp::from_millis(0), 2000)
        .unwrap();
    let times = records.run_times("build fixture").unwrap();
    let input = fixture(temp.path(), &[0, 2000, 4000]);
    let output = temp.path().join("observed.json");
    let result = rehearse(&alpha, &input, &output, &[]);
    assert!(result.status.success(), "{result:?}");
    assert_eq!(
        records
            .labelled_since(Timestamp::from_millis(0), 2000)
            .unwrap(),
        history
    );
    assert_eq!(records.run_times("build fixture").unwrap(), times);
    let run = alpha.run(&[
        "run",
        "--pinned",
        "--label",
        "build fixture",
        "--",
        "/bin/sleep",
        "0.01",
    ]);
    assert!(run.status.success());
    let latest = alpha.ok(&["runs", "--limit", "1"])["runs"][0]["run_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let learned = alpha.ok(&["explain", &latest]);
    assert_eq!(learned["size_basis"]["from"], "learned", "{learned}");
    assert_eq!(learned["size"], 0.5, "{learned}");
    assert!(
        (learned["hold_limit"]["expected_run_seconds"]
            .as_f64()
            .unwrap()
            - 1.05)
            .abs()
            < 0.25,
        "{learned}"
    );
    assert_eq!(
        records
            .labelled_since(Timestamp::from_millis(0), 2000)
            .unwrap()
            .len(),
        4
    );
}
#[test]
fn limit_stops_in_flight_local_and_remote_work_and_exports_it() {
    // Retain ownership through assertion failures, including an exit timeout.
    struct Controller(Child);
    impl Drop for Controller {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    let (alpha, bravo) = fleet();
    let temp = tempfile::tempdir().unwrap();
    // The bound includes two calibrations, scratch preparation and a remote
    // copy. Leave time to reach execution before testing in-flight expiry.
    let input = fixture(temp.path(), &[0, 0, 20000]);
    let mut source: Value = serde_json::from_slice(&fs::read(&input).unwrap()).unwrap();
    for r in source["requests"].as_array_mut().unwrap() {
        r["run_ms"] = json!(20000);
    }
    fs::write(&input, source.to_string()).unwrap();
    let output = temp.path().join("observed.json");
    let stdout = temp.path().join("controller.stdout");
    let stderr = temp.path().join("controller.stderr");
    let start = Instant::now();
    // File-backed output cannot fill a pipe while we observe controller exit.
    let mut controller = Controller(
        rehearse_command(&alpha, &input, &output, &["--limit", "6s"])
            .stdout(fs::File::create(&stdout).unwrap())
            .stderr(fs::File::create(&stderr).unwrap())
            .spawn()
            .unwrap(),
    );
    let status = loop {
        if let Some(status) = controller.0.try_wait().unwrap() {
            break status;
        }
        assert!(
            start.elapsed() < processes::WAIT,
            "controller did not exit: stdout={} stderr={} alpha={} bravo={}",
            fs::read_to_string(&stdout).unwrap(),
            fs::read_to_string(&stderr).unwrap(),
            alpha.log(),
            bravo.log()
        );
        sleep(Duration::from_millis(20));
    };
    let result = Output {
        status,
        stdout: fs::read(&stdout).unwrap(),
        stderr: fs::read(&stderr).unwrap(),
    };
    assert!(
        result.status.success(),
        "{result:?}; alpha={}; bravo={}",
        alpha.log(),
        bravo.log()
    );
    let result = members::json(&result)["data"].clone();
    eprintln!(
        "in-flight limit: controller_ms={} cleanup_complete={} cleanup_errors={}",
        start.elapsed().as_millis(),
        result["cleanup_complete"],
        result["cleanup_errors"]
    );
    assert_eq!(result["stop_reason"], "limit", "{result}");
    assert!(result["certificate"].is_null());
    assert_eq!(result["submitted"], 2, "{result}");
    assert!(start.elapsed() >= Duration::from_secs(6), "{result}");
    let trace: Value = serde_json::from_slice(&fs::read(&output).unwrap()).unwrap();
    assert_eq!(trace["requests"].as_array().unwrap().len(), 2, "{trace}");
    assert!(
        trace["requests"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["unfinished"] == false),
        "{trace}"
    );
    let members: std::collections::BTreeSet<_> = trace["requests"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            assert!(r["resources"]["started_at"].is_string(), "{trace}");
            assert!(r["outcome"].is_object(), "{trace}");
            assert_ne!(
                r["outcome"],
                json!({"kind":"exited", "exit_code":0}),
                "{trace}"
            );
            r["member"].as_str().unwrap()
        })
        .collect();
    assert_eq!(
        members,
        std::collections::BTreeSet::from(["alpha", "bravo"])
    );
    assert!(result["simulation"]["fidelity"].is_object());
    assert_eq!(result["cleanup_complete"], true, "{result}");
    assert_clean(&alpha, &bravo, result["session"].as_str().unwrap());
}
#[test]
fn repeated_cleanup_of_a_rehearsal_with_a_remote_copy_is_successful() {
    let (alpha, bravo) = fleet();
    let temp = tempfile::tempdir().unwrap();
    let input = fixture(temp.path(), &[0, 0]);
    let output = temp.path().join("observed.json");
    // Finish well before the member watchdog: the controller cleans first.
    let result = rehearse(&alpha, &input, &output, &[]);
    assert!(result.status.success(), "{result:?}");
    let data = members::json(&result)["data"].clone();
    assert_eq!(data["stop_reason"], "completed", "{data}");
    assert_eq!(data["submitted"], 2, "{data}");
    assert_eq!(data["cleanup_complete"], true, "{data}");
    let trace: Value = serde_json::from_slice(&fs::read(&output).unwrap()).unwrap();
    assert_eq!(trace["requests"].as_array().unwrap().len(), 2, "{trace}");
    assert!(
        trace["requests"].as_array().unwrap().iter().any(|r| {
            r["member"] == "bravo"
                && r["warmth"] == "cold"
                && r["resources"]["started_at"].is_string()
                && r["unfinished"] == false
        }),
        "{trace}"
    );
    let session = data["session"].as_str().unwrap();
    assert_clean(&alpha, &bravo, session);
    // The watchdog and controller can both clean at --limit. Reproduce that
    // ordering explicitly, without depending on which wins a scheduling race.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    for member in [&alpha, &bravo] {
        let reply: elsecall::rehearsal::protocol::Reply = runtime
            .block_on(elsecall::client::post_large(
                &elsecall::paths::Paths::for_home(alpha.home.path())
                    .unwrap()
                    .socket(),
                "/v1/rehearsal",
                &elsecall::rehearsal::protocol::Call {
                    member: member.name.into(),
                    operation: elsecall::rehearsal::protocol::Action::Clean {
                        session: session.into(),
                    },
                },
            ))
            .unwrap();
        assert!(reply.clean);
    }
    assert_clean(&alpha, &bravo, session);
}

#[test]
fn abandoned_controller_is_cleaned_by_member_watchdogs() {
    let (alpha, bravo) = fleet();
    let temp = tempfile::tempdir().unwrap();
    let input = fixture(temp.path(), &[0, 0, 10000]);
    let mut source: Value = serde_json::from_slice(&fs::read(&input).unwrap()).unwrap();
    for r in source["requests"].as_array_mut().unwrap() {
        r["run_ms"] = json!(20000);
    }
    fs::write(&input, source.to_string()).unwrap();
    let output = temp.path().join("observed.json");
    let mut replay = processes::command(env!("CARGO_BIN_EXE_elsecall"))
        .env("HOME", alpha.home.path())
        .args([
            "rehearse",
            "--trace",
            input.to_str().unwrap(),
            "--output",
            output.to_str().unwrap(),
            "--compress",
            "2",
            "--limit",
            "3s",
        ])
        .spawn()
        .unwrap();
    wait_running(&bravo, "rehearsal:build fixture");
    let run = alpha.ok(&["runs"])["runs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| {
            r["label"]
                .as_str()
                .is_some_and(|s| s.starts_with("rehearsal:"))
        })
        .unwrap()
        .clone();
    let session = run["label"]
        .as_str()
        .unwrap()
        .strip_prefix("rehearsal:")
        .unwrap()
        .split(':')
        .next()
        .unwrap()
        .to_owned();
    replay.kill().unwrap();
    replay.wait().unwrap();
    let end = Instant::now() + processes::WAIT;
    while alpha.data().join("rehearsal-session.json").exists()
        || bravo.data().join("rehearsal-session.json").exists()
    {
        assert!(Instant::now() < end);
        sleep(Duration::from_millis(50));
    }
    assert_clean(&alpha, &bravo, &session);
}

#[test]
fn service_restart_cleans_workers_even_after_their_caller_was_killed() {
    let (mut alpha, mut bravo) = fleet();
    let temp = tempfile::tempdir().unwrap();
    let input = fixture(temp.path(), &[0]);
    let mut source: Value = serde_json::from_slice(&fs::read(&input).unwrap()).unwrap();
    source["requests"][0]["run_ms"] = json!(60000);
    fs::write(&input, source.to_string()).unwrap();
    let output = temp.path().join("observed.json");
    let mut replay = processes::command(env!("CARGO_BIN_EXE_elsecall"))
        .env("HOME", alpha.home.path())
        .args([
            "rehearse",
            "--trace",
            input.to_str().unwrap(),
            "--output",
            output.to_str().unwrap(),
            "--limit",
            "1m",
        ])
        .spawn()
        .unwrap();
    wait_running(&alpha, "rehearsal:build fixture");
    let journal: Value =
        serde_json::from_slice(&fs::read(alpha.data().join("rehearsal-session.json")).unwrap())
            .unwrap();
    let session = journal["session"].as_str().unwrap().to_owned();
    let records = elsecall::scheduling::records::Records::open_read_only(
        &alpha.data().join("records.sqlite"),
    )
    .unwrap();
    let mut workers = Vec::new();
    let until = Instant::now() + processes::WAIT;
    while workers.is_empty() {
        for record in records.unfinished().unwrap() {
            if record.synthetic {
                workers.extend(
                    record
                        .processes
                        .into_iter()
                        .filter(|p| *p != record.requested_by),
                );
            }
        }
        assert!(Instant::now() < until);
        sleep(Duration::from_millis(20));
    }
    replay.kill().unwrap();
    replay.wait().unwrap();
    alpha.stop();
    for caller in journal["callers"].as_array().unwrap() {
        unsafe {
            libc::kill(caller["pid"].as_u64().unwrap() as i32, libc::SIGKILL);
        }
    }
    alpha.restart();
    bravo.restart();
    let until = Instant::now() + processes::WAIT;
    while alpha.data().join("rehearsal-session.json").exists()
        || bravo.data().join("rehearsal-session.json").exists()
    {
        assert!(Instant::now() < until, "{}", alpha.log());
        sleep(Duration::from_millis(20));
    }
    for worker in workers {
        assert!(
            !elsecall::macos::processes::describe(worker.pid).is_some_and(|p| p.identity == worker),
            "surviving synthetic worker {worker:?}"
        );
    }
    assert_clean(&alpha, &bravo, &session);
}

#[test]
fn a_same_time_burst_retains_source_arrival_timing_in_the_export() {
    let (alpha, bravo) = fleet();
    let temp = tempfile::tempdir().unwrap();
    let input = fixture(temp.path(), &[0; 8]);
    let output = temp.path().join("observed.json");
    let result = rehearse(&alpha, &input, &output, &[]);
    assert!(result.status.success(), "{result:?}");
    let result = members::json(&result)["data"].clone();
    assert_eq!(result["submitted"], 8, "{result}");
    for request in result["requests"].as_array().unwrap() {
        assert_eq!(request["arrival_offset_ms"], 0);
        assert!(
            (0..=1000).contains(&request["arrival_error_ms"].as_i64().unwrap()),
            "{request}"
        );
    }
    assert_clean(&alpha, &bravo, result["session"].as_str().unwrap());
}
#[test]
fn queued_production_alone_refuses_startup_while_synthetic_work_is_ignored() {
    let (alpha, bravo) = fleet();
    let capacity = bravo.ok(&["runs"])["member"]["capacity"]["capacity"]
        .as_f64()
        .unwrap()
        .to_string();
    let mut occupied = processes::command(env!("CARGO_BIN_EXE_elsecall"))
        .env("HOME", bravo.home.path())
        .args([
            "run",
            "--pinned",
            "--label",
            "rehearsal:occupier",
            "--size",
            &capacity,
            "--",
            "/bin/sleep",
            "30",
        ])
        .spawn()
        .unwrap();
    wait_running(&bravo, "rehearsal:occupier");
    let mut queued = production(&bravo, "0.1");
    let until = Instant::now() + processes::WAIT;
    loop {
        if bravo.ok(&["runs"])["runs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["label"] == "production build" && r["state"] == "queued")
        {
            break;
        }
        assert!(Instant::now() < until);
        sleep(Duration::from_millis(20));
    }
    let temp = tempfile::tempdir().unwrap();
    let input = fixture(temp.path(), &[0]);
    let output = temp.path().join("observed.json");
    let result = rehearse(&alpha, &input, &output, &[]);
    assert!(!result.status.success());
    assert_eq!(
        members::json(&result)["error"]["code"],
        "rehearsal_busy",
        "{result:?}"
    );
    // TERM the caller: its normal forwarding stops sleep and releases capacity.
    unsafe {
        libc::kill(occupied.id() as i32, libc::SIGTERM);
    }
    occupied.wait().unwrap();
    queued.wait().unwrap();
}

#[test]
fn live_cleanup_stops_a_worker_after_only_its_caller_is_killed() {
    let (alpha, bravo) = fleet();
    let temp = tempfile::tempdir().unwrap();
    let input = fixture(temp.path(), &[0]);
    let output = temp.path().join("observed.json");
    let mut source: Value = serde_json::from_slice(&fs::read(&input).unwrap()).unwrap();
    source["requests"][0]["run_ms"] = json!(60000);
    fs::write(&input, source.to_string()).unwrap();
    let replay = processes::command(env!("CARGO_BIN_EXE_elsecall"))
        .env("HOME", alpha.home.path())
        .args([
            "rehearse",
            "--trace",
            input.to_str().unwrap(),
            "--output",
            output.to_str().unwrap(),
            "--limit",
            "1m",
        ])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    wait_running(&alpha, "rehearsal:build fixture");
    let records = elsecall::scheduling::records::Records::open_read_only(
        &alpha.data().join("records.sqlite"),
    )
    .unwrap();
    let until = Instant::now() + processes::WAIT;
    let record = loop {
        if let Some(r) = records
            .unfinished()
            .unwrap()
            .into_iter()
            .find(|r| r.synthetic && r.processes.iter().any(|p| *p != r.requested_by))
        {
            break r;
        }
        assert!(Instant::now() < until);
        sleep(Duration::from_millis(20));
    };
    unsafe {
        libc::kill(record.requested_by.pid as i32, libc::SIGKILL);
    }
    let result = replay.wait_with_output().unwrap();
    assert!(result.status.success(), "{result:?}");
    let result = members::json(&result)["data"].clone();
    for worker in record.processes {
        assert!(
            !elsecall::macos::processes::describe(worker.pid).is_some_and(|p| p.identity == worker)
        );
    }
    assert_clean(&alpha, &bravo, result["session"].as_str().unwrap());
}

#[test]
fn completed_rehearsal_certifies_tune_and_build_changes_expire_it() {
    let (alpha, bravo) = fleet();
    let temp = tempfile::tempdir().unwrap();
    let input = certification_fixture(temp.path());
    let output = temp.path().join("certified.json");
    let result = rehearse(&alpha, &input, &output, &[]);
    assert!(result.status.success(), "{result:?}");
    let result = members::json(&result)["data"].clone();
    assert!(
        !result["certificate"].is_null(),
        "{}",
        serde_json::to_string_pretty(&result).unwrap()
    );
    let tuned = alpha.ok(&["tune", "--trace", output.to_str().unwrap(), "--budget", "1"]);
    assert_eq!(tuned["trusted_for_tuning"], true, "{tuned}");
    for member in [&alpha, &bravo] {
        let db = rusqlite::Connection::open(member.data().join("tuning-evidence.sqlite")).unwrap();
        db.execute(
            "UPDATE certificate SET value=json_set(value,'$.build.build','0000000000000000')",
            [],
        )
        .unwrap();
    }
    let tuned = alpha.ok(&["tune", "--trace", output.to_str().unwrap(), "--budget", "1"]);
    assert_eq!(tuned["trusted_for_tuning"], false, "{tuned}");
    assert_eq!(
        tuned["certificate_evidence"]["members"][0]["evidence"]["status"],
        "expired"
    );
}

const MANAGED_SOURCE: &str = "# approved settings; preserve formatting\r\n[defaults.placement]\r\nkeep_local_margin = 0.10 # baseline";
fn managed(alpha: &TestMember, bravo: &TestMember) {
    fs::write(alpha.data().join("fleet-config.toml"), MANAGED_SOURCE).unwrap();
    let plan = alpha.ok(&["sync", "service"]);
    alpha.ok(&[
        "sync",
        "service",
        "--confirm",
        plan["plan"]["plan_id"].as_str().unwrap(),
    ]);
    let end = Instant::now() + processes::WAIT;
    loop {
        if [&alpha, &bravo]
            .iter()
            .all(|m| m.run(&["runs"]).status.success())
        {
            let status = alpha.run(&["fleet"]);
            if status.status.success()
                && members::json(&status)["data"]["members"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|m| m["status"]["config_sync"]["service"]["pending"] == false)
            {
                break;
            }
        }
        assert!(Instant::now() < end);
        sleep(Duration::from_millis(100));
    }
}
fn service_snapshots(alpha: &TestMember) -> std::collections::BTreeMap<String, Value> {
    let status = alpha.ok(&["sync", "status"]);
    let fleet = alpha.ok(&["fleet"]);
    status["members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|member| {
            let name = member["member"].as_str().unwrap();
            let service = &member["sync"]["service"];
            let peer = fleet["members"]
                .as_array()
                .unwrap()
                .iter()
                .find(|m| m["name"] == name)
                .unwrap();
            assert_eq!(
                service["effective_fingerprint"],
                peer["status"]["config_sync"]["service"]["effective_fingerprint"]
            );
            assert!(
                service["effective_fingerprint"]
                    .as_str()
                    .is_some_and(|f| f.len() == 16)
            );
            (name.to_owned(), service.clone())
        })
        .collect()
}
fn sealed_service(member: &TestMember) -> Vec<u8> {
    use elsecall::config_sync::{
        state::State,
        store::{FileStore, ReplicaStore},
    };
    let manifest = State::load(&member.data().join("config-sync/state.json"))
        .unwrap()
        .manifest
        .unwrap();
    FileStore::open(&member.data().join("config-sync/objects"))
        .unwrap()
        .read(&manifest.key, &manifest.service.unwrap())
        .unwrap()
        .to_vec()
}
fn assert_restored_snapshots(
    alpha: &TestMember,
    before: &std::collections::BTreeMap<String, Value>,
) {
    let after = service_snapshots(alpha);
    for (name, service) in before {
        assert_eq!(
            after[name]["fingerprint"], service["fingerprint"],
            "{after:?}"
        );
        assert_eq!(
            after[name]["effective_fingerprint"], service["effective_fingerprint"],
            "{after:?}"
        );
    }
}
fn latest_margin(member: &TestMember) -> Option<f64> {
    elsecall::scheduling::records::Records::open_read_only(&member.data().join("records.sqlite"))
        .unwrap()
        .scheduler_settings(
            Timestamp::from_millis(0),
            Timestamp::now().after(Duration::from_millis(1)),
        )
        .unwrap()
        .last()
        .map(|s| s.parameters.placement.keep_local_margin)
}
fn wait_margin(alpha: &TestMember, bravo: &TestMember, margin: f64) {
    let end = Instant::now() + processes::WAIT;
    while latest_margin(alpha) != Some(margin) || latest_margin(bravo) != Some(margin) {
        assert!(
            Instant::now() < end,
            "alpha={} bravo={}",
            alpha.log(),
            bravo.log()
        );
        sleep(Duration::from_millis(50));
    }
}
fn proposal_file(dir: &Path) -> std::path::PathBuf {
    let path = dir.join("proposal.json");
    fs::write(&path,json!({"data":{"best":{"settings":{"placement.keep_local_margin":"0.2","label.build fixture.size":"1.5"}}}}).to_string()).unwrap();
    path
}
#[test]
fn compare_reports_live_metrics_uses_synthetic_sizes_and_restores_synced_settings() {
    let (alpha, bravo) = fleet();
    managed(&alpha, &bravo);
    let before = service_snapshots(&alpha);
    let temp = tempfile::tempdir().unwrap();
    let input = fixture(temp.path(), &[0, 2000]);
    let proposal = proposal_file(temp.path());
    let output = temp.path().join("ab.json");
    let result = rehearse(
        &alpha,
        &input,
        &output,
        &["--compare", proposal.to_str().unwrap()],
    );
    assert!(result.status.success(), "{result:?}");
    let data = members::json(&result)["data"].clone();
    assert_eq!(data["settings_restored"], true, "{data}");
    assert_eq!(
        data["effective_fingerprints"]["before"],
        data["effective_fingerprints"]["after"]
    );
    for (name, service) in &before {
        assert_eq!(
            data["effective_fingerprints"]["before"][name],
            service["effective_fingerprint"]
        );
    }
    assert_eq!(data["comparison"]["status"], "compared", "{data}");
    assert!(
        data["comparison"]["metrics"]["baseline"]["objective"]["mean_charged_seconds"].is_number()
    );
    assert!(
        data["comparison"]["metrics"]["proposal"]["objective"]["mean_charged_seconds"].is_number()
    );
    // A missing noise floor prevents acceptance. A known hard violation may
    // still reject the proposal, including slowdown caused by host contention.
    let expected = elsecall::tuning_objective::compare(
        &data["baseline"]["metrics"]["scheduling_objective"],
        &data["proposal"]["metrics"]["scheduling_objective"],
        None,
    );
    assert_eq!(
        data["comparison"]["accepted"], expected["accepted"],
        "{data}"
    );
    assert_ne!(data["comparison"]["accepted"], true, "{data}");
    assert_eq!(data["comparison"]["required_mean_improvement"], Value::Null);
    let a: Value = serde_json::from_slice(&fs::read(&output).unwrap()).unwrap();
    let b: Value =
        serde_json::from_slice(&fs::read(data["proposal"]["trace"].as_str().unwrap()).unwrap())
            .unwrap();
    assert!(
        a["requests"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["size"] == 0.5)
    );
    assert!(
        b["requests"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["size"] == 1.5)
    );
    assert_eq!(
        a["rehearsal"]["source_digest"],
        b["rehearsal"]["source_digest"]
    );
    assert_eq!(a["rehearsal"]["seed"], b["rehearsal"]["seed"]);
    wait_margin(&alpha, &bravo, 0.1);
    assert_eq!(
        fs::read_to_string(alpha.data().join("fleet-config.toml")).unwrap(),
        MANAGED_SOURCE
    );
    for member in [&alpha, &bravo] {
        assert_eq!(sealed_service(member), MANAGED_SOURCE.as_bytes());
    }
    assert_restored_snapshots(&alpha, &before);
    let after = service_snapshots(&alpha);
    for (name, service) in &before {
        assert_eq!(
            after[name]["generation"].as_u64().unwrap(),
            service["generation"].as_u64().unwrap() + 2
        );
    }
}
#[test]
fn consumer_only_compare_keeps_generation_and_documents_unchanged() {
    let (alpha, bravo) = fleet();
    managed(&alpha, &bravo);
    let before = service_snapshots(&alpha);
    let temp = tempfile::tempdir().unwrap();
    let input = fixture(temp.path(), &[0, 2000]);
    let proposal = temp.path().join("consumer.json");
    fs::write(
        &proposal,
        json!({"label.build fixture.size":"1.5"}).to_string(),
    )
    .unwrap();
    let result = rehearse(
        &alpha,
        &input,
        &temp.path().join("consumer-ab.json"),
        &["--compare", proposal.to_str().unwrap()],
    );
    assert!(result.status.success(), "{result:?}");
    let data = members::json(&result)["data"].clone();
    assert_eq!(data["settings_restored"], true, "{data}");
    assert!(data["lease_expires_at"].is_null(), "{data}");
    assert_eq!(
        data["effective_fingerprints"]["before"],
        data["effective_fingerprints"]["after"]
    );
    assert_eq!(data["comparison"]["status"], "compared", "{data}");
    assert_eq!(service_snapshots(&alpha), before);
    for member in [&alpha, &bravo] {
        assert_eq!(sealed_service(member), MANAGED_SOURCE.as_bytes());
    }
    let trace: Value =
        serde_json::from_slice(&fs::read(data["proposal"]["trace"].as_str().unwrap()).unwrap())
            .unwrap();
    assert!(
        trace["requests"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["size"] == 1.5)
    );
}
#[test]
fn compare_restores_on_proposal_artifact_failure() {
    let (alpha, bravo) = fleet();
    managed(&alpha, &bravo);
    let temp = tempfile::tempdir().unwrap();
    let input = fixture(temp.path(), &[0, 4000]);
    let proposal = proposal_file(temp.path());
    let output = temp.path().join("failure.json");
    fs::write(temp.path().join("failure.json.proposal.json"), "occupied").unwrap();
    let result = rehearse(
        &alpha,
        &input,
        &output,
        &["--compare", proposal.to_str().unwrap()],
    );
    assert!(!result.status.success(), "{result:?}");
    let data = members::json(&result)["data"].clone();
    assert_eq!(data["settings_restored"], true, "{data}");
    assert!(!data["rehearsal_error"].is_null(), "{data}");
    wait_margin(&alpha, &bravo, 0.1);
}
#[test]
fn compare_restores_after_forced_controller_loss_without_contacting_installed_services() {
    use std::os::unix::process::ExitStatusExt;

    // A failed observation must not leave a running or stopped controller behind.
    struct Controller(Child);
    impl Drop for Controller {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    let (alpha, bravo) = fleet();
    managed(&alpha, &bravo);
    let before = service_snapshots(&alpha);
    let temp = tempfile::tempdir().unwrap();
    let input = fixture(temp.path(), &[0, 2000]);
    let proposal = proposal_file(temp.path());
    let output = temp.path().join("killed.json");
    let diagnostics = temp.path().join("controller.log");
    let log = fs::File::create(&diagnostics).unwrap();
    let mut child = Controller(
        processes::command(env!("CARGO_BIN_EXE_elsecall"))
            .env("HOME", alpha.home.path())
            .current_dir(alpha.home.path())
            .args([
                "rehearse",
                "--trace",
                input.to_str().unwrap(),
                "--output",
                output.to_str().unwrap(),
                "--compare",
                proposal.to_str().unwrap(),
                "--compress",
                "2",
                "--limit",
                // This bounds baseline preparation too, not just the trial.
                // Its 55s settings lease fits within the 120s restoration wait.
                "30s",
            ])
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap(),
    );
    let end = Instant::now() + processes::WAIT;
    loop {
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "controller exited before proposal activation: {}",
            fs::read_to_string(&diagnostics).unwrap()
        );
        // Stop at the first member's activation; the services continue syncing
        // independently while the controller cannot finish B and restore normally.
        if latest_margin(&alpha) == Some(0.2) || latest_margin(&bravo) == Some(0.2) {
            break;
        }
        assert!(
            Instant::now() < end,
            "proposal did not activate: controller={} alpha={} bravo={}",
            fs::read_to_string(&diagnostics).unwrap(),
            alpha.log(),
            bravo.log()
        );
        sleep(Duration::from_millis(20));
    }
    assert_eq!(unsafe { libc::kill(child.0.id() as i32, libc::SIGSTOP) }, 0);
    loop {
        // Observe without reaping: the Child handle must retain ownership even
        // if the controller exited before SIGSTOP, so cleanup cannot use a stale PID.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                child.0.id() as libc::id_t,
                &mut info,
                libc::WSTOPPED | libc::WEXITED | libc::WNOWAIT | libc::WNOHANG,
            )
        };
        assert_eq!(result, 0, "{}", std::io::Error::last_os_error());
        if info.si_pid != 0 {
            assert_eq!(info.si_pid, child.0.id() as i32);
            assert_eq!(
                info.si_code,
                libc::CLD_STOPPED,
                "controller exited before suspension: status={} output={}",
                info.si_status,
                fs::read_to_string(&diagnostics).unwrap()
            );
            assert_eq!(info.si_status, libc::SIGSTOP);
            break;
        }
        assert!(Instant::now() < end, "controller did not stop");
        sleep(Duration::from_millis(20));
    }
    wait_margin(&alpha, &bravo, 0.2);
    child.0.kill().unwrap();
    assert_eq!(child.0.wait().unwrap().signal(), Some(libc::SIGKILL));
    wait_margin(&alpha, &bravo, 0.1);
    assert_eq!(
        fs::read_to_string(alpha.data().join("fleet-config.toml")).unwrap(),
        MANAGED_SOURCE
    );
    let after = service_snapshots(&alpha);
    for (name, service) in &before {
        assert_eq!(
            after[name]["effective_fingerprint"],
            service["effective_fingerprint"]
        );
        assert_eq!(after[name]["pending"], false);
    }
}

fn seed_recorded_day(
    member: &TestMember,
    template: &elsecall::scheduling::record::Record,
    day: Timestamp,
    complete: bool,
) {
    use elsecall::{
        load_log::{minute::Minute, store::LoadStore},
        scheduling::{
            capacity::Cores,
            records::{Records, SchedulerSettings},
        },
    };
    let records = Records::open(&member.data().join("records.sqlite")).unwrap();
    let current = records
        .scheduler_settings(
            Timestamp::from_millis(0),
            Timestamp::now().after(Duration::from_secs(1)),
        )
        .unwrap()
        .last()
        .unwrap()
        .clone();
    records
        .save_scheduler_settings(&SchedulerSettings {
            at: Timestamp::from_millis(day.millis() - 14 * 86_400_000 - 1),
            ..current.clone()
        })
        .unwrap();
    let load = LoadStore::open(&member.data().join("load.sqlite")).unwrap();
    for minute in 0..if complete { 1440 } else { 1 } {
        let active = (10..34).contains(&minute) && minute % 3 == 1;
        let cores = current.capacity.weighted_cores.as_f64();
        let row:Minute=serde_json::from_value(json!({"at":day.after(Duration::from_secs(minute*60)),"seconds":60,"cpu_mean":(0.12+if active{0.3}else{0.0})/cores,"cpu_max":(0.12+if active{0.6}else{0.0})/cores,"outside_mean":0.12/cores,"weighted_cores":current.capacity.weighted_cores,"capacity":current.capacity.capacity,"in_use_mean":if active{0.25}else{0.0},"in_use_max":if active{0.5}else{0.0},"queued_max":0,"interactive_queued_max":0,"busy_seconds":if active{30}else{0},"saturated_seconds":0,"saturated_busy_seconds":0,"memory_pressure_seconds":0,"worst_memory_pressure":"normal","worst_thermal_state":"nominal","speed":1})).unwrap();
        load.append(&row).unwrap();
        if active {
            let mut record = template.clone();
            record.run_id = elsecall::scheduling::run_id::RunId::parse(&format!(
                "r-{:012x}",
                day.millis() / 1000 + minute
            ))
            .unwrap();
            record.calling_member = member.name.into();
            record.member = Some(member.name.into());
            record.label = Some("drift fixture".into());
            record.size = Cores::from_millis(500);
            record.requested_at = day.after(Duration::from_secs(minute * 60));
            record.started_at = Some(record.requested_at.after(Duration::from_secs(10)));
            record.ended_at = Some(record.requested_at.after(Duration::from_secs(40)));
            record.waited_ms = Some(10000);
            record.run_ms = Some(30000);
            record.cpu_ms = Some(18000);
            record.decision = None;
            record.hold_limit.as_mut().unwrap().seconds = Duration::from_secs(120);
            records.save(&record).unwrap();
        }
    }
}
#[test]
fn automatically_monitored_failing_day_revokes_trust_and_incomplete_day_is_not_checkable() {
    let (mut alpha, mut bravo) = fleet();
    let temp = tempfile::tempdir().unwrap();
    let input = certification_fixture(temp.path());
    let output = temp.path().join("cert.json");
    let result = rehearse(&alpha, &input, &output, &[]);
    assert!(result.status.success(), "{result:?}");
    assert!(
        !members::json(&result)["data"]["certificate"].is_null(),
        "{result:?}"
    );
    assert!(
        alpha
            .run(&[
                "run",
                "--pinned",
                "--size",
                "0.5",
                "--label",
                "drift fixture",
                "--",
                "/usr/bin/true"
            ])
            .status
            .success()
    );
    let template = elsecall::scheduling::records::Records::open_read_only(
        &alpha.data().join("records.sqlite"),
    )
    .unwrap()
    .recent(10)
    .unwrap()
    .into_iter()
    .find(|r| r.label.as_deref() == Some("drift fixture"))
    .unwrap();
    let today = Timestamp::now().millis() / 86_400_000 * 86_400_000;
    alpha.stop();
    bravo.stop();
    for member in [&alpha, &bravo] {
        seed_recorded_day(
            member,
            &template,
            Timestamp::from_millis(today - 86_400_000),
            true,
        );
        seed_recorded_day(
            member,
            &template,
            Timestamp::from_millis(today - 2 * 86_400_000),
            false,
        );
        fs::write(
            member.data().join("config.toml"),
            "[housekeeping]\ninterval='1s'\n",
        )
        .unwrap();
    }
    alpha.restart();
    bravo.restart();
    let deadline = Instant::now() + processes::WAIT;
    let report = loop {
        let result = alpha.run(&["report", "--since", "3d"]);
        if result.status.success() {
            let data = members::json(&result)["data"].clone();
            if data["tuning_evidence"]["revoked"] == true
                && data["tuning_evidence"]["members"][0]["evidence"]["days"]
                    .as_array()
                    .is_some_and(|days| days.iter().any(|d| d["status"] == "not_checkable"))
            {
                break data;
            }
            assert!(
                Instant::now() < deadline,
                "{}; alpha={} bravo={}",
                serde_json::to_string_pretty(&data["tuning_evidence"]).unwrap(),
                alpha.log(),
                bravo.log()
            );
        } else {
            assert!(Instant::now() < deadline, "{result:?}");
        }
        sleep(Duration::from_millis(200));
    };
    assert!(
        report["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["kind"] == "simulator_drift" && f["severity"] == "high"),
        "{report}"
    );
    let days = &report["tuning_evidence"]["members"][0]["evidence"]["days"];
    assert!(
        days.as_array()
            .unwrap()
            .iter()
            .any(|d| d["status"] == "not_checkable"),
        "{days}"
    );
    let tuned = alpha.ok(&["tune", "--trace", output.to_str().unwrap(), "--budget", "1"]);
    assert_eq!(tuned["trusted_for_tuning"], false, "{tuned}");
    assert_eq!(tuned["certificate_evidence"]["revoked"], true);
}

fn spawn_comparison(
    alpha: &TestMember,
    input: &Path,
    output: &Path,
    proposal: &Path,
    limit: &str,
) -> Child {
    processes::command(env!("CARGO_BIN_EXE_elsecall"))
        .env("HOME", alpha.home.path())
        .current_dir(alpha.home.path())
        .args([
            "rehearse",
            "--trace",
            input.to_str().unwrap(),
            "--output",
            output.to_str().unwrap(),
            "--compare",
            proposal.to_str().unwrap(),
            "--compress",
            "2",
            "--limit",
            limit,
        ])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap()
}
#[test]
fn compare_restores_after_signal_and_new_production_work() {
    for signal in [true, false] {
        let (alpha, bravo) = fleet();
        managed(&alpha, &bravo);
        let temp = tempfile::tempdir().unwrap();
        let input = fixture(temp.path(), &[0, 3000, 6000]);
        let output = temp.path().join("interrupted.json");
        let proposal = proposal_file(temp.path());
        let child = spawn_comparison(&alpha, &input, &output, &proposal, "10s");
        wait_margin(&alpha, &bravo, 0.2);
        wait_running(&alpha, "rehearsal:build fixture");
        let production = if signal {
            assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGTERM) }, 0);
            None
        } else {
            Some(production(&bravo, "0.5"))
        };
        let result = child.wait_with_output().unwrap();
        let data = members::json(&result)["data"].clone();
        assert_eq!(data["settings_restored"], true, "{data}");
        assert_eq!(
            data["proposal"]["stop_reason"],
            if signal {
                "terminated"
            } else {
                "production_arrived"
            },
            "{data}"
        );
        assert!(data["proposal"]["certificate"].is_null());
        assert_eq!(data["comparison"]["wins_within_noise"], Value::Null);
        if let Some(mut production) = production {
            production.wait().unwrap();
        }
        wait_margin(&alpha, &bravo, 0.1);
    }
}
#[test]
fn compare_restores_after_second_run_hits_its_bound() {
    let (alpha, bravo) = fleet();
    managed(&alpha, &bravo);
    let temp = tempfile::tempdir().unwrap();
    let input = fixture(temp.path(), &[0, 100]);
    let output = temp.path().join("bounded.json");
    let mut source: Value = serde_json::from_slice(&fs::read(&input).unwrap()).unwrap();
    for request in source["requests"].as_array_mut().unwrap() {
        request["hold_limit"]["seconds"] = json!(120.0);
        request["urgency"]["interactive"]["hold_limit_ms"] = json!(120_000);
    }
    fs::write(&input, source.to_string()).unwrap();
    let proposal = proposal_file(temp.path());
    // Fixture observations are a constant 5% CPU. A's gate is open; B's
    // proposal closes it, keeping work queued until the arm's own limit. This
    // replaces the race between two-second work and a four-second setup+run cap.
    fs::write(
        &proposal,
        json!({"admission.saturated_utilization":"0.01","placement.keep_local_margin":"0.2"})
            .to_string(),
    )
    .unwrap();
    let result = rehearse(
        &alpha,
        &input,
        &output,
        &["--compare", proposal.to_str().unwrap(), "--limit", "30s"],
    );
    assert!(result.status.success(), "{result:?}");
    let data = members::json(&result)["data"].clone();
    assert_eq!(data["baseline"]["stop_reason"], "completed", "{data}");
    assert_eq!(data["baseline"]["submitted"], 2, "{data}");
    assert_eq!(data["baseline"]["recording_complete"], true, "{data}");
    assert_eq!(data["proposal"]["stop_reason"], "limit", "{data}");
    assert!(
        data["proposal"]["elapsed_ms"].as_u64().unwrap() >= 30_000,
        "{data}"
    );
    assert!(
        data["proposal"]["metrics"]["members"]["alpha"]["interactive_queue_peak"]
            .as_u64()
            .unwrap()
            > 0,
        "{data}"
    );
    assert_eq!(data["proposal"]["submitted"], 2, "{data}");
    let observed: Value =
        serde_json::from_slice(&fs::read(data["proposal"]["trace"].as_str().unwrap()).unwrap())
            .unwrap();
    let requests = observed["requests"].as_array().unwrap();
    assert_eq!(requests.len(), 2, "{observed}");
    assert!(
        requests
            .iter()
            .all(|r| r["resources"]["started_at"].is_null() && r["unfinished"] == false),
        "{observed}"
    );
    assert_eq!(data["settings_restored"], true, "{data}");
    assert!(data["proposal"]["certificate"].is_null());
    assert_eq!(data["comparison"]["status"], "incomplete");
    assert!(data["comparison"]["accepted"].is_null());
    wait_margin(&alpha, &bravo, 0.1);
    assert_clean(
        &alpha,
        &bravo,
        data["proposal"]["session"].as_str().unwrap(),
    );
}

#[test]
fn compare_restores_and_exits_unsuccessfully_after_member_io_failure() {
    let (alpha, mut bravo) = fleet();
    managed(&alpha, &bravo);
    let temp = tempfile::tempdir().unwrap();
    let input = fixture(temp.path(), &[0, 4000, 6000]);
    let output = temp.path().join("io-failure.json");
    let proposal = proposal_file(temp.path());
    let child = spawn_comparison(&alpha, &input, &output, &proposal, "20s");
    wait_margin(&alpha, &bravo, 0.2);
    wait_running(&alpha, "rehearsal:build fixture");
    bravo.stop();
    sleep(Duration::from_secs(1));
    bravo.restart();
    let result = child.wait_with_output().unwrap();
    let data = members::json(&result)["data"].clone();
    assert!(!result.status.success(), "{data}");
    assert_eq!(data["settings_restored"], true, "{data}");
    assert!(!data["rehearsal_error"].is_null(), "{data}");
    assert_eq!(data["comparison"]["wins_within_noise"], Value::Null);
    wait_margin(&alpha, &bravo, 0.1);
}

#[test]
fn aa_noise_file_freezes_work_and_compare_applies_the_overhead_acceptance_rule() {
    let (alpha, bravo) = fleet();
    managed(&alpha, &bravo);
    let temp = tempfile::tempdir().unwrap();
    let input = fixture(temp.path(), &[0, 3000]);
    let noise_path = temp.path().join("noise.json");
    let aa = rehearse(
        &alpha,
        &input,
        &temp.path().join("aa.json"),
        &["--aa", "--noise-output", noise_path.to_str().unwrap()],
    );
    assert!(aa.status.success(), "{aa:?}");
    let aa = members::json(&aa)["data"].clone();
    assert_eq!(aa["comparison"]["status"], "compared", "{aa}");
    let noise = elsecall::tuning_objective::Noise::read(&noise_path).unwrap();
    assert!(noise.valid());
    assert_eq!(serde_json::to_value(&noise).unwrap(), aa["noise_floor"]);
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        fs::metadata(&noise_path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let proposal = proposal_file(temp.path());
    let ab = rehearse(
        &alpha,
        &input,
        &temp.path().join("ab.json"),
        &[
            "--compare",
            proposal.to_str().unwrap(),
            "--noise",
            noise_path.to_str().unwrap(),
        ],
    );
    assert!(ab.status.success(), "{ab:?}");
    let ab = members::json(&ab)["data"].clone();
    assert_eq!(ab["comparison"]["status"], "compared", "{ab}");
    assert_eq!(ab["baseline"]["calibration"], aa["baseline"]["calibration"]);
    for i in 0..2 {
        assert_eq!(
            ab["baseline"]["requests"][i]["fixed_work"],
            ab["proposal"]["requests"][i]["fixed_work"]
        );
        assert_eq!(
            ab["baseline"]["requests"][i]["ideal_ms"],
            aa["baseline"]["requests"][i]["ideal_ms"]
        );
    }
    let a = &ab["baseline"]["metrics"]["scheduling_objective"];
    let b = &ab["proposal"]["metrics"]["scheduling_objective"];
    let expected = elsecall::tuning_objective::compare(a, b, Some(&noise));
    assert_eq!(ab["comparison"]["accepted"], expected["accepted"]);
    assert_eq!(ab["comparison"]["deltas"], expected["deltas"]);
    assert!(ab["comparison"]["accepted"].is_boolean(), "{ab}");
    let rejected = rehearse(
        &alpha,
        &input,
        &temp.path().join("wrong-seed.json"),
        &[
            "--compare",
            proposal.to_str().unwrap(),
            "--noise",
            noise_path.to_str().unwrap(),
            "--seed",
            "99",
        ],
    );
    assert!(!rejected.status.success());
    assert!(!temp.path().join("wrong-seed.json").exists());
    let rejected = rehearse(
        &alpha,
        &input,
        &temp.path().join("wrong-load.json"),
        &[
            "--compare",
            proposal.to_str().unwrap(),
            "--noise",
            noise_path.to_str().unwrap(),
            "--load-factor",
            "2",
        ],
    );
    assert!(!rejected.status.success());
    assert!(!temp.path().join("wrong-load.json").exists());
    assert_clean(&alpha, &bravo, ab["proposal"]["session"].as_str().unwrap());
}

#[test]
fn cpu_contention_in_a_real_rehearsal_reports_overhead_slowdown_and_zero_admission_violation() {
    let (alpha, bravo) = fleet();
    let temp = tempfile::tempdir().unwrap();
    let input = fixture(temp.path(), &[0]);
    let mut trace: Value = serde_json::from_slice(&fs::read(&input).unwrap()).unwrap();
    let at =
        Timestamp::parse_rfc3339(trace["requests"][0]["requested_at"].as_str().unwrap()).unwrap();
    trace["requests"][0]["run_ms"] = json!(2400);
    trace["requests"][0]["resources"]["ended_at"] = json!(at.after(Duration::from_millis(2400)));
    fs::write(&input, trace.to_string()).unwrap();
    let output = temp.path().join("contention.json");
    let replay = processes::command(env!("CARGO_BIN_EXE_elsecall"))
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
            "--limit",
            "20s",
        ])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let records = elsecall::scheduling::records::Records::open_read_only(
        &alpha.data().join("records.sqlite"),
    )
    .unwrap();
    let deadline = Instant::now() + processes::WAIT;
    let command = loop {
        let found = records
            .unfinished()
            .unwrap()
            .into_iter()
            .filter(|r| r.synthetic)
            .find_map(|r| {
                r.processes.iter().copied().find(|p| {
                    p.pid != r.requested_by.pid
                        && elsecall::macos::processes::describe(p.pid)
                            .is_some_and(|p| p.parent == r.requested_by.pid)
                })
            });
        if let Some(command) = found {
            break command;
        }
        assert!(Instant::now() < deadline, "{}", alpha.log());
        sleep(Duration::from_millis(10));
    };
    // Model a known loss of CPU service by descheduling only the private worker
    // group. This is reproducible on hosts with any number of idle cores.
    let group = unsafe { libc::getpgid(command.pid as i32) };
    assert!(group > 1 && group != unsafe { libc::getpgrp() });
    struct Resume(i32);
    impl Drop for Resume {
        fn drop(&mut self) {
            unsafe {
                libc::kill(-self.0, libc::SIGCONT);
            }
        }
    }
    assert_eq!(unsafe { libc::kill(-group, libc::SIGSTOP) }, 0);
    let resume = Resume(group);
    sleep(Duration::from_millis(1500));
    drop(resume);
    let result = replay.wait_with_output().unwrap();
    assert!(result.status.success(), "{result:?}");
    let data = members::json(&result)["data"].clone();
    let m = &data["metrics"]["scheduling_objective"];
    assert_eq!(m["reserve_violation_seconds"], 0.0, "{data}");
    assert_eq!(m["fleet_busy"]["rate"], 0.0);
    assert_eq!(m["failures"]["count"], 0);
    let h = m["overhead"]["mean"].as_f64().unwrap();
    let slowdown = m["contention_slowdown"]["distribution"]["p90"]
        .as_f64()
        .unwrap();
    // A fractional worker may absorb part of the CPU loss in its normal
    // idle allowance; the 1.5 s stall still exceeds its 1.2 s ideal.
    assert!(h > 0.25 && slowdown > 1.25, "{data}");
    let r = &m["requests"][0];
    let exact = r["time_to_result_ms"].as_f64().unwrap() / r["ideal_ms"].as_f64().unwrap() - 1.0;
    assert!((h - exact).abs() < 1e-12);
    assert_eq!(m["overhead"]["p90"], m["overhead"]["mean"]);
    assert_eq!(m["hard_limits"]["contention_slowdown"], false);
    assert_clean(&alpha, &bravo, data["session"].as_str().unwrap());
}

#[test]
fn explicit_rehearsal_bounds_replay_only_the_selected_arrivals() {
    let (alpha, bravo) = fleet();
    let temp = tempfile::tempdir().unwrap();
    let input = fixture(temp.path(), &[0, 5000]);
    let trace: Trace = serde_json::from_slice(&fs::read(&input).unwrap()).unwrap();
    let until = trace.since.after(Duration::from_secs(2));
    let output = temp.path().join("bounded.json");
    let response = rehearse(
        &alpha,
        &input,
        &output,
        &[
            "--since",
            &trace.since.to_string(),
            "--until",
            &until.to_string(),
        ],
    );
    assert!(response.status.success(), "{response:?}");
    let result: Value = serde_json::from_slice(&response.stdout).unwrap();
    let data = &result["data"];
    assert_eq!(data["planned"], 1, "{result}");
    assert_eq!(data["submitted"], 1, "{result}");
    assert_eq!(data["requests"][0]["original_run_id"], "r-000000000001");
    assert_clean(&alpha, &bravo, data["session"].as_str().unwrap());
}

#[test]
fn internal_drift_and_housekeeping_defer_until_rehearsal_cleanup() {
    let configuration =
        "[housekeeping]\ninterval = \"1s\"\nmin_free_percent = 0\nmin_free = \"0\"\n";
    let alpha = TestMember::start_with("alpha", configuration, &[]);
    let bravo = TestMember::start_with("bravo", configuration, &[]);
    enrol(&alpha, &bravo);
    let temp = tempfile::tempdir().unwrap();
    let input = fixture(temp.path(), &[0, 8_000]);
    let output = rehearse(&alpha, &input, &temp.path().join("recorded.json"), &[]);
    assert!(output.status.success(), "{output:?}");
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["data"]["stop_reason"], "completed", "{result}");
    assert_eq!(result["data"]["cleanup_complete"], true);
    for member in [&alpha, &bravo] {
        let until = Instant::now() + processes::WAIT;
        loop {
            let runs = member.ok(&["runs", "--limit", "500"]);
            let found = ["elsecall housekeeping", "elsecall fidelity drift"]
                .iter()
                .all(|label| {
                    runs["runs"].as_array().unwrap().iter().any(|r| {
                        if r["label"] != *label || r["state"] != "ended" {
                            return false;
                        }
                        let record = member.ok(&["explain", r["run_id"].as_str().unwrap()]);
                        record["internal"] == true
                            && record["warnings"].as_array().unwrap().iter().any(|w| {
                                w.as_str()
                                    .is_some_and(|w| w.contains("Internal deadline extended"))
                            })
                    })
                });
            if found {
                break;
            }
            assert!(
                Instant::now() < until,
                "deferred work did not finish: {runs}"
            );
            sleep(Duration::from_millis(50));
        }
    }
}

#[test]
fn current_acme_rules_make_replayed_pinned_work_eligible_on_both_members() {
    let (alpha, bravo) = fleet();
    let temp = tempfile::tempdir().unwrap();
    let input = fixture(temp.path(), &[0]);
    let mut trace: Value = serde_json::from_slice(&fs::read(&input).unwrap()).unwrap();
    trace["requests"][0]["label"] = json!("acme test Checkout");
    fs::write(&input, trace.to_string()).unwrap();
    let rules = temp.path().join("rules.toml");
    let bytes = include_bytes!("fixtures/acme.elsecall.toml");
    fs::write(&rules, bytes).unwrap();
    let output = rehearse(
        &alpha,
        &input,
        &temp.path().join("current.json"),
        &["--placement", "current", "--rules", rules.to_str().unwrap()],
    );
    assert!(output.status.success(), "{output:?}");
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    let request = &result["data"]["requests"][0];
    assert!(
        request["target_effective_cores"]["alpha"].is_number(),
        "{result}"
    );
    assert!(
        request["target_effective_cores"]["bravo"].is_number(),
        "{result}"
    );
    assert_eq!(result["data"]["placement_replay"]["split"]["placeable"], 1);
    assert_eq!(fs::read(rules).unwrap(), bytes);
    assert_clean(&alpha, &bravo, result["data"]["session"].as_str().unwrap());
}
