#![cfg(target_os = "macos")]
//! `elsecall report` over real member services in separate temporary homes,
//! with loopback standing in for the tailnet: real requests make real records
//! and load history on both members, and each member's report asks the other.
mod members;
mod processes;

use members::{TestMember, enrol};
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::{Child, Output, Stdio},
};

fn git(dir: &Path, args: &[&str]) {
    let output = crate::processes::command("git")
        .current_dir(dir)
        .args(["-c", "user.name=Test", "-c", "user.email=test@example.com"])
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}: {output:?}");
}

fn repository() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::Builder::new()
        .prefix("ec-report-")
        .tempdir_in("/tmp")
        .unwrap();
    let root = temp.path().canonicalize().unwrap().join("app");
    std::fs::create_dir_all(&root).unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    std::fs::write(root.join("README.md"), "hello\n").unwrap();
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-q", "-m", "initial"]);
    (temp, root)
}

/// A member with scripted cores and speed.
fn machine(name: &'static str, cores: u32, speed: f64) -> (TestMember, tempfile::TempDir) {
    let dir = tempfile::Builder::new()
        .prefix("ec-load-")
        .tempdir_in("/tmp")
        .unwrap();
    let load = dir.path().join("load.json");
    std::fs::write(&load, json!({"performance_cores": cores}).to_string()).unwrap();
    let member = TestMember::start_with(
        name,
        "",
        &[
            ("ELSECALL_MACHINE_LOAD", &format!("file:{}", load.display())),
            ("ELSECALL_MACHINE_SPEED", &speed.to_string()),
        ],
    );
    (member, dir)
}

fn run(caller: &TestMember, cwd: &Path, args: &[&str]) -> Output {
    crate::processes::command(env!("CARGO_BIN_EXE_elsecall"))
        .current_dir(cwd)
        .env("HOME", caller.home.path())
        .arg("run")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

/// Holds `size` cores of `member` until the returned child is killed.
fn hold(member: &TestMember, size: &str) -> Child {
    let mut child = crate::processes::command("/bin/sh")
        .arg("-c")
        .arg(format!(
            "\"$EC\" lease acquire --size {size} --label hold --holder $$ >/dev/null || exit 1; echo held; exec sleep 120"
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

fn member<'a>(report: &'a Value, name: &str) -> &'a Value {
    report["members"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["member"] == name)
        .unwrap_or_else(|| panic!("no member {name}: {report}"))
}

fn group<'a>(groups: &'a Value, member: &str, label: Option<&str>) -> &'a Value {
    groups
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["member"] == member && label.is_none_or(|l| g["label"] == l))
        .unwrap_or_else(|| panic!("no group {member} {label:?}: {groups}"))
}

#[test]
fn report_covers_every_members_requests_load_and_estimates() {
    let (alpha, _alpha_load) = machine("alpha", 4, 1.0);
    let (mut bravo, _bravo_load) = machine("bravo", 10, 2.0);
    enrol(&alpha, &bravo);
    let (_temp, repo) = repository();

    // A required run sets bravo's copy up; three placeable runs then go to the
    // warm, twice-as-fast bravo on the 60 s prior, taking a fraction of it.
    let required = run(
        &alpha,
        &repo,
        &[
            "--member",
            "bravo",
            "--",
            "/bin/sh",
            "-c",
            "echo $ELSECALL_RUN_ID",
        ],
    );
    assert!(required.status.success(), "{required:?}");
    // Write one caller record in the pre-instrumentation format.
    let id = std::str::from_utf8(&required.stdout).unwrap().trim();
    let db = rusqlite::Connection::open(alpha.data().join("records.sqlite")).unwrap();
    db.busy_timeout(crate::processes::WAIT).unwrap();
    assert_eq!(
        db.execute(
            "UPDATE requests SET record = json_remove(record, '$.run_phases') WHERE run_id = ?",
            [id],
        )
        .unwrap(),
        1
    );
    drop(db);
    let old = alpha.ok(&["explain", id]);
    assert!(old["run_phases"].is_null());
    assert!(old["placed"]["sync"]["duration_ms"].is_u64());
    for _ in 0..3 {
        let output = run(
            &alpha,
            &repo,
            &[
                "--placeable",
                "--label",
                "synthetic",
                "--",
                "/bin/sh",
                "-c",
                "echo $ELSECALL_MEMBER",
            ],
        );
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "bravo");
    }
    // bravo's own run is only in bravo's records.
    let own = run(&bravo, &repo, &["--label", "own", "--", "/usr/bin/true"]);
    assert!(own.status.success());
    // A fleet_busy on alpha.
    let mut holder = hold(&alpha, "4");
    let busy = run(
        &alpha,
        &repo,
        &[
            "--label",
            "late",
            "--hold-limit",
            "1s",
            "--",
            "/usr/bin/true",
        ],
    );
    assert_eq!(busy.status.code(), Some(125));
    holder.kill().unwrap();
    holder.wait().unwrap();

    let report = alpha.ok(&["report", "--since", "1h"]);
    assert_eq!(report["definitions"], 7);
    // Both members answer their outside history, this one locally and the
    // other over the member route; nothing spun for six hours in this test.
    for name in ["alpha", "bravo"] {
        let answered = member(&report, name);
        assert_eq!(
            answered["outside_warnings"],
            serde_json::json!([]),
            "{answered}"
        );
        assert!(answered.get("outside_error").is_none(), "{answered}");
    }
    assert_eq!(report["fleet"]["master"], "alpha");
    assert_eq!(report["fleet"]["this_member"], "alpha");
    let phases = &report["fleet"]["run_phases"];
    assert_eq!(phases["local"]["execution"]["count"], 1);
    assert_eq!(phases["local"]["workspace_sync"]["count"], 0);
    assert_eq!(phases["local"]["result_return"]["count"], 0);
    assert_eq!(phases["local"]["request_to_result"]["count"], 1);
    assert_eq!(phases["remote"]["admission_wait"]["count"], 4);
    assert_eq!(phases["remote"]["workspace_sync"]["count"], 4);
    assert_eq!(phases["remote"]["execution"]["count"], 3);
    assert_eq!(phases["remote"]["result_return"]["count"], 0);
    assert_eq!(phases["remote"]["request_to_result"]["count"], 3);
    assert_eq!(member(&report, "bravo")["run_phases"], *phases);
    assert_eq!(
        report["fleet"]["requests"]["failed"]["fleet_busy"], 1,
        "{report}"
    );
    assert_eq!(report["fleet"]["fleet_busy"]["count"], 1);
    let placement = &report["fleet"]["placement"];
    assert_eq!(placement["required"], 1);
    assert_eq!(placement["placeable"]["elsewhere"], 3, "{report}");
    assert_eq!(placement["placeable"]["by_member"]["bravo"], 3);
    assert_eq!(placement["placeable"]["run_prior"], 3);
    assert!(
        report["fleet"]["waits"]["interactive"]["count"]
            .as_u64()
            .unwrap()
            >= 6
    );

    for name in ["alpha", "bravo"] {
        let view = member(&report, name);
        assert!(
            view["load"]["sampled_seconds"].as_u64().unwrap() >= 1,
            "{view}"
        );
        assert!(view["records"]["requests"].as_u64().unwrap() >= 1, "{view}");
        assert!(view.get("records_error").is_none(), "{view}");
        // Each member answered what held its gate closed: nothing, here.
        assert!(view.get("outside_error").is_none(), "{view}");
        assert!(view["blocked"].is_null(), "{view}");
        assert!(!view["series"].as_array().unwrap().is_empty(), "{view}");
    }
    let bravo_view = member(&report, "bravo");
    assert_eq!(bravo_view["master"], false);
    assert_eq!(bravo_view["speed"]["max"], 2.0);
    assert_eq!(bravo_view["speed"]["drift"], 0.0);
    // The three placed runs, the required one and bravo's own; alpha's lease.
    assert!(
        bravo_view["work"]["runs"].as_u64().unwrap() >= 5,
        "{bravo_view}"
    );
    assert!(member(&report, "alpha")["work"]["leases"].as_u64().unwrap() >= 1);

    // Run estimates: 30 s predicted (60 s of work at speed 2), a fraction taken.
    let run_accuracy = group(
        &report["accuracy"]["run"]["by_label"],
        "bravo",
        Some("synthetic"),
    );
    assert_eq!(run_accuracy["estimator"], "placement");
    assert_eq!(run_accuracy["samples"], 3);
    assert_eq!(run_accuracy["priors"], 3);
    assert_eq!(run_accuracy["predicted_median_seconds"], 30.0);
    assert!(
        run_accuracy["ratio_median"].as_f64().unwrap() < 0.2,
        "{run_accuracy}"
    );
    assert!(run_accuracy["bias_seconds"].as_f64().unwrap() < -25.0);
    let by_level = group(&report["accuracy"]["run"]["by_level"], "bravo", None);
    assert_eq!(by_level["level"], "prior");
    assert_eq!(by_level["samples"], 3);
    // The backtest estimates every completed run again from the master's
    // earlier runs: the three on the prior again, as the baseline would.
    let backtest = &report["accuracy"]["backtest"];
    assert_eq!(backtest["history_member"], "alpha");
    assert_eq!(backtest["limited"], false);
    assert!(backtest["runs"].as_u64().unwrap() >= 4, "{backtest}");
    assert_eq!(
        backtest["all"]["estimate"]["priors"], backtest["all"]["baseline"]["priors"],
        "nothing for families or learned ratios to add: {backtest}"
    );
    let sync = group(&report["accuracy"]["sync"]["by_member"], "bravo", None);
    assert_eq!(sync["warmth"], "warm");
    assert_eq!(sync["samples"], 3, "{sync}");
    let wait = group(&report["accuracy"]["wait"]["by_member"], "bravo", None);
    assert_eq!(wait["samples"], 3, "the placeable runs' arrival estimates");
    let finding = report["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["kind"] == "run_estimate")
        .unwrap_or_else(|| panic!("{report}"));
    assert_eq!(finding["label"], "synthetic");
    assert_eq!(finding["member"], "bravo");
    let message = finding["message"].as_str().unwrap();
    assert!(
        message.starts_with("Label \"synthetic\" run estimates on bravo are")
            && message.contains("too high"),
        "{message}"
    );

    // Filters.
    let synthetic = alpha.ok(&["report", "--since", "1h", "--label", "synth*"]);
    assert_eq!(synthetic["fleet"]["requests"]["total"], 3);
    assert_eq!(synthetic["filters"]["label"], "synth*");
    let bravo_own = alpha.ok(&["report", "--member", "bravo", "--label", "own"]);
    assert_eq!(bravo_own["fleet"]["requests"]["total"], 1);
    assert_eq!(bravo_own["members"].as_array().unwrap().len(), 1);

    // The same fleet from bravo, which reads alpha's requests over the channel.
    let from_bravo = bravo.ok(&["report", "--since", "30m", "--bucket", "1m"]);
    assert_eq!(from_bravo["fleet"]["this_member"], "bravo");
    assert_eq!(
        from_bravo["fleet"]["placement"]["placeable"]["elsewhere"],
        3
    );
    assert_eq!(from_bravo["period"]["series_seconds"], 60);

    // Invalid requests.
    for args in [
        &["report", "--since", "40d"][..],
        &["report", "--member", "nobody"],
        &["report", "--label", ""],
    ] {
        let output = alpha.run(args);
        assert_eq!(output.status.code(), Some(2), "{args:?}: {output:?}");
        assert_eq!(members::json(&output)["error"]["code"], "invalid_request");
    }

    // A member that cannot answer is named; the rest of the report stands.
    bravo.stop();
    let partial = alpha.ok(&["report", "--since", "1h"]);
    let gone = member(&partial, "bravo");
    assert_eq!(
        gone["records_error"]["code"], "member_unavailable",
        "{gone}"
    );
    assert_eq!(gone["load_error"]["code"], "member_unavailable");
    assert_eq!(partial["findings"][0]["kind"], "incomplete");
    assert_eq!(partial["fleet"]["placement"]["placeable"]["elsewhere"], 3);
}

#[test]
fn report_and_fleet_include_other_members_peer_failures_after_recovery() {
    let mut alpha = TestMember::start("alpha");
    let bravo = TestMember::start("bravo");
    enrol(&alpha, &bravo);
    alpha.stop();
    let failed = bravo.ok(&["fleet"]);
    assert!(
        failed["members"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["name"] == "alpha" && m["reachability"] == "unreachable")
    );
    alpha.restart();
    let fleet = alpha.ok(&["fleet"]);
    let remote = fleet["members"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["name"] == "bravo")
        .unwrap();
    assert!(
        remote["status"]["peer_io"]["attempts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["member"] == "alpha" && a["failure_kind"] == "connect")
    );
    let report = alpha.ok(&["report", "--since", "5m"]);
    let peer = report["peer_io"]["members"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["member"] == "bravo")
        .unwrap();
    assert!(
        peer["history"]["attempts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["member"] == "alpha" && a["attempt"] == 2)
    );
    assert_eq!(peer["history"]["error"], Value::Null);
    let filtered = alpha.ok(&["report", "--since", "5m", "--member", "bravo"]);
    assert_eq!(filtered["peer_io"]["members"].as_array().unwrap().len(), 1);
}
