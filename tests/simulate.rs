#![cfg(target_os = "macos")]
//! Record decisions from real services, then replay them with the production
//! scheduler and virtual adapters. Only machine observation and tailnet are fake.
mod members;
mod processes;

use elsecall::scheduling::timestamp::Timestamp;
use members::{TestMember, enrol};
use serde_json::{Value, json};
use std::{collections::BTreeMap, fs, path::Path, process::Output};

fn machine(name: &'static str, cores: u32, speed: f64) -> (TestMember, tempfile::TempDir) {
    let temp = tempfile::Builder::new()
        .prefix("ec-sim-load-")
        .tempdir_in("/tmp")
        .unwrap();
    let load = temp.path().join("load.json");
    fs::write(&load, json!({"performance_cores":cores}).to_string()).unwrap();
    let member = TestMember::start_with(
        name,
        "",
        &[
            ("ELSECALL_MACHINE_LOAD", &format!("file:{}", load.display())),
            ("ELSECALL_MACHINE_SPEED", &speed.to_string()),
        ],
    );
    (member, temp)
}

fn run(member: &TestMember, directory: &Path, args: &[&str]) -> Output {
    crate::processes::command(env!("CARGO_BIN_EXE_elsecall"))
        .env("HOME", member.home.path())
        .current_dir(directory)
        .arg("run")
        .args(args)
        .output()
        .unwrap()
}

fn git(directory: &Path, args: &[&str]) {
    let output = crate::processes::command("git")
        .current_dir(directory)
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
    assert!(output.status.success(), "{output:?}");
}

#[test]
fn real_service_decisions_replay_without_installing_or_contacting_services() {
    let (alpha, _alpha_load) = machine("alpha", 4, 1.0);
    let (bravo, _bravo_load) = machine("bravo", 8, 2.0);
    enrol(&alpha, &bravo);
    let temp = tempfile::Builder::new()
        .prefix("ec-sim-")
        .tempdir_in("/tmp")
        .unwrap();
    let repository = temp.path().join("app");
    fs::create_dir(&repository).unwrap();
    git(&repository, &["init", "-q", "-b", "main"]);
    fs::write(repository.join("README.md"), "fixture\n").unwrap();
    git(&repository, &["add", "-A"]);
    git(&repository, &["commit", "-q", "-m", "initial"]);
    let since = Timestamp::now().to_string();
    for member in [&alpha, &bravo] {
        let output = run(
            member,
            &repository,
            &[
                "--pinned",
                "--label",
                "pinned fixture",
                "--",
                "/bin/sleep",
                "0.1",
            ],
        );
        assert!(output.status.success(), "{output:?}");
    }
    // The required cold copy establishes warmth. The next three runs use
    // the faster warm peer; their learned short duration then makes the fourth
    // stay on the caller under the real keep-local margin.
    let required = run(
        &alpha,
        &repository,
        &[
            "--member",
            "bravo",
            "--label",
            "required fixture",
            "--",
            "/bin/sleep",
            "0.1",
        ],
    );
    assert!(required.status.success(), "{required:?}");
    for _ in 0..4 {
        let output = run(
            &alpha,
            &repository,
            &[
                "--placeable",
                "--label",
                "learned fixture",
                "--",
                "/bin/sleep",
                "0.1",
            ],
        );
        assert!(output.status.success(), "{output:?}");
    }
    let too_large = run(
        &alpha,
        &repository,
        &[
            "--pinned",
            "--label",
            "oversized fixture",
            "--size",
            "1000",
            "--",
            "/usr/bin/true",
        ],
    );
    assert_eq!(too_large.status.code(), Some(125), "{too_large:?}");
    let path = temp.path().join("recorded.json");
    let exported = alpha.ok(&[
        "trace",
        "export",
        "--since",
        &since,
        "--output",
        path.to_str().unwrap(),
    ]);
    assert_eq!(exported["incomplete"], false, "{exported}");
    let recorded: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let requests = recorded["requests"].as_array().unwrap();
    assert_eq!(requests.len(), 8, "{recorded}");
    assert_eq!(
        requests
            .iter()
            .filter(|r| r["label"] == "learned fixture" && r["member"] == "bravo")
            .count(),
        3
    );

    assert_eq!(
        requests
            .iter()
            .filter(|r| r["label"] == "learned fixture" && r["member"] == "alpha")
            .count(),
        1,
        "the fourth request must use its learned label history"
    );

    // Stop both fixture services: the CLI must be entirely offline.
    drop(bravo);
    drop(alpha);
    let simulate = |fidelity: bool| {
        let mut command = crate::processes::command(env!("CARGO_BIN_EXE_elsecall"));
        command.env("HOME", temp.path()).args([
            "simulate",
            "--trace",
            path.to_str().unwrap(),
            "--seed",
            "17",
        ]);
        if fidelity {
            command.arg("--fidelity");
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{output:?}\n{}",
            recorded["members"]
        );
        let document: Value = serde_json::from_slice(&output.stdout).unwrap();
        document["data"].clone()
    };
    let replay = simulate(false);
    assert_eq!(
        replay,
        simulate(false),
        "same trace and seed must be identical"
    );
    let outcomes: BTreeMap<_, _> = replay["outcomes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|outcome| (outcome["run_id"].as_str().unwrap(), outcome))
        .collect();
    assert_eq!(outcomes.len(), requests.len(), "{replay}");
    for request in requests {
        let outcome = outcomes[request["run_id"].as_str().unwrap()];
        assert_eq!(outcome["member"], request["member"], "{request}\n{outcome}");
        assert_eq!(
            outcome["failure"], request["failure"],
            "{request}\n{outcome}"
        );
        if let Some(recorded_wait) = request["waited_ms"].as_u64() {
            assert!(
                outcome["waited_ms"]
                    .as_u64()
                    .unwrap()
                    .abs_diff(recorded_wait)
                    <= 1000,
                "{request}\n{outcome}"
            );
        }
    }
    let fidelity = simulate(true);
    assert_eq!(
        fidelity["fidelity"]["status"], "blocked_by_missing_evidence",
        "{fidelity}"
    );
    assert_eq!(fidelity["fidelity"]["trusted_for_tuning"], false);
    assert!(
        fidelity["fidelity"]["blockers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b.as_str().unwrap().contains("complete recorded UTC day")),
        "{fidelity}"
    );

    // Exercise the CLI's arrival boundary using records from the real fixture
    // services. The last command still owes a result after admission closes.
    let mut cut: elsecall::workload::trace::Trace = serde_json::from_value(recorded).unwrap();
    let last = cut
        .requests
        .iter()
        .filter(|r| r.fact.run_ms.is_some())
        .max_by_key(|r| r.fact.requested_at)
        .unwrap();
    let last_id = last.fact.run_id.clone();
    let until = last
        .fact
        .requested_at
        .after(std::time::Duration::from_millis(1));
    cut.until = until;
    cut.requests.retain(|r| r.fact.requested_at < until);
    for member in &mut cut.members {
        member.load.retain(|b| b.at < until);
        member.scheduler_settings.retain(|s| s.at < until);
    }
    let cut_path = temp.path().join("arrival-window.json");
    fs::write(&cut_path, serde_json::to_vec(&cut).unwrap()).unwrap();
    let output = crate::processes::command(env!("CARGO_BIN_EXE_elsecall"))
        .env("HOME", temp.path())
        .args([
            "simulate",
            "--trace",
            cut_path.to_str().unwrap(),
            "--seed",
            "17",
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    let replay = &result["data"];
    let last = replay["outcomes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["run_id"] == last_id)
        .unwrap();
    assert_eq!(last["state"], "ended", "{last}");
    assert!(Timestamp::parse_rfc3339(last["terminal_at"].as_str().unwrap()).unwrap() > until);
    assert_eq!(
        replay["outcomes"].as_array().unwrap().len(),
        cut.requests.len()
    );
    assert_eq!(
        replay["metrics"]["scheduling_objective"]["failures"]["unknown_run_ids"],
        json!([])
    );
}
