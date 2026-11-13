#![cfg(target_os = "macos")]
//! Real services and records; fake only machine observation and the tailnet.
mod members;
mod processes;
use members::{TestMember, enrol};
use serde_json::Value;
use std::{fs, os::unix::fs::PermissionsExt, path::Path};
fn read(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}
#[test]
fn exports_paginated_fleet_history_without_commands_or_environment() {
    let alpha = TestMember::start("alpha");
    let mut bravo = TestMember::start("bravo");
    enrol(&alpha, &bravo);
    let temp = tempfile::tempdir().unwrap();
    for member in [&alpha, &bravo] {
        fs::write(
            member.home.path().join(".elsecall.toml"),
            "[[labels]]\nmatch = 'probe *'\nsize = 0.25\n",
        )
        .unwrap();
        for _ in 0..12 {
            let out = crate::processes::command(env!("CARGO_BIN_EXE_elsecall"))
                .env("HOME", member.home.path())
                .env("TRACE_SECRET_SENTINEL", "PRIVATE_ENV_SENTINEL")
                .current_dir(member.home.path())
                .args([
                    "run",
                    "--label",
                    "probe run",
                    "--size",
                    "0.25",
                    "--",
                    "/bin/sh",
                    "-c",
                    "printf PRIVATE_ARGUMENT_SENTINEL",
                ])
                .output()
                .unwrap();
            assert!(out.status.success(), "{out:?}");
        }
    }
    let repo = temp.path().join("repo");
    fs::create_dir(&repo).unwrap();
    for args in [
        &["init", "-q"][..],
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--allow-empty",
            "-q",
            "-m",
            "initial",
        ],
    ] {
        assert!(
            crate::processes::command("git")
                .current_dir(&repo)
                .args(args)
                .output()
                .unwrap()
                .status
                .success()
        );
    }
    let placed = crate::processes::command(env!("CARGO_BIN_EXE_elsecall"))
        .env("HOME", alpha.home.path())
        .current_dir(&repo)
        .args([
            "run",
            "--member",
            "bravo",
            "--label",
            "remote probe",
            "--",
            "/usr/bin/true",
        ])
        .output()
        .unwrap();
    assert!(placed.status.success(), "{placed:?}");
    let output = temp.path().join("fleet.json");
    let summary = alpha.ok(&[
        "trace",
        "export",
        "--since",
        "1h",
        "--output",
        output.to_str().unwrap(),
    ]);
    assert_eq!(summary["trace_version"], 1);
    assert_eq!(summary["incomplete"], false, "{summary}");
    assert_eq!(summary["requests"], 25);
    assert_eq!(
        fs::metadata(&output).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let bytes = fs::read(&output).unwrap();
    assert_eq!(summary["bytes"], bytes.len());
    let json = String::from_utf8(bytes).unwrap();
    assert!(!json.contains("PRIVATE_ARGUMENT_SENTINEL"));
    assert!(!json.contains("PRIVATE_ENV_SENTINEL"));
    assert!(!json.contains("TRACE_SECRET_SENTINEL"));
    let trace = read(&output);
    assert_eq!(trace["members"].as_array().unwrap().len(), 2);
    for m in trace["members"].as_array().unwrap() {
        assert!(!m["load"].as_array().unwrap().is_empty());
        let settings = m["scheduler_settings"].as_array().unwrap();
        assert_eq!(settings.len(), 1);
        assert!(settings[0]["parameters"].is_object());
        assert!(settings[0]["capacity"]["reserved"].is_number());
    }
    let remote = trace["requests"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["label"] == "remote probe")
        .unwrap();
    assert_eq!(remote["member"], "bravo");
    assert_eq!(remote["placement_request"]["member"], "bravo");
    assert_eq!(remote["warmth"], "cold");
    assert!(remote["sync_ms"].is_number());
    assert_eq!(remote["workspace"]["workspace"].as_str().unwrap().len(), 64);
    assert!(!json.contains(repo.to_str().unwrap()));
    let r = &trace["requests"][0];
    assert_eq!(r["size"], 0.25);
    assert!(r["resources"]["cpu_ms"].is_number());
    assert_eq!(r["placement_request"], "pinned");
    assert!(r["resources"]["started_at"].is_string());
    assert!(r["hold_limit"].is_object());
    assert!(r["decision"]["parameters"].is_object());
    // Existing destinations and symlinks cannot overwrite another file.
    let again = alpha.run(&[
        "trace",
        "export",
        "--since",
        "1h",
        "--output",
        output.to_str().unwrap(),
    ]);
    assert!(!again.status.success());
    assert_eq!(read(&output), trace);
    let link = temp.path().join("link");
    std::os::unix::fs::symlink(&output, &link).unwrap();
    assert!(
        !alpha
            .run(&["trace", "export", "--output", link.to_str().unwrap()])
            .status
            .success()
    );
    assert_eq!(read(&output), trace);
    let filtered = temp.path().join("bravo.json");
    alpha.ok(&[
        "trace",
        "export",
        "--member",
        "bravo",
        "--output",
        filtered.to_str().unwrap(),
    ]);
    assert_eq!(read(&filtered)["requests"].as_array().unwrap().len(), 13);
    assert_eq!(read(&filtered)["incomplete"], true);
    bravo.stop();
    let partial = temp.path().join("partial.json");
    let summary = alpha.ok(&["trace", "export", "--output", partial.to_str().unwrap()]);
    assert_eq!(summary["incomplete"], true);
    assert_eq!(summary["requests"], 13);
    for args in [
        &["trace", "export", "--since", "40d"][..],
        &["trace", "export", "--member", "nobody"],
    ] {
        assert_eq!(alpha.run(args).status.code(), Some(2));
    }
}

#[test]
fn exports_real_overrun_shape_across_load_pages_without_losing_history() {
    use elsecall::{
        load_log::{minute::Minute, store::LoadStore},
        paths::Paths,
        scheduling::timestamp::Timestamp,
    };
    let alpha = TestMember::start("alpha");
    let paths = Paths::for_home(alpha.home.path()).unwrap();
    let store = LoadStore::open(&paths.load_history()).unwrap();
    // The three 61-observation rows from the November 4 preflight. Keep all
    // measured values, moving timestamps only to span three retained pages.
    let rows: Vec<Value> =
        serde_json::from_str(include_str!("fixtures/trace-load-overrun.json")).unwrap();
    let start = Timestamp::now().millis() / 60_000 * 60_000 - 25 * 3_600_000;
    let mut times = Vec::new();
    for (i, mut row) in rows.into_iter().enumerate() {
        let at = Timestamp::from_millis(start + i as u64 * 12 * 3_600_000);
        row["at"] = serde_json::to_value(at).unwrap();
        row["speed"] = row["speed_min"].clone();
        store
            .append(&serde_json::from_value::<Minute>(row).unwrap())
            .unwrap();
        times.push(at);
    }
    let temp = tempfile::tempdir().unwrap();
    let output = temp.path().join("trace.json");
    let summary = alpha.ok(&[
        "trace",
        "export",
        "--since",
        "26h",
        "--output",
        output.to_str().unwrap(),
    ]);
    assert_eq!(summary["incomplete"], false, "{summary}");
    let trace: elsecall::workload::trace::Trace =
        serde_json::from_slice(&fs::read(output).unwrap()).unwrap();
    for at in times {
        assert_eq!(
            trace.members[0]
                .load
                .iter()
                .find(|b| b.at == at)
                .unwrap()
                .seconds,
            61
        );
    }
    elsecall::simulation::validate_trace(&trace).unwrap();
}

#[test]
fn selected_export_is_a_rehearsable_workload_and_explains_rejections() {
    use elsecall::{
        load_log::{minute::Minute, store::LoadStore},
        paths::Paths,
        scheduling::{record::Record, records::Records, run_id::RunId, timestamp::Timestamp},
        workload::trace::Trace,
    };
    use std::time::Duration;
    let alpha = TestMember::start("alpha");
    assert!(
        alpha
            .run(&[
                "run",
                "--label",
                "window fixture",
                "--size",
                "2",
                "--",
                "/usr/bin/true"
            ])
            .status
            .success()
    );
    let paths = Paths::for_home(alpha.home.path()).unwrap();
    let records = Records::open(&paths.records()).unwrap();
    let template = records.recent(1).unwrap().remove(0);
    let start = Timestamp::now().millis() / 60_000 * 60_000 - 60 * 60_000;
    let since = Timestamp::from_millis(start);
    let mut settings = records
        .scheduler_settings(since, Timestamp::now())
        .unwrap()
        .remove(0);
    settings.at = since;
    records.save_scheduler_settings(&settings).unwrap();
    let mut history: Vec<Record> = [1, 5, 9, 13, 17, 21, 22, 23, 25, 27]
        .into_iter()
        .enumerate()
        .map(|(i, minute)| {
            let mut r = template.clone();
            // Replace the seed run too: with relaxed load coverage its recent
            // arrival would otherwise remain a separate eligible window.
            if i > 0 {
                r.run_id = RunId::parse(&format!("r-{:012x}", i + 1)).unwrap();
            }
            r.requested_at = Timestamp::from_millis(start + minute * 60_000);
            r.started_at = Some(r.requested_at);
            r.ended_at = Some(r.requested_at.after(Duration::from_secs(60)));
            r.run_ms = Some(60_000);
            r.cpu_ms = Some(60_000);
            r.waited_ms = Some(0);
            r.decision = None;
            r.settings = None;
            r
        })
        .collect();
    let store = LoadStore::open(&paths.load_history()).unwrap();
    let write_history = |history: &[Record]| {
        for r in history {
            records.save(r).unwrap();
        }
        let sample: Value =
            serde_json::from_str::<Vec<Value>>(include_str!("fixtures/trace-load-overrun.json"))
                .unwrap()
                .remove(0);
        for minute in 0..60 {
            let at = start + minute * 60_000;
            let running = history
                .iter()
                .filter(|r| r.requested_at.millis() <= at && r.ended_at.unwrap().millis() > at)
                .count();
            let mut row = sample.clone();
            row["at"] = serde_json::to_value(Timestamp::from_millis(at)).unwrap();
            row["seconds"] = 60.into();
            row["weighted_cores"] = serde_json::to_value(settings.capacity.weighted_cores).unwrap();
            row["capacity"] = serde_json::to_value(settings.capacity.capacity).unwrap();
            row["cpu_mean"] = (0.1 + running as f64 * 0.15).into();
            row["cpu_max"] = row["cpu_mean"].clone();
            row["outside_mean"] = 0.1.into();
            row["in_use_mean"] = (running as f64 * 2.0).into();
            row["in_use_max"] = row["in_use_mean"].clone();
            row["busy_seconds"] = if running > 0 { 60 } else { 0 }.into();
            row["speed"] = 1.0.into();
            store
                .append(&serde_json::from_value::<Minute>(row).unwrap())
                .unwrap();
        }
    };
    write_history(&history);
    let temp = tempfile::tempdir().unwrap();
    let export = |name: &str| {
        let path = temp.path().join(name);
        let response = alpha.run(&[
            "trace",
            "export",
            "--since",
            &since.to_string(),
            "--rehearsal-window",
            "busiest:4m",
            "--rehearsal-min-interactive",
            "1",
            "--output",
            path.to_str().unwrap(),
        ]);
        (response, path)
    };
    let default_path = temp.path().join("too-small.json");
    let response = alpha.run(&[
        "trace",
        "export",
        "--since",
        &since.to_string(),
        "--rehearsal-window",
        "busiest:4m",
        "--output",
        default_path.to_str().unwrap(),
    ]);
    assert!(!response.status.success(), "{response:?}");
    let result: Value = serde_json::from_slice(&response.stdout).unwrap();
    assert_eq!(result["error"]["details"]["minimum_interactive"], 40);
    assert!(!default_path.exists());
    let (response, path) = export("selected.json");
    assert!(response.status.success(), "{response:?}");
    let result: Value = serde_json::from_slice(&response.stdout).unwrap();
    assert_eq!(
        result["data"]["rehearsal_window"]["chosen"]["interactive_roots"], 3,
        "{result}"
    );
    let selected: Trace = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(selected.since.millis(), start + 19 * 60_000 + 1);
    elsecall::rehearsal::plan::Plan::new(&selected, 6.0).unwrap();
    // Older arrivals stay as history, while no root crosses the chosen start.
    assert!(
        selected
            .requests
            .iter()
            .any(|r| r.fact.requested_at < selected.since)
    );
    assert!(
        selected
            .requests
            .iter()
            .filter(|r| r.fact.requested_at < selected.since)
            .all(|r| r.fact.resources.as_ref().unwrap().ended_at.unwrap() <= selected.since)
    );

    // Arrival cutoff stays fixed while load and settings continue through drain.
    let mut draining = history[0].clone();
    draining.ended_at = Some(Timestamp::from_millis(start + 4 * 60_000));
    draining.run_ms = Some(3 * 60_000);
    records.save(&draining).unwrap();
    let mut tail_settings = settings.clone();
    tail_settings.at = Timestamp::from_millis(start + 3 * 60_000);
    records.save_scheduler_settings(&tail_settings).unwrap();
    let cutoff = Timestamp::from_millis(start + 2 * 60_000);
    for (name, extra) in [
        ("explicit-export.json", vec![]),
        (
            "explicit-selected.json",
            vec![
                "--rehearsal-window",
                "explicit",
                "--rehearsal-min-interactive",
                "1",
            ],
        ),
    ] {
        let path = temp.path().join(name);
        let mut args = vec![
            "trace",
            "export",
            "--since",
            &since.to_string(),
            "--until",
            &cutoff.to_string(),
            "--output",
            path.to_str().unwrap(),
        ]
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
        args.extend(extra.into_iter().map(str::to_owned));
        let refs: Vec<_> = args.iter().map(String::as_str).collect();
        let response = alpha.run(&refs);
        assert!(response.status.success(), "{response:?}");
        let exported: Trace = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(exported.until, cutoff);
        assert!(
            exported
                .requests
                .iter()
                .all(|r| r.fact.requested_at < cutoff)
        );
        assert!(
            exported.members[0]
                .load
                .iter()
                .any(|b| b.at == tail_settings.at)
        );
        assert!(
            exported.members[0]
                .scheduler_settings
                .iter()
                .any(|s| s.at == tail_settings.at)
        );
        elsecall::simulation::validate_trace(&exported).unwrap();
    }
    let before = records.recent(100).unwrap().len();
    let response = alpha.run(&[
        "tune",
        "preflight",
        "--since",
        &since.to_string(),
        "--until",
        &cutoff.to_string(),
        "--rehearsal-window",
        "explicit",
        "--min-interactive",
        "1",
        "--sample",
        "2",
    ]);
    let result: Value = serde_json::from_slice(&response.stdout).unwrap();
    assert_eq!(result["data"]["offline"], true, "{result}");
    assert_eq!(result["data"]["read_only"], true, "{result}");
    assert_eq!(records.recent(100).unwrap().len(), before);
    records.save(&history[0]).unwrap();

    history[7].workspace = None;
    history[7].workspace_recorded = false;
    write_history(&history);
    let (response, path) = export("legacy-workspace.json");
    assert!(response.status.success(), "{response:?}");
    let result: Value = serde_json::from_slice(&response.stdout).unwrap();
    assert_eq!(
        result["data"]["rehearsal_window"]["chosen"]["interactive_roots"],
        3
    );
    let selected: Trace = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let legacy = selected
        .requests
        .iter()
        .find(|r| r.fact.run_id == history[7].run_id.to_string())
        .unwrap();
    assert!(legacy.synthetic_workspace);
    assert!(!legacy.workspace_recorded);
    assert!(legacy.workspace.is_some());
    elsecall::rehearsal::plan::Plan::new(&selected, 6.0).unwrap();

    history[7].workspace_recorded = true;
    history[4].ended_at = Some(Timestamp::from_millis(start + 26 * 60_000));
    history[4].run_ms = Some(9 * 60_000);
    write_history(&history);
    let (response, _) = export("idle-only.json");
    assert!(response.status.success(), "{response:?}");
    let result: Value = serde_json::from_slice(&response.stdout).unwrap();
    assert_eq!(
        result["data"]["rehearsal_window"]["chosen"]["interactive_roots"], 1,
        "{result}"
    );
    assert!(
        response
            .stdout
            .windows(b"active_carry_in".len())
            .any(|w| w == b"active_carry_in")
    );

    for r in &mut history {
        r.workspace = None;
        r.workspace_recorded = false;
        r.outcome = Some(elsecall::scheduling::record::Outcome::Signaled { signal: 9 });
    }
    write_history(&history);
    let (response, path) = export("no-window.json");
    assert!(!response.status.success());
    let result: Value = serde_json::from_slice(&response.stdout).unwrap();
    assert_eq!(result["error"]["code"], "no_rehearsal_window", "{result}");
    assert!(result["error"]["details"]["chosen"].is_null());
    assert!(!path.exists());
    // Zero is not a usable population floor, and the option is selection-only.
    assert!(
        !alpha
            .run(&[
                "trace",
                "export",
                "--rehearsal-window",
                "busiest:4m",
                "--rehearsal-min-interactive",
                "0"
            ])
            .status
            .success()
    );
    assert!(
        !alpha
            .run(&["trace", "export", "--rehearsal-min-interactive", "1"])
            .status
            .success()
    );
    assert!(
        !alpha
            .run(&[
                "trace",
                "export",
                "--member",
                "alpha",
                "--rehearsal-window",
                "busiest:4m"
            ])
            .status
            .success()
    );
}
