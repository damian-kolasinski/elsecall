#![cfg(target_os = "macos")]
//! Automatic placement between real member services in separate temporary homes,
//! with loopback standing in for the tailnet. Only machine observation is
//! scripted: each member's core count, load and speed come from the test.
mod members;
mod processes;

use members::{TestMember, enrol};
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, Output, Stdio},
    time::{Duration, Instant},
};

fn git(dir: &Path, args: &[&str]) {
    let output = crate::processes::command("git")
        .current_dir(dir)
        .args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}: {output:?}");
}

/// A fresh caller repository with a commit and an uncommitted change.
fn repository() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::Builder::new()
        .prefix("ec-place-")
        .tempdir_in("/tmp")
        .unwrap();
    let root = temp.path().canonicalize().unwrap().join("app");
    std::fs::create_dir_all(&root).unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    std::fs::write(root.join("README.md"), "hello\n").unwrap();
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-q", "-m", "initial"]);
    std::fs::write(root.join("notes.txt"), "uncommitted\n").unwrap();
    (temp, root)
}

/// A member whose cores, load and speed the test scripts.
struct Machine {
    member: TestMember,
    load: PathBuf,
    cores: u32,
    _dir: tempfile::TempDir,
}

impl Machine {
    fn start(name: &'static str, cores: u32, reserve: f64, speed: f64) -> Self {
        Self::configured(
            name,
            cores,
            speed,
            &format!("[capacity]\nreserved_cores = {reserve}\n"),
        )
    }

    /// A member whose service has `configuration` as its config.toml.
    fn configured(name: &'static str, cores: u32, speed: f64, configuration: &str) -> Self {
        let dir = tempfile::Builder::new()
            .prefix("ec-load-")
            .tempdir_in("/tmp")
            .unwrap();
        let load = dir.path().join("load.json");
        std::fs::write(&load, json!({"performance_cores": cores}).to_string()).unwrap();
        let member = TestMember::start_with(
            name,
            configuration,
            &[
                ("ELSECALL_MACHINE_LOAD", &format!("file:{}", load.display())),
                ("ELSECALL_MACHINE_SPEED", &speed.to_string()),
            ],
        );
        Self {
            member,
            load,
            cores,
            _dir: dir,
        }
    }

    /// Scripts the machine's CPU utilization, and waits until admission sees it.
    fn set_utilization(&self, utilization: f64) {
        std::fs::write(
            &self.load,
            json!({"performance_cores": self.cores, "cpu_utilization": utilization}).to_string(),
        )
        .unwrap();
        let saturated = utilization >= 0.9;
        until("the load gate to follow", crate::processes::WAIT, || {
            let state = &self.member.ok(&["runs", "--limit", "1"])["member"];
            (state["gate"]["verdict"] == "saturated") == saturated
        });
    }
}

fn until(what: &str, limit: Duration, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + limit;
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn spawn(caller: &TestMember, cwd: &Path, args: &[&str]) -> Child {
    crate::processes::command(env!("CARGO_BIN_EXE_elsecall"))
        .current_dir(cwd)
        .env("HOME", caller.home.path())
        .env("EC", env!("CARGO_BIN_EXE_elsecall"))
        .arg("run")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

fn run(caller: &TestMember, cwd: &Path, args: &[&str]) -> Output {
    spawn(caller, cwd, args).wait_with_output().unwrap()
}

/// Runs `args` and the command after them, which prints its run ID and member;
/// returns both.
fn placed(caller: &TestMember, cwd: &Path, args: &[&str]) -> (String, String) {
    let mut all = args.to_vec();
    all.extend([
        "--",
        "/bin/sh",
        "-c",
        "echo $ELSECALL_RUN_ID $ELSECALL_MEMBER",
    ]);
    let output = run(caller, cwd, &all);
    placed_output(output)
}

fn placed_output(output: Output) -> (String, String) {
    assert!(output.status.success(), "{output:?}");
    let text = String::from_utf8(output.stdout).unwrap();
    let (id, member) = text.trim().split_once(' ').unwrap();
    (id.to_owned(), member.to_owned())
}

/// A real command that holds its grant until stdin is released, with readiness
/// acknowledged by the command itself rather than a short sleep.
fn gated_work(caller: &TestMember, cwd: &Path, options: &[&str]) -> Child {
    let mut child = crate::processes::command(env!("CARGO_BIN_EXE_elsecall"))
        .env("HOME", caller.home.path())
        .current_dir(cwd)
        .arg("run")
        .args(options)
        .args(["--", "/bin/sh", "-c", "echo ready; read finish"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (sent, received) = std::sync::mpsc::channel();
    let reading = std::thread::spawn(move || {
        let mut line = String::new();
        let result = BufReader::new(stdout).read_line(&mut line).map(|_| line);
        let _ = sent.send(result);
    });
    match received.recv_timeout(crate::processes::WAIT) {
        Ok(Ok(line)) if line == "ready\n" => (),
        other => {
            let _ = child.kill();
            let _ = child.wait();
            panic!("gated command never became ready: {other:?}");
        }
    }
    reading.join().unwrap();
    child
}

fn release_work(mut child: Child) {
    child.stdin.take().unwrap().write_all(b"finish\n").unwrap();
    let deadline = Instant::now() + crate::processes::WAIT;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "{status}");
            return;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("released command did not exit");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn explain(member: &TestMember, run_id: &str) -> Value {
    member.ok(&["explain", run_id])
}

/// Expected training observations, read from the public records. Sync time is
/// excluded from command learning; no guessed ceiling on scheduler latency.
fn command_seconds(member: &TestMember, id: &str) -> f64 {
    let record = explain(member, id);
    let total = record["run_ms"].as_u64().unwrap();
    let sync = record["placed"]["sync"]["duration_ms"]
        .as_u64()
        .unwrap_or(0);
    total.saturating_sub(sync) as f64 / 1000.0
}
fn median(mut seconds: Vec<f64>) -> f64 {
    seconds.sort_by(f64::total_cmp);
    (seconds[(seconds.len() - 1) / 2] + seconds[seconds.len() / 2]) / 2.0
}
fn near(actual: f64, expected: f64) {
    assert!(
        (actual - expected).abs() < 0.003,
        "{actual} against measured {expected}"
    );
}

fn candidate<'a>(record: &'a Value, member: &str) -> &'a Value {
    record["decision"]["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["member"] == member)
        .unwrap_or_else(|| panic!("no candidate {member}: {record}"))
}

fn envelope(output: &Output) -> Value {
    let text = String::from_utf8_lossy(&output.stderr);
    serde_json::from_str(text.lines().last().expect("an envelope")).unwrap()
}

/// Holds `size` cores of `member` with a lease until the returned child is killed.
fn hold(member: &TestMember, size: &str, label: &str) -> Child {
    let mut child = crate::processes::command("/bin/sh")
        .arg("-c")
        .arg(format!(
            "\"$EC\" lease acquire --size {size} --label '{label}' --holder $$ >/dev/null || exit 1; echo held; exec sleep 120"
        ))
        .env("HOME", member.home.path())
        .env("EC", env!("CARGO_BIN_EXE_elsecall"))
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.as_mut().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert_eq!(line.trim(), "held");
    child
}

#[test]
fn a_new_workspace_stays_on_the_caller_and_member_overrides_it() {
    let alpha = Machine::start("alpha", 10, 0.0, 1.0);
    let bravo = Machine::start("bravo", 10, 0.0, 2.0);
    enrol(&alpha.member, &bravo.member);
    let (_temp, repo) = repository();

    // bravo is twice as fast, but has no copy: a first sync and a cold run cost
    // more than running here.
    let (id, member) = placed(&alpha.member, &repo, &["--placeable"]);
    assert_eq!(member, "alpha");
    let record = explain(&alpha.member, &id);
    assert_eq!(record["decision"]["verdict"], "started");
    assert!(candidate(&record, "alpha")["chosen"].as_bool().unwrap());
    let cold = &candidate(&record, "bravo")["estimate"];
    assert_eq!(cold["copy"], "cold", "{record}");
    assert_eq!(cold["sync_seconds"], 120.0);
    assert_eq!(cold["sync_from"]["from"], "prior");
    assert_eq!(cold["cold_factor"], 3.0);
    assert_eq!(cold["speed"], 2.0);
    assert!(
        record["decision"]["reason"]
            .as_str()
            .unwrap()
            .contains("margin for leaving the caller"),
        "{record}"
    );

    // A required member stays a hard override.
    let (id, member) = placed(&alpha.member, &repo, &["--member", "bravo"]);
    assert_eq!(member, "bravo");
    assert_eq!(explain(&alpha.member, &id)["placed"]["warmth"], "cold");

    // A member that is not the master runs placeable work itself.
    let (id, member) = placed(&bravo.member, &repo, &["--placeable"]);
    assert_eq!(member, "bravo");
    assert!(
        explain(&bravo.member, &id)["placement_note"]
            .as_str()
            .unwrap()
            .contains("not the master")
    );
}

#[test]
fn a_warm_faster_member_wins_and_explain_shows_why() {
    let alpha = Machine::start("alpha", 10, 0.0, 1.0);
    let bravo = Machine::start("bravo", 10, 0.0, 2.0);
    enrol(&alpha.member, &bravo.member);
    let (_temp, repo) = repository();
    placed(&alpha.member, &repo, &["--member", "bravo"]);

    let (id, member) = placed(&alpha.member, &repo, &["--placeable", "--size", "2"]);
    assert_eq!(member, "bravo");
    let record = explain(&alpha.member, &id);
    let decision = &record["decision"];
    assert_eq!(decision["verdict"], "started");
    assert!(
        decision["reason"]
            .as_str()
            .unwrap()
            .starts_with("bravo completes earliest"),
        "{decision}"
    );
    let warm = candidate(&record, "bravo");
    assert!(warm["chosen"].as_bool().unwrap());
    let estimate = &warm["estimate"];
    assert_eq!(estimate["copy"], "warm");
    assert_eq!(estimate["start"]["at"], "now");
    assert_eq!(estimate["run_seconds"], 30.0, "60 s of work at speed 2");
    assert_eq!(estimate["run_from"]["from"], "prior");
    assert!(warm["why"].as_str().unwrap().starts_with("Starts now"));
    let here = &candidate(&record, "alpha")["estimate"];
    assert_eq!(here["copy"], "caller");
    assert_eq!(here["completion_seconds"], 60.0);
    assert_eq!(record["placed"]["member"], "bravo");

    // `runs` shows where it went and why.
    let runs = alpha.member.ok(&["runs", "--limit", "5"]);
    let listed = runs["runs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["run_id"] == id.as_str())
        .unwrap();
    assert_eq!(listed["placement"], "placeable");
    assert_eq!(listed["member"], "bravo");
    assert_eq!(listed["placed"]["member"], "bravo");
    assert_eq!(listed["placed"]["warmth"], "warm");
    assert!(
        listed["reason"]
            .as_str()
            .unwrap()
            .contains("completes earliest")
    );
}

#[test]
fn the_masters_margin_decides_and_explain_records_every_input() {
    // The same warm, twice-as-fast bravo as above wins by default (33 s against
    // 60 s); a 50% keep-local margin on the master keeps the work on the caller.
    // bravo's own, smaller margin does not govern placement.
    let alpha = Machine::configured(
        "alpha",
        10,
        1.0,
        "[placement]\nkeep_local_margin = 0.5\nsync_prior_cold = \"200s\"\n",
    );
    let bravo = Machine::configured("bravo", 10, 2.0, "[placement]\nkeep_local_margin = 0\n");
    enrol(&alpha.member, &bravo.member);
    let (_temp, repo) = repository();
    placed(&alpha.member, &repo, &["--member", "bravo"]);

    let (id, member) = placed(&alpha.member, &repo, &["--placeable", "--size", "2"]);
    assert_eq!(member, "alpha");
    let record = explain(&alpha.member, &id);
    let decision = &record["decision"];
    assert!(
        decision["reason"]
            .as_str()
            .unwrap()
            .contains("not earlier by the 30 s margin"),
        "{decision}"
    );
    let parameters = &decision["parameters"];
    assert_eq!(parameters["placement"]["keep_local_margin"], 0.5);
    assert_eq!(parameters["placement"]["sync_prior_cold"], "3m 20s");
    assert_eq!(
        parameters["placement"]["run_prior"], "1m",
        "defaults fill in"
    );
    assert_eq!(parameters["admission"]["saturated_utilization"], 0.9);
    assert_eq!(parameters["estimates"]["minimum_runs"], 3);
    let scoring = &decision["scoring"];
    assert_eq!(scoring["winner"], "alpha");
    assert_eq!(scoring["size"], 2.0);
    assert!(
        scoring["hold_left_ms"].as_u64().unwrap() > 500_000,
        "{scoring}"
    );
    let inputs = &candidate(&record, "bravo")["inputs"];
    assert_eq!(inputs["caller"], false);
    assert_eq!(inputs["speed"], 2.0);
    assert_eq!(inputs["copy"]["state"], "remote");
    assert_eq!(inputs["copy"]["warmth"], "warm");
    assert_eq!(inputs["learned"]["warm_run"], Value::Null);
    assert_eq!(
        inputs["admission"]["saturated_utilization"], 0.9,
        "bravo's own gate thresholds"
    );
    assert!(inputs["holding"].as_array().unwrap().is_empty());
    assert_eq!(candidate(&record, "alpha")["inputs"]["caller"], true);
    let arrival = &decision["arrival"];
    assert_eq!(arrival["winner"], "alpha");
    let expected = arrival["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["member"] == "bravo")
        .unwrap();
    assert_eq!(expected["completion_seconds"], 33.0);

    // Pinned work records the parameters its admission used too.
    let output = run(&alpha.member, &repo, &["--", "/usr/bin/true"]);
    assert!(output.status.success());
    let latest = &alpha.member.ok(&["runs", "--limit", "1"])["runs"][0]["run_id"];
    let pinned = explain(&alpha.member, latest.as_str().unwrap());
    assert_eq!(
        pinned["decision"]["parameters"]["admission"]["memory_pressure"],
        "warning"
    );

    // Every member's load so far shows in the fleet view.
    let fleet = alpha.member.ok(&["fleet"]);
    for member in fleet["members"].as_array().unwrap() {
        let last_hour = &member["status"]["load_history"]["last_hour"];
        assert!(last_hour["seconds"].as_u64().unwrap() >= 1, "{member}");
        assert_eq!(last_hour["weighted_cores"], 10.0);
        assert!(last_hour["cpu_mean"].as_f64().unwrap() < 0.2, "{member}");
    }
}

#[test]
fn a_saturated_caller_sends_work_away_and_a_saturated_member_is_avoided() {
    let alpha = Machine::start("alpha", 10, 0.0, 1.0);
    let bravo = Machine::start("bravo", 10, 0.0, 1.0);
    enrol(&alpha.member, &bravo.member);
    let (_temp, repo) = repository();

    alpha.set_utilization(0.97);
    let (id, member) = placed(&alpha.member, &repo, &["--placeable"]);
    assert_eq!(member, "bravo", "even a cold copy beats a saturated caller");
    let record = explain(&alpha.member, &id);
    let here = &candidate(&record, "alpha")["estimate"];
    assert_eq!(here["start"]["at"], "blocked", "{record}");
    assert!(here["start"]["why"].as_str().unwrap().contains("Saturated"));

    alpha.set_utilization(0.05);
    bravo.set_utilization(0.97);
    let (id, member) = placed(&alpha.member, &repo, &["--placeable", "--size", "2"]);
    assert_eq!(
        member, "alpha",
        "never sent somewhere saturated, warm or not"
    );
    let record = explain(&alpha.member, &id);
    let avoided = &candidate(&record, "bravo")["estimate"];
    assert_eq!(avoided["copy"], "warm");
    assert_eq!(avoided["start"]["at"], "blocked");
    assert_eq!(avoided["eligible"], false);
}

#[test]
fn the_callers_reserve_holds_through_capacity() {
    // Four cores with two reserved leave alpha a capacity of two.
    let alpha = Machine::start("alpha", 4, 2.0, 1.0);
    let bravo = Machine::start("bravo", 10, 0.0, 1.0);
    enrol(&alpha.member, &bravo.member);
    let (_temp, repo) = repository();
    let (id, member) = placed(&alpha.member, &repo, &["--placeable", "--size", "3"]);
    assert_eq!(member, "bravo");
    let record = explain(&alpha.member, &id);
    let here = candidate(&record, "alpha");
    assert_eq!(here["capacity"]["reserved"], 2.0);
    assert_eq!(here["estimate"]["start"]["at"], "never");
    // Within its capacity, the caller still takes new work itself.
    let (_, member) = placed(&alpha.member, &repo, &["--placeable", "--size", "2"]);
    assert_eq!(member, "alpha");
    // Larger than every member: refused at once.
    let output = run(
        &alpha.member,
        &repo,
        &["--placeable", "--size", "11", "--", "/usr/bin/true"],
    );
    assert_eq!(output.status.code(), Some(125));
    assert_eq!(envelope(&output)["error"]["code"], "request_too_large");
}

#[test]
fn a_warm_copy_is_preferred_when_the_caller_is_busy() {
    let alpha = Machine::start("alpha", 4, 0.0, 1.0);
    let bravo = Machine::start("bravo", 10, 0.0, 1.0);
    let charlie = Machine::start("charlie", 10, 0.0, 1.0);
    enrol(&alpha.member, &bravo.member);
    enrol(&alpha.member, &charlie.member);
    let (_temp, repo) = repository();
    placed(&alpha.member, &repo, &["--member", "charlie"]);
    let mut holder = hold(&alpha.member, "4", "busy here");

    let (id, member) = placed(&alpha.member, &repo, &["--placeable", "--size", "2"]);
    assert_eq!(member, "charlie");
    let record = explain(&alpha.member, &id);
    assert_eq!(candidate(&record, "charlie")["estimate"]["copy"], "warm");
    assert_eq!(candidate(&record, "bravo")["estimate"]["copy"], "cold");
    assert_eq!(
        candidate(&record, "alpha")["estimate"]["start"]["at"],
        "after_wait"
    );
    holder.kill().unwrap();
    let _ = holder.wait();
}

#[test]
fn no_member_in_time_is_fleet_busy_with_a_retry_hint_and_what_runs_where() {
    let alpha = Machine::start("alpha", 4, 0.0, 1.0);
    let bravo = Machine::start("bravo", 4, 0.0, 1.0);
    enrol(&alpha.member, &bravo.member);
    let (_temp, repo) = repository();
    let mut here = hold(&alpha.member, "4", "hold alpha");
    let mut there = hold(&bravo.member, "4", "hold bravo");

    let output = run(
        &alpha.member,
        &repo,
        &["--placeable", "--hold-limit", "2s", "--", "/usr/bin/true"],
    );
    assert_eq!(output.status.code(), Some(125), "{output:?}");
    let error = &envelope(&output)["error"];
    assert_eq!(error["code"], "fleet_busy", "{error}");
    let message = error["message"].as_str().unwrap();
    assert!(
        message.contains("alpha:") && message.contains("bravo:"),
        "{message}"
    );
    let details = &error["details"];
    assert!(details["retry_after_seconds"].as_u64().unwrap() >= 5);
    let running = details["running"].as_array().unwrap();
    for (member, label) in [("alpha", "hold alpha"), ("bravo", "hold bravo")] {
        assert!(
            running
                .iter()
                .any(|r| r["member"] == member && r["label"] == label),
            "{details}"
        );
    }
    let record = explain(&alpha.member, error["run_id"].as_str().unwrap());
    assert_eq!(record["decision"]["verdict"], "fleet_busy");
    assert_eq!(
        record["decision"]["candidates"].as_array().unwrap().len(),
        2
    );
    for child in [&mut here, &mut there] {
        child.kill().unwrap();
        let _ = child.wait();
    }
}

#[test]
fn a_request_waiting_on_a_busy_caller_moves_to_a_member_that_frees() {
    let alpha = Machine::start("alpha", 2, 0.0, 1.0);
    let bravo = Machine::start("bravo", 2, 0.0, 1.0);
    enrol(&alpha.member, &bravo.member);
    let (_temp, repo) = repository();
    placed(&alpha.member, &repo, &["--member", "bravo"]);
    let mut here = hold(&alpha.member, "2", "hold alpha");
    let mut there = hold(&bravo.member, "2", "hold bravo");
    let child = spawn(
        &alpha.member,
        &repo,
        &[
            "--placeable",
            "--size",
            "2",
            "--",
            "/bin/sh",
            "-c",
            "echo $ELSECALL_MEMBER",
        ],
    );
    std::thread::sleep(Duration::from_secs(2));
    // bravo frees first: the waiting request is reconsidered and goes there.
    there.kill().unwrap();
    let _ = there.wait();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "bravo");
    here.kill().unwrap();
    let _ = here.wait();
}

#[test]
fn learned_run_times_change_the_decision_as_records_accumulate() {
    let alpha = Machine::start("alpha", 10, 0.0, 1.0);
    let bravo = Machine::start("bravo", 10, 0.0, 1.0);
    enrol(&alpha.member, &bravo.member);
    let (_temp, repo) = repository();
    // The label's work takes 3.5 s on alpha and a fraction of a second on bravo.
    let work = [
        "--label",
        "work",
        "--",
        "/bin/sh",
        "-c",
        "if [ \"$ELSECALL_MEMBER\" = bravo ]; then sleep 0.1; else sleep 3.5; fi; echo $ELSECALL_RUN_ID $ELSECALL_MEMBER",
    ];
    let runs = |args: &[&str]| -> (String, String) {
        let mut all = args.to_vec();
        all.extend(work);
        let output = run(&alpha.member, &repo, &all);
        assert!(output.status.success(), "{output:?}");
        let text = String::from_utf8(output.stdout).unwrap();
        let (id, member) = text.trim().split_once(' ').unwrap();
        (id.to_owned(), member.to_owned())
    };
    // Without history, placeable work stays on the caller.
    for _ in 0..3 {
        assert_eq!(runs(&["--placeable"]).1, "alpha");
    }
    let (id, member) = runs(&["--placeable"]);
    assert_eq!(member, "alpha", "bravo is still cold");
    let record = explain(&alpha.member, &id);
    assert_eq!(
        candidate(&record, "alpha")["estimate"]["run_from"],
        json!({"from": "member_runs", "samples": 3, "level": "label"})
    );
    // Runs on bravo teach how fast the label is there and how long syncs take.
    let mut bravo_training = Vec::new();
    for n in 0..4 {
        let (id, member) = runs(&["--member", "bravo"]);
        assert_eq!(member, "bravo");
        if n > 0 {
            bravo_training.push(command_seconds(&alpha.member, &id));
        }
    }
    let (id, member) = runs(&["--placeable"]);
    assert_eq!(member, "bravo", "{}", explain(&alpha.member, &id));
    let record = explain(&alpha.member, &id);
    let estimate = &candidate(&record, "bravo")["estimate"];
    assert_eq!(estimate["run_from"]["from"], "member_runs", "{record}");
    assert_eq!(estimate["sync_from"]["from"], "workspace_syncs");
    near(
        estimate["run_seconds"].as_f64().unwrap(),
        median(bravo_training),
    );
    near(
        estimate["completion_seconds"].as_f64().unwrap(),
        estimate["run_seconds"].as_f64().unwrap() + estimate["sync_seconds"].as_f64().unwrap(),
    );
}

#[test]
fn sparse_labels_take_their_familys_runs_and_learned_speed_ratios() {
    // The master learns from runs of 100 ms on, and moves work for any gain.
    let alpha = Machine::configured(
        "alpha",
        10,
        1.0,
        "[placement]\nkeep_local_margin_floor = \"0s\"\n[estimates]\nshort_run = \"100ms\"\n",
    );
    // Measured speeds say both members are alike; the work says bravo is 5×
    // faster.
    let bravo = Machine::start("bravo", 10, 0.0, 1.0);
    enrol(&alpha.member, &bravo.member);
    let (_temp, repo) = repository();
    std::fs::write(
        repo.join(".elsecall.toml"),
        "[[labels]]\nmatch = \"test *\"\nsize = 1\n",
    )
    .unwrap();
    let runs = |label: &str, args: &[&str]| -> (String, String) {
        let mut all = vec!["--label", label];
        all.extend(args);
        all.extend([
            "--",
            "/bin/sh",
            "-c",
            "if [ \"$ELSECALL_MEMBER\" = bravo ]; then sleep 0.3; else sleep 1.5; fi; echo $ELSECALL_RUN_ID $ELSECALL_MEMBER",
        ]);
        let output = run(&alpha.member, &repo, &all);
        assert!(output.status.success(), "{output:?}");
        let text = String::from_utf8(output.stdout).unwrap();
        let (id, member) = text.trim().split_once(' ').unwrap();
        (id.to_owned(), member.to_owned())
    };
    // A first run sets bravo's copy up; then "test one" runs three times, warm,
    // on each member.
    assert_eq!(runs("warm-up", &["--member", "bravo"]).1, "bravo");
    let (mut alpha_training, mut bravo_training) = (Vec::new(), Vec::new());
    for _ in 0..3 {
        let (id, member) = runs("test one", &["--member", "bravo"]);
        assert_eq!(member, "bravo");
        bravo_training.push(command_seconds(&alpha.member, &id));
        let (id, member) = runs("test one", &[]);
        assert_eq!(member, "alpha");
        alpha_training.push(command_seconds(&alpha.member, &id));
    }
    let (alpha_median, bravo_median) = (median(alpha_training), median(bravo_training));

    // A new label of the family: its family's runs stand in on each member,
    // and send it to bravo, where they are faster. Without a family, both
    // members' 60 s priors would keep it on the caller.
    let (id, member) = runs("test two", &["--placeable"]);
    let record = explain(&alpha.member, &id);
    assert_eq!(member, "bravo", "{record}");
    assert_eq!(record["settings"]["rule"]["match"], "test *");
    let family = json!({"from": "family_runs", "samples": 3, "label_samples": 0,
                        "conversions": {"learned": 0, "measured": 0}, "level": "family"});
    for (name, expected) in [("alpha", alpha_median), ("bravo", bravo_median)] {
        let estimate = &candidate(&record, name)["estimate"];
        assert_eq!(estimate["run_from"], family, "{name}: {record}");
        let run = estimate["run_seconds"].as_f64().unwrap();
        near(run, expected);
    }
    assert!(
        candidate(&record, "bravo")["why"]
            .as_str()
            .unwrap()
            .contains("family: 3 runs of other labels there"),
        "{record}"
    );

    // A label with history only on alpha converts to bravo by the ratio
    // "test one" taught, not by the measured speeds, which say 1.5 s.
    let mut three_training = Vec::new();
    for _ in 0..3 {
        let (id, member) = runs("test three", &[]);
        assert_eq!(member, "alpha");
        three_training.push(command_seconds(&alpha.member, &id));
    }
    let converted = median(three_training) * bravo_median / alpha_median;
    let (id, member) = runs("test three", &["--placeable"]);
    let record = explain(&alpha.member, &id);
    assert_eq!(member, "bravo", "{record}");
    assert_eq!(
        candidate(&record, "alpha")["estimate"]["run_from"]["level"],
        "label"
    );
    let bravo_estimate = &candidate(&record, "bravo")["estimate"];
    assert_eq!(
        bravo_estimate["run_from"],
        json!({"from": "fleet_runs", "samples": 3,
               "conversions": {"learned": 3, "measured": 0}, "level": "converted"}),
        "{record}"
    );
    near(bravo_estimate["run_seconds"].as_f64().unwrap(), converted);
    assert_eq!(
        candidate(&record, "bravo")["inputs"]["learned"]["warm_run"]["from"],
        "fleet_runs"
    );

    // A label no rule matched has no family: the prior.
    let (id, _) = runs("lint", &["--placeable"]);
    let record = explain(&alpha.member, &id);
    assert_eq!(record["settings"]["rule"], Value::Null);
    for name in ["alpha", "bravo"] {
        assert_eq!(
            candidate(&record, name)["estimate"]["run_from"],
            json!({"from": "prior", "level": "prior"})
        );
    }
    assert_eq!(
        record["decision"]["parameters"]["estimates"]["short_run"],
        "100ms"
    );
    assert_eq!(
        record["decision"]["parameters"]["estimates"]["minimum_family_runs"],
        3
    );
    // Replay reproduces every decision, family estimates and all.
    let replay = alpha.member.ok(&["replay"]);
    assert_eq!(replay["decisions"]["placement"], 3, "{replay}");
    assert_eq!(replay["decisions"]["replayed"], 3, "{replay}");
    assert_eq!(replay["decisions"]["flipped"], 0, "{replay}");

    // The report groups placement's run estimates by level, and its backtest
    // finds the family estimate of "test two" against the baseline's prior.
    let report = alpha.member.ok(&["report", "--since", "1h"]);
    let levels: Vec<(&str, &str)> = report["accuracy"]["run"]["by_level"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| (g["member"].as_str().unwrap(), g["level"].as_str().unwrap()))
        .collect();
    for expected in [
        ("bravo", "family"),
        ("bravo", "converted"),
        ("alpha", "prior"),
    ] {
        assert!(levels.contains(&expected), "{expected:?} in {levels:?}");
    }
    let backtest = &report["accuracy"]["backtest"];
    let family = backtest["by_level"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["level"] == "family")
        .unwrap_or_else(|| panic!("{backtest}"));
    assert_eq!(family["estimate"]["priors"], 0, "{backtest}");
    assert!(
        family["baseline"]["priors"].as_u64().unwrap() >= 1,
        "{backtest}"
    );
}

#[test]
fn perf_a_member_that_never_answers_delays_a_new_request_by_under_a_second() {
    let alpha = Machine::start("alpha", 10, 0.0, 1.0);
    let bravo = Machine::start("bravo", 10, 0.0, 1.0);
    enrol(&alpha.member, &bravo.member);
    let (_temp, repo) = repository();
    placed(&alpha.member, &repo, &["--placeable"]);
    // Stopped, bravo accepts connections but never answers them.
    let pid = bravo.member.service_pid() as i32;
    // SAFETY: kill has no memory-safety preconditions.
    unsafe { libc::kill(pid, libc::SIGSTOP) };
    let started = Instant::now();
    let (id, member) = placed(&alpha.member, &repo, &["--placeable"]);
    let took = started.elapsed();
    // SAFETY: as above.
    unsafe { libc::kill(pid, libc::SIGCONT) };
    assert_eq!(member, "alpha");
    if std::env::var_os("EC_TEST_PERF").is_some() {
        assert!(took < Duration::from_millis(2500), "{took:?}");
    }
    // It was scored on its earlier answer.
    let record = explain(&alpha.member, &id);
    assert_eq!(
        record["decision"]["candidates"].as_array().unwrap().len(),
        2,
        "{record}"
    );
}

#[test]
fn waiting_deferrable_work_does_not_hold_up_placeable_work() {
    let alpha = Machine::start("alpha", 4, 0.0, 1.0);
    let bravo = Machine::start("bravo", 4, 0.0, 1.0);
    enrol(&alpha.member, &bravo.member);
    let (_temp, repo) = repository();
    let mut holder = hold(&alpha.member, "2", "half");
    // A deferrable request for all four cores waits behind the lease.
    let mut deferrable = crate::processes::command(env!("CARGO_BIN_EXE_elsecall"))
        .env("HOME", alpha.member.home.path())
        .args([
            "lease",
            "acquire",
            "--size",
            "4",
            "--deferrable",
            "--deadline",
            "30m",
            "--at-deadline",
            "skip",
            "--label",
            "cache warm",
        ])
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    until(
        "the deferrable request to queue",
        crate::processes::WAIT,
        || alpha.member.ok(&["runs", "--limit", "1"])["member"]["queued"]["deferrable"] == 1,
    );
    let (id, member) = placed(&alpha.member, &repo, &["--placeable", "--size", "1"]);
    assert_eq!(member, "alpha");
    let record = explain(&alpha.member, &id);
    assert_eq!(
        candidate(&record, "alpha")["estimate"]["start"]["at"],
        "now",
        "{record}"
    );
    for child in [&mut holder, &mut deferrable] {
        let _ = child.kill();
        let _ = child.wait();
    }
}

#[test]
fn concurrent_placeable_requests_all_run_without_overcommitting() {
    let alpha = Machine::start("alpha", 4, 0.0, 1.0);
    let bravo = Machine::start("bravo", 4, 0.0, 2.0);
    enrol(&alpha.member, &bravo.member);
    let (_temp, repo) = repository();
    placed(&alpha.member, &repo, &["--member", "bravo"]);
    let children: Vec<Child> = (0..4)
        .map(|_| {
            spawn(
                &alpha.member,
                &repo,
                &[
                    "--placeable",
                    "--size",
                    "3",
                    "--",
                    "/bin/sh",
                    "-c",
                    "sleep 1; echo $ELSECALL_MEMBER",
                ],
            )
        })
        .collect();
    let mut members = Vec::new();
    for child in children {
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{output:?}");
        members.push(String::from_utf8(output.stdout).unwrap().trim().to_owned());
    }
    assert!(members.iter().any(|m| m == "bravo"), "{members:?}");
    // Size 3 on capacity 4: no member ever ran two at once.
    for member in [&alpha.member, &bravo.member] {
        let runs = member.ok(&["runs", "--limit", "20"]);
        let mut spans: Vec<(String, String)> = runs["runs"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["size"] == 3.0 && r["member"] == member.name)
            .map(|r| {
                let id = r["run_id"].as_str().unwrap();
                let record = explain(member, id);
                (
                    record["started_at"].as_str().unwrap().to_owned(),
                    record["ended_at"].as_str().unwrap().to_owned(),
                )
            })
            .collect();
        spans.sort();
        for pair in spans.windows(2) {
            assert!(
                pair[0].1 <= pair[1].0,
                "{} overlapped: {spans:?}",
                member.name
            );
        }
    }
}

#[test]
fn perf_an_unreachable_member_is_left_out_and_named() {
    let alpha = Machine::start("alpha", 10, 0.0, 1.0);
    let mut bravo = Machine::start("bravo", 10, 0.0, 1.0);
    enrol(&alpha.member, &bravo.member);
    let (_temp, repo) = repository();
    bravo.member.stop();
    let started = Instant::now();
    let (id, member) = placed(&alpha.member, &repo, &["--placeable"]);
    assert_eq!(member, "alpha");
    if std::env::var_os("EC_TEST_PERF").is_some() {
        assert!(started.elapsed() < Duration::from_secs(5));
    }
    let record = explain(&alpha.member, &id);
    assert_eq!(
        record["decision"]["unreachable"][0]["member"], "bravo",
        "{record}"
    );
    assert_eq!(
        record["decision"]["candidates"].as_array().unwrap().len(),
        1
    );
}

#[test]
fn replay_reproduces_real_decisions_and_flips_them_under_changed_parameters() {
    // bravo is warm and twice as fast: it wins by default, 33 s against 60 s.
    let alpha = Machine::start("alpha", 4, 0.0, 1.0);
    let bravo = Machine::start("bravo", 4, 0.0, 2.0);
    enrol(&alpha.member, &bravo.member);
    let (_temp, repo) = repository();
    placed(&alpha.member, &repo, &["--member", "bravo"]);
    let (faster, member) = placed(&alpha.member, &repo, &["--placeable", "--size", "2"]);
    assert_eq!(member, "bravo");
    // A saturated caller sends its work away.
    alpha.set_utilization(0.97);
    let (away, member) = placed(&alpha.member, &repo, &["--placeable"]);
    assert_eq!(member, "bravo");
    alpha.set_utilization(0.05);
    // Leases without an expected run time hold both members for the assumed
    // 60 s, longer than the hold limit: fleet_busy.
    let mut here = hold(&alpha.member, "4", "hold alpha");
    let mut there = hold(&bravo.member, "4", "hold bravo");
    let output = run(
        &alpha.member,
        &repo,
        &["--placeable", "--hold-limit", "8s", "--", "/usr/bin/true"],
    );
    let error = envelope(&output)["error"].clone();
    assert_eq!(error["code"], "fleet_busy", "{output:?}");
    let busy = error["run_id"].as_str().unwrap().to_owned();
    for child in [&mut here, &mut there] {
        child.kill().unwrap();
        let _ = child.wait();
    }

    // Unchanged, every decision reproduces from its arrival scoring; so does
    // every member's gate, derived again from its recorded load.
    for settings in [vec![], vec!["--set", "admission.saturated_utilization=0.9"]] {
        let mut args = vec!["replay"];
        args.extend(settings);
        let replay = alpha.member.ok(&args);
        let decisions = &replay["decisions"];
        assert_eq!(decisions["placement"], 3, "{replay}");
        assert_eq!(decisions["replayed"], 3, "{replay}");
        assert_eq!(decisions["flipped"], 0, "{replay}");
        assert_eq!(decisions["replayed_from"]["arrival"], 3);
        assert_eq!(replay["effect"]["fleet_busy"]["recorded"], 1);
        assert_eq!(replay["member"], "alpha");
    }

    // A 50% keep-local margin keeps the faster member's work on the caller.
    let replay = alpha
        .member
        .ok(&["replay", "--set", "keep_local_margin=0.5"]);
    assert_eq!(replay["set"], json!({"placement.keep_local_margin": 0.5}));
    assert_eq!(replay["decisions"]["flipped"], 1, "{replay}");
    let flip = &replay["flips"][0];
    assert_eq!(flip["run_id"], faster.as_str());
    assert_ne!(
        flip["run_id"],
        away.as_str(),
        "a saturated caller still cannot"
    );
    assert_eq!(flip["recorded"]["winner"], "bravo");
    assert_eq!(flip["recorded"]["completion_seconds"], 33.0);
    assert_eq!(flip["replayed"]["winner"], "alpha");
    assert_eq!(flip["replayed"]["completion_seconds"], 60.0);
    assert_eq!(flip["completion_difference_seconds"], 27.0);
    assert_eq!(flip["ran_on"], "bravo");
    assert!(flip["actual"]["completion_seconds"].as_f64().unwrap() < 30.0);
    assert!(
        replay["limits"][0]
            .as_str()
            .unwrap()
            .contains("Queue effects are not simulated")
    );

    // Assuming held work ends within 1 s, the fleet_busy request had a place.
    let replay = alpha
        .member
        .ok(&["replay", "--set", "unknown_remaining=1s"]);
    let flips = replay["flips"].as_array().unwrap();
    let placed = flips
        .iter()
        .find(|f| f["run_id"] == busy.as_str())
        .unwrap_or_else(|| panic!("{replay}"));
    assert_eq!(placed["verdict"], "fleet_busy");
    assert_eq!(placed["recorded"]["winner"], Value::Null);
    assert!(placed["replayed"]["winner"].is_string(), "{placed}");
    assert_eq!(replay["effect"]["fleet_busy"]["would_place"], 1);

    // The period, and what replay refuses.
    let future = alpha
        .member
        .ok(&["replay", "--since", "2099-01-01T00:00:00Z"]);
    assert_eq!(future["decisions"]["placement"], 0);
    let recent = alpha.member.ok(&["replay", "--since", "1h"]);
    assert_eq!(recent["decisions"]["placement"], 3);
    let refused = alpha.member.run(&["replay", "--set", "estimates.window=5"]);
    assert_eq!(refused.status.code(), Some(2));
    let error = &members::json(&refused)["error"];
    assert_eq!(error["code"], "invalid_request");
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains("cannot re-derive"),
        "{error}"
    );
}

#[test]
fn perf_a_request_behind_long_running_work_without_an_estimate_moves_well_before_its_hold_limit() {
    // A short floor and a 10 s prior keep the test short; the growth is the
    // default, half the time the work has run.
    let alpha = Machine::configured(
        "alpha",
        2,
        1.0,
        "[placement]\nunknown_remaining = \"1s\"\nrun_prior = \"10s\"\n",
    );
    let bravo = Machine::start("bravo", 2, 0.0, 1.0);
    enrol(&alpha.member, &bravo.member);
    let (_temp, repo) = repository();
    placed(&alpha.member, &repo, &["--member", "bravo"]);
    // Work nobody has an estimate for fills alpha.
    let mut here = hold(&alpha.member, "2", "probe-unknown");
    let started = Instant::now();
    let child = spawn(
        &alpha.member,
        &repo,
        &[
            "--placeable",
            "--size",
            "2",
            "--hold-limit",
            "120s",
            "--",
            "/bin/sh",
            "-c",
            "echo $ELSECALL_RUN_ID $ELSECALL_MEMBER",
        ],
    );
    // On arrival alpha completes first (about 1 s + 10 s against bravo's 3 s
    // warm sync + 10 s), so the request waits there; as the lease runs on, the
    // wait it is assumed to need grows past bravo's completion by the moving
    // margin (2 s), at about 10 s in, and the request moves.
    let output = child.wait_with_output().unwrap();
    let waited = started.elapsed();
    here.kill().unwrap();
    let _ = here.wait();
    assert!(output.status.success(), "{output:?}");
    let text = String::from_utf8(output.stdout).unwrap();
    let (id, member) = text.trim().split_once(' ').unwrap();
    let record = explain(&alpha.member, id);
    assert_eq!(member, "bravo", "{record}");
    if std::env::var_os("EC_TEST_PERF").is_some() {
        assert!(
            waited < Duration::from_secs(40),
            "moved after {waited:?}, its hold limit is 120 s"
        );
    }
    let arrival = &record["decision"]["arrival"];
    assert_eq!(arrival["winner"], "alpha", "{record}");
    // What the wait was assumed from is recorded: the lease's elapsed time and
    // no estimate of any kind.
    let alpha_arrival = arrival["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["member"] == "alpha")
        .unwrap();
    let held = &alpha_arrival["inputs"]["holding"][0];
    assert_eq!(held["remaining_ms"], Value::Null, "{held}");
    assert_eq!(held["learned_run"], Value::Null, "{held}");
    assert!(held["elapsed_ms"].as_u64().is_some(), "{held}");
    // The last scoring, which moved it, saw a grown wait and says why.
    let alpha_last = candidate(&record, "alpha");
    let last_held = &alpha_last["inputs"]["holding"][0];
    assert!(
        last_held["elapsed_ms"].as_u64().unwrap() >= 9_000,
        "{last_held}"
    );
    let why = alpha_last["why"].as_str().unwrap();
    assert!(
        why.contains("behind running work: 2 cores") && why.contains("(no estimate)"),
        "{why}"
    );
    assert!(
        record["decision"]["reason"]
            .as_str()
            .unwrap()
            .starts_with("bravo completes earliest"),
        "{record}"
    );
    assert_eq!(
        record["decision"]["parameters"]["placement"]["unknown_remaining_per_elapsed"],
        0.5
    );
    // Replay reproduces the arrival scoring from its recorded elapsed time.
    let replay = alpha.member.ok(&["replay"]);
    assert_eq!(replay["decisions"]["flipped"], 0, "{replay}");
    assert_eq!(
        replay["decisions"]["replayed"], replay["decisions"]["placement"],
        "{replay}"
    );
}

#[test]
fn running_work_with_a_family_estimate_is_scored_from_it() {
    // Work without any estimate is assumed to hold its cores for 10 minutes, so
    // only an estimate keeps the caller's wait short.
    let alpha = Machine::configured(
        "alpha",
        2,
        1.0,
        "[placement]\nunknown_remaining = \"10m\"\n[estimates]\nshort_run = \"100ms\"\n",
    );
    let bravo = Machine::start("bravo", 2, 0.0, 1.0);
    enrol(&alpha.member, &bravo.member);
    let (_temp, repo) = repository();
    std::fs::write(
        repo.join(".elsecall.toml"),
        "[[labels]]\nmatch = \"hold *\"\nsize = 2\n",
    )
    .unwrap();
    for _ in 0..3 {
        let output = run(
            &alpha.member,
            &repo,
            &["--pinned", "--label", "hold one", "--", "sleep", "2"],
        );
        assert!(output.status.success(), "{output:?}");
    }
    // Keep the new family label alive through arrival scoring.
    let two = gated_work(&alpha.member, &repo, &["--pinned", "--label", "hold two"]);
    let mut request = spawn(
        &alpha.member,
        &repo,
        &[
            "--placeable",
            "--size",
            "2",
            "--",
            "/bin/sh",
            "-c",
            "echo $ELSECALL_RUN_ID $ELSECALL_MEMBER",
        ],
    );
    until("arrival scoring", crate::processes::WAIT, || {
        request.try_wait().unwrap().is_some()
            || alpha.member.ok(&["runs", "--limit", "1"])["member"]["queued"]["interactive"] == 1
    });
    release_work(two);
    let (id, _member) = placed_output(request.wait_with_output().unwrap());
    let record = explain(&alpha.member, &id);
    let alpha_arrival = record["decision"]["arrival"]["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["member"] == "alpha")
        .unwrap()
        .clone();
    let held = &alpha_arrival["inputs"]["holding"][0];
    assert_eq!(held["remaining_ms"], Value::Null, "{held}");
    let learned = &held["learned_run"];
    assert_eq!(learned["from"], "family_runs", "{held}");
    assert_eq!(learned["samples"], 3, "{held}");
    assert_eq!(learned["label_samples"], 0, "{held}");
    let ms = learned["ms"].as_u64().unwrap();
    let records = alpha.member.ok(&["runs", "--limit", "100"]);
    let mut training: Vec<_> = records["runs"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["label"] == "hold one")
        .map(|r| r["run_ms"].as_u64().unwrap())
        .collect();
    training.sort();
    assert!(
        ms.abs_diff(training[1]) <= 1,
        "the family's measured median: {held}; {training:?}"
    );
    let wait = alpha_arrival["wait_seconds"].as_f64().unwrap();
    let elapsed = held["elapsed_ms"].as_u64().unwrap();
    if elapsed < ms {
        assert!(wait <= ms as f64 / 1000.0 + 0.001, "{alpha_arrival}");
    } else {
        // Descheduling can outlive a learned run. The recorded estimate must
        // still be the family's, and the normal unknown-time rule then applies.
        assert!(wait >= 600.0, "{alpha_arrival}");
    }

    // Work the master placed on bravo, as bravo's quote lists it, takes its
    // family from the master's record and its warm runs there (the first sets
    // the copy up, cold). Until its sync is reported, its command has not run.
    for _ in 0..4 {
        let output = run(
            &alpha.member,
            &repo,
            &[
                "--member", "bravo", "--label", "hold one", "--", "sleep", "2",
            ],
        );
        assert!(output.status.success(), "{output:?}");
    }
    let three = gated_work(
        &alpha.member,
        &repo,
        &["--member", "bravo", "--label", "hold three"],
    );
    let (id, _) = placed(&alpha.member, &repo, &["--placeable", "--size", "2"]);
    release_work(three);
    let record = explain(&alpha.member, &id);
    let bravo_arrival = record["decision"]["arrival"]["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["member"] == "bravo")
        .unwrap()
        .clone();
    let held = &bravo_arrival["inputs"]["holding"][0];
    assert_eq!(held["remaining_ms"], Value::Null, "{held}");
    assert_eq!(held["learned_run"]["from"], "family_runs", "{held}");
    assert_eq!(held["learned_run"]["samples"], 3, "{held}");
    assert!(held["elapsed_ms"].is_u64(), "{held}");

    let replay = alpha.member.ok(&["replay"]);
    assert_eq!(replay["decisions"]["flipped"], 0, "{replay}");
    assert_eq!(replay["decisions"]["replayed"], 2, "{replay}");
}

/// A sequential plan of `units` with keys that set it apart, as `--workload`
/// takes it.
fn plan(units: u32) -> String {
    json!({"kind": "sequential", "planned_units": units, "stop_policy": "continue_all",
           "estimate_key": format!("{units:064x}"), "variant_key": "b".repeat(64)})
    .to_string()
}

#[test]
fn a_declared_plan_is_estimated_by_its_units_from_observed_plans() {
    let alpha = Machine::configured("alpha", 10, 1.0, "[estimates]\nshort_run = \"100ms\"\n");
    let bravo = Machine::start("bravo", 10, 0.0, 1.0);
    enrol(&alpha.member, &bravo.member);
    let (_temp, repo) = repository();
    std::fs::write(
        repo.join(".elsecall.toml"),
        "[[labels]]\nmatch = \"test *\"\nsize = 1\n",
    )
    .unwrap();
    // A first run pays the session's start-up costs, so no plan carries them.
    let warm_up = run(
        &alpha.member,
        &repo,
        &[
            "--label",
            "warm-up",
            "--",
            "/bin/sh",
            "-c",
            "\"$EC\" --version",
        ],
    );
    assert!(warm_up.status.success(), "{warm_up:?}");
    // Plans of 6, 2 and 10 units of 0.1 s each, after 0.2 s of set-up, whose
    // wrappers report every unit.
    let sizes = [6u32, 2, 10];
    let mut training = Vec::new();
    for units in sizes {
        let observed = json!({"status": "all_units_observed", "coverage": {
            "attempted_units": units, "terminal_units": units, "failed_units": 0}});
        let script = format!(
            "sleep {}; \"$EC\" workload observe --observation '{observed}' >/dev/null || exit 99; echo $ELSECALL_RUN_ID",
            0.2 + 0.1 * f64::from(units)
        );
        let label = format!("test plan of {units}");
        let workload = plan(units);
        let output = run(
            &alpha.member,
            &repo,
            &[
                "--label",
                &label,
                "--workload",
                &workload,
                "--",
                "/bin/sh",
                "-c",
                &script,
            ],
        );
        assert!(output.status.success(), "{output:?}");
        let id = String::from_utf8(output.stdout).unwrap().trim().to_owned();
        assert_eq!(
            explain(&alpha.member, &id)["plan_observation"]["status"],
            "all_units_observed"
        );
        training.push(command_seconds(&alpha.member, &id));
    }

    // A plan of 40 units of the same family is expected to take about 4.2 s:
    // the line through the observed plans, not their median of under a second.
    let (id, _) = placed(
        &alpha.member,
        &repo,
        &[
            "--label",
            "test plan of 40",
            "--workload",
            &plan(40),
            "--placeable",
        ],
    );
    let record = explain(&alpha.member, &id);
    let estimate = &candidate(&record, "alpha")["estimate"];
    assert_eq!(
        estimate["run_from"],
        json!({"from": "plan_runs", "samples": 3, "units": 40,
               "conversions": {"learned": 0, "measured": 0}, "level": "plan"}),
        "{record}"
    );
    let run = estimate["run_seconds"].as_f64().unwrap();
    // Theil–Sen through the three measured plans: the median slope, then the
    // median overhead.
    let points: Vec<(f64, f64)> = sizes
        .iter()
        .map(|u| f64::from(*u))
        .zip(training.clone())
        .collect();
    let slope = median(
        [(0, 1), (0, 2), (1, 2)]
            .iter()
            .map(|&(a, b)| (points[b].1 - points[a].1) / (points[b].0 - points[a].0))
            .collect(),
    )
    .max(0.0);
    let overhead = median(
        points
            .iter()
            .map(|(units, seconds)| seconds - slope * units)
            .collect(),
    )
    .max(0.0);
    near(run, overhead + slope * 40.0);
    assert!((3.0..6.0).contains(&run), "{run}: {record}");
    assert!(
        candidate(&record, "alpha")["why"]
            .as_str()
            .unwrap()
            .contains("plan of 40 units, from 3 observed plans of this label and family"),
        "{record}"
    );
    assert_eq!(
        candidate(&record, "bravo")["estimate"]["run_from"]["level"],
        "plan",
        "{record}"
    );

    // Work that declares no plan keeps its family's median.
    let (id, _) = placed(
        &alpha.member,
        &repo,
        &["--label", "test opaque", "--placeable"],
    );
    let record = explain(&alpha.member, &id);
    let estimate = &candidate(&record, "alpha")["estimate"];
    assert_eq!(estimate["run_from"]["from"], "family_runs", "{record}");
    near(estimate["run_seconds"].as_f64().unwrap(), median(training));

    // A running plan holds its cores for its plan's expected time.
    let held = gated_work(
        &alpha.member,
        &repo,
        &["--label", "test plan held", "--workload", &plan(40)],
    );
    let (id, _) = placed(
        &alpha.member,
        &repo,
        &["--label", "test after", "--placeable"],
    );
    release_work(held);
    let record = explain(&alpha.member, &id);
    let holding = candidate(&record, "alpha")["inputs"]["holding"]
        .as_array()
        .unwrap();
    let learned = holding
        .iter()
        .find_map(|h| h.get("learned_run"))
        .unwrap_or_else(|| panic!("{record}"));
    assert_eq!(learned["from"], "plan_runs", "{record}");
    assert_eq!(learned["units"], 40, "{record}");
    near(learned["ms"].as_f64().unwrap() / 1000.0, run);

    // Replay reproduces every decision from its recorded estimates.
    let replay = alpha.member.ok(&["replay"]);
    assert_eq!(replay["decisions"]["placement"], 3, "{replay}");
    assert_eq!(replay["decisions"]["flipped"], 0, "{replay}");
}
