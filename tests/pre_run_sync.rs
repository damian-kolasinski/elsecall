#![cfg(target_os = "macos")]
//! Configuration sync before placed runs, between real member services in
//! separate temporary homes with loopback standing in for the tailnet: the master
//! brings the member up to date with its synced paths before the run starts there.
mod file_gate;
mod members;
mod processes;
mod sync_gate;

use elsecall::paths::Paths;
use members::{TestMember, enrol};
use serde_json::Value;
use std::{
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::{Child, Output, Stdio},
    time::{Duration, Instant},
};

/// A value that must never appear outside the synced files and the replica store.
const SENTINEL: &str = "s3ntinel-PRERUN-41d9";
const DECLARED: &str = "[sync]\npaths = [\"~/.config/demo\"]\n";
const SECRET: &str = ".config/demo/secret.json";

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

/// A caller repository whose `.elsecall.toml` declares a synced and an unsynced
/// configuration location for consistency checks.
struct Repository {
    _temp: tempfile::TempDir,
    root: PathBuf,
}
fn repository() -> Repository {
    let temp = tempfile::Builder::new()
        .prefix("ec-repo-")
        .tempdir_in("/tmp")
        .unwrap();
    let root = temp.path().canonicalize().unwrap().join("app");
    std::fs::create_dir_all(&root).unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    std::fs::write(
        root.join(".elsecall.toml"),
        "[consistency]\nconfiguration = [\"~/.config/demo/sub/token\", \"~/unsynced.cfg\"]\n",
    )
    .unwrap();
    std::fs::write(root.join("README.md"), "hello\n").unwrap();
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-q", "-m", "initial"]);
    Repository { _temp: temp, root }
}

fn write(member: &TestMember, relative: &str, content: &str) {
    let path = member.home.path().join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn read(member: &TestMember, relative: &str) -> Option<String> {
    std::fs::read_to_string(member.home.path().join(relative)).ok()
}

/// Alpha (master, declaring ~/.config/demo) and bravo, enrolled, with the path
/// enabled and synced at generation 1.
fn synced_pair() -> (TestMember, TestMember) {
    let alpha = TestMember::start_with("alpha", DECLARED, &[]);
    let bravo = TestMember::start_with("bravo", DECLARED, &[]);
    enrol(&alpha, &bravo);
    write(&alpha, SECRET, "{\"token\":\"first\"}");
    write(&alpha, ".config/demo/sub/token", "alpha-token");
    // An unsynced location that differs between them still warns.
    write(&alpha, "unsynced.cfg", "alpha");
    write(&bravo, "unsynced.cfg", "bravo");
    let planned = alpha.ok(&["sync", "enable", "~/.config/demo"]);
    let id = planned["plan"]["plan_id"].as_str().unwrap().to_owned();
    let enabled = alpha.ok(&["sync", "enable", "~/.config/demo", "--confirm", &id]);
    assert_eq!(enabled["round"]["complete"], true, "{enabled}");
    assert_eq!(read(&bravo, SECRET).unwrap(), "{\"token\":\"first\"}");
    // Bravo's override (its directory exists after the first sync) makes its
    // token differ from alpha's on purpose; the next sync lays it.
    write(
        &bravo,
        ".elsecall/sync-overrides/.config/demo/sub/token",
        "bravo-override",
    );
    (alpha, bravo)
}

/// `elsecall run ARGS` from `cwd` through `caller`'s service.
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

/// Runs `script` on bravo, placed by alpha, writing the status file to `status`.
fn placed(alpha: &TestMember, repo: &Repository, status: &Path, script: &str) -> Output {
    run(
        alpha,
        &repo.root,
        &[
            "--member",
            "bravo",
            "--status-file",
            status.to_str().unwrap(),
            "--",
            "/bin/sh",
            "-c",
            script,
        ],
    )
}

fn status_of(path: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

fn latest_run(member: &TestMember) -> Value {
    member.ok(&["runs"])["runs"][0].clone()
}

fn history_len(member: &TestMember) -> usize {
    member.ok(&["sync", "history", "--limit", "500"])["records"]
        .as_array()
        .map_or(0, Vec::len)
}

fn sync_dir(member: &TestMember) -> PathBuf {
    Paths::for_home(member.home.path())
        .unwrap()
        .data
        .join("config-sync")
}

#[test]
fn a_stale_member_is_synced_before_the_run_and_a_current_one_costs_a_check() {
    let (alpha, bravo) = synced_pair();
    let repo = repository();
    let status = repo.root.parent().unwrap().join("status.json");

    // The master's file changes; nothing moves until a placed run needs it.
    let value = format!("{{\"token\":\"{SENTINEL}\"}}");
    write(&alpha, SECRET, &value);
    assert_eq!(read(&bravo, SECRET).unwrap(), "{\"token\":\"first\"}");
    let bravo_history = history_len(&bravo);

    let output = placed(
        &alpha,
        &repo,
        &status,
        "cat \"$HOME/.config/demo/secret.json\"",
    );
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        value,
        "the run sees the new file"
    );
    assert!(output.stderr.is_empty(), "{output:?}");
    assert_eq!(history_len(&bravo), bravo_history + 1, "one push");
    assert_eq!(
        read(&bravo, ".config/demo/sub/token").unwrap(),
        "bravo-override",
        "the same push laid bravo's override"
    );

    // Only the unsynced location warns. The synced one is current by
    // construction, though bravo's override makes its fingerprint differ.
    let warnings = status_of(&status)["data"]["warnings"].clone();
    assert_eq!(warnings.as_array().unwrap().len(), 1, "{warnings}");
    assert!(
        warnings[0]
            .as_str()
            .unwrap()
            .contains("Configuration ~/unsynced.cfg differs"),
        "{warnings}"
    );

    // The record says what was pushed, by paths and sizes, and how long it took.
    let summary = latest_run(&alpha);
    let id = summary["run_id"].as_str().unwrap().to_owned();
    assert!(summary["placed"]["config_sync_ms"].is_u64(), "{summary}");
    let record = alpha.ok(&["explain", &id]);
    let sync = &record["config_sync"];
    assert_eq!(sync["member"], "bravo", "{record}");
    assert_eq!(sync["outcome"], "pushed", "{record}");
    assert_eq!(sync["generation"], 2, "{record}");
    assert_eq!(sync["paths"], serde_json::json!(["~/.config/demo"]));
    assert_eq!(
        sync["pushed"],
        serde_json::json!([{"path": "~/.config/demo", "files": 1, "bytes": value.len()}]),
        "{record}"
    );
    assert!(sync["duration_ms"].is_u64(), "{record}");
    assert!(
        record["warnings"].to_string().contains("~/unsynced.cfg")
            && !record["warnings"].to_string().contains("sub/token"),
        "{record}"
    );

    // Unchanged: one generation check, no push.
    let output = placed(&alpha, &repo, &status, "true");
    assert!(output.status.success(), "{output:?}");
    assert_eq!(history_len(&bravo), bravo_history + 1, "no second push");
    let id = latest_run(&alpha)["run_id"].as_str().unwrap().to_owned();
    let record = alpha.ok(&["explain", &id]);
    assert_eq!(record["config_sync"]["outcome"], "current", "{record}");
    assert_eq!(record["config_sync"]["generation"], 2, "{record}");
    assert!(record["config_sync"]["pushed"].is_null(), "{record}");

    // A run on the calling member syncs nothing and records no sync.
    write(
        &alpha,
        SECRET,
        &format!("{{\"token\":\"{SENTINEL}-local\"}}"),
    );
    let output = run(
        &alpha,
        &repo.root,
        &[
            "--pinned",
            "--",
            "/bin/cat",
            alpha.home.path().join(SECRET).to_str().unwrap(),
        ],
    );
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        history_len(&bravo),
        bravo_history + 1,
        "a local run pushes nothing"
    );
    let local = latest_run(&alpha);
    assert!(local["placed"].is_null(), "{local}");
    let id = local["run_id"].as_str().unwrap().to_owned();
    assert!(alpha.ok(&["explain", &id])["config_sync"].is_null());
    assert_eq!(read(&bravo, SECRET).unwrap(), value);

    // Values never reach records, logs, run listings or the status file.
    let mut outputs = vec![
        alpha.ok(&["runs"]).to_string(),
        bravo.ok(&["runs"]).to_string(),
        alpha.log(),
        bravo.log(),
        std::fs::read_to_string(&status).unwrap(),
    ];
    for run in alpha.ok(&["runs"])["runs"].as_array().unwrap() {
        let id = run["run_id"].as_str().unwrap();
        outputs.push(alpha.ok(&["explain", id]).to_string());
        if let Some(hosted) = bravo
            .run(&["explain", id])
            .status
            .success()
            .then(|| bravo.ok(&["explain", id]).to_string())
        {
            outputs.push(hosted);
        }
    }
    for member in [&alpha, &bravo] {
        let data = Paths::for_home(member.home.path()).unwrap().data;
        for name in ["records.sqlite", "records.sqlite-wal"] {
            if let Ok(bytes) = std::fs::read(data.join(name)) {
                outputs.push(String::from_utf8_lossy(&bytes).into_owned());
            }
        }
        outputs.push(std::fs::read_to_string(sync_dir(member).join("history.jsonl")).unwrap());
    }
    for output in outputs {
        assert!(
            !output.contains(SENTINEL),
            "a synced value leaked: {output}"
        );
    }
}

#[test]
fn a_failed_sync_fails_the_run_with_its_code_and_line() {
    let (alpha, bravo) = synced_pair();
    let repo = repository();
    let status = repo.root.parent().unwrap().join("status.json");
    // On bravo, ~/.config becomes a link into ~/.ssh, where sync never writes.
    let home = bravo.home.path();
    std::fs::create_dir_all(home.join(".ssh")).unwrap();
    std::fs::rename(home.join(".config"), home.join("config-moved")).unwrap();
    std::os::unix::fs::symlink(home.join(".ssh"), home.join(".config")).unwrap();
    write(&alpha, SECRET, "{\"token\":\"second\"}");

    let marker = home.join("ran");
    let output = placed(
        &alpha,
        &repo,
        &status,
        &format!("touch {}", marker.display()),
    );
    assert_eq!(output.status.code(), Some(125), "{output:?}");
    assert!(
        !marker.exists(),
        "the command never ran on stale configuration"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    let lines: Vec<&str> = stderr.lines().collect();
    assert_eq!(lines.len(), 2, "{stderr}");
    let envelope: Value = serde_json::from_str(lines[1]).unwrap();
    let error = &envelope["error"];
    assert_eq!(error["code"], "config_sync_failed", "{envelope}");
    assert_eq!(error["details"]["member"], "bravo");
    assert_eq!(error["details"]["cause"], "unsafe_sync_path");
    let id = error["run_id"].as_str().unwrap().to_owned();
    assert_eq!(
        lines[0],
        format!(
            "elsecall: config_sync_failed: The synced configuration on bravo could not be brought up to date before the run (unsafe_sync_path), so the command did not run there. \
Run `elsecall sync push --member bravo` to see why and retry the sync, fix what it reports, then rerun the same command. \
`elsecall explain {id}` shows the record."
        )
    );
    assert_eq!(status_of(&status)["error"]["code"], "config_sync_failed");

    let record = alpha.ok(&["explain", &id]);
    assert_eq!(record["outcome"]["code"], "config_sync_failed", "{record}");
    assert_eq!(
        record["decision"]["verdict"], "config_sync_failed",
        "{record}"
    );
    assert_eq!(record["config_sync"]["outcome"], "failed", "{record}");
    assert_eq!(
        record["config_sync"]["cause"], "unsafe_sync_path",
        "{record}"
    );
    assert!(!home.join(".ssh/demo").exists(), "nothing written there");
}

#[test]
fn an_unreachable_member_is_still_member_unavailable() {
    let (alpha, mut bravo) = synced_pair();
    let repo = repository();
    let status = repo.root.parent().unwrap().join("status.json");
    bravo.stop();
    let output = placed(&alpha, &repo, &status, "true");
    assert_eq!(output.status.code(), Some(125), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    let envelope: Value = serde_json::from_str(stderr.lines().last().unwrap()).unwrap();
    assert_eq!(
        envelope["error"]["code"], "member_unavailable",
        "{envelope}"
    );
    assert!(
        envelope["error"]["message"]
            .as_str()
            .unwrap()
            .contains("bravo could not be reached"),
        "{envelope}"
    );
}

#[test]
fn a_sync_still_running_at_the_hold_limit_fails_the_run() {
    let (alpha, bravo) = synced_pair();
    let repo = repository();
    // A stopped member service accepts the connection and never answers, so the
    // generation check outlasts the hold limit.
    let pid = bravo.service_pid() as i32;
    // SAFETY: kill has no memory-safety preconditions.
    unsafe { libc::kill(pid, libc::SIGSTOP) };
    let output = run(
        &alpha,
        &repo.root,
        &["--member", "bravo", "--hold-limit", "1s", "--", "true"],
    );
    // SAFETY: as above.
    unsafe { libc::kill(pid, libc::SIGCONT) };
    assert_eq!(output.status.code(), Some(125), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    let envelope: Value = serde_json::from_str(stderr.lines().last().unwrap()).unwrap();
    let error = &envelope["error"];
    assert_eq!(error["code"], "config_sync_failed", "{envelope}");
    assert_eq!(error["details"]["cause"], "hold_limit_passed", "{envelope}");
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains("when the 1 s hold limit passed"),
        "{envelope}"
    );
    let record = alpha.ok(&["explain", error["run_id"].as_str().unwrap()]);
    assert_eq!(record["config_sync"]["outcome"], "failed", "{record}");
    assert_eq!(record["config_sync"]["cause"], "hold_limit_passed");
    assert!(record["config_sync"]["duration_ms"].as_u64().unwrap() >= 900);
}

// Falling back when the chosen member's sync fails, and per-member locking.

/// A member of a synced fleet whose machine the test scripts: `cores` idle
/// cores at `speed`, nothing reserved.
struct Scripted {
    member: TestMember,
    _load: tempfile::TempDir,
}

fn scripted(name: &'static str, cores: u32, speed: f64) -> Scripted {
    let load = tempfile::Builder::new()
        .prefix("ec-load-")
        .tempdir_in("/tmp")
        .unwrap();
    let file = load.path().join("load.json");
    std::fs::write(&file, format!("{{\"performance_cores\": {cores}}}")).unwrap();
    let member = TestMember::start_with(
        name,
        &format!("{DECLARED}[capacity]\nreserved_cores = 0\n"),
        &[
            ("ELSECALL_MACHINE_LOAD", &format!("file:{}", file.display())),
            ("ELSECALL_MACHINE_SPEED", &speed.to_string()),
        ],
    );
    Scripted {
        member,
        _load: load,
    }
}

/// A fleet whose first member is the master, every member enrolled there and
/// holding the synced path at generation 1.
fn synced_fleet(machines: &[(&'static str, u32, f64)]) -> Vec<Scripted> {
    let fleet: Vec<Scripted> = machines
        .iter()
        .map(|(name, cores, speed)| scripted(name, *cores, *speed))
        .collect();
    let master = &fleet[0].member;
    for other in &fleet[1..] {
        enrol(master, &other.member);
    }
    write(master, SECRET, "{\"token\":\"first\"}");
    let planned = master.ok(&["sync", "enable", "~/.config/demo"]);
    let id = planned["plan"]["plan_id"].as_str().unwrap().to_owned();
    let enabled = master.ok(&["sync", "enable", "~/.config/demo", "--confirm", &id]);
    assert_eq!(enabled["round"]["complete"], true, "{enabled}");
    fleet
}

/// From now on, syncing to `member` fails: its `~/.config` becomes a link into
/// `~/.ssh`, where sync never writes.
fn break_sync(member: &TestMember) {
    let home = member.home.path();
    std::fs::create_dir_all(home.join(".ssh")).unwrap();
    std::fs::rename(home.join(".config"), home.join("config-moved")).unwrap();
    std::os::unix::fs::symlink(home.join(".ssh"), home.join(".config")).unwrap();
}

/// Runs `args` and then a command printing where it ran and the synced secret.
fn where_and_what(caller: &TestMember, repo: &Repository, args: &[&str]) -> Output {
    let mut all = args.to_vec();
    all.extend([
        "--",
        "/bin/sh",
        "-c",
        "echo $ELSECALL_MEMBER; cat \"$HOME/.config/demo/secret.json\"",
    ]);
    run(caller, &repo.root, &all)
}

fn envelope(output: &Output) -> Value {
    let stderr = String::from_utf8_lossy(&output.stderr);
    serde_json::from_str(stderr.lines().last().expect("an envelope")).unwrap()
}

fn candidates(record: &Value) -> Vec<String> {
    record["decision"]["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["member"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn perf_a_failed_sync_falls_back_to_another_member_and_then_to_the_caller() {
    // bravo is fastest, then charlie, then alpha, the caller and master.
    let fleet = synced_fleet(&[("alpha", 4, 1.0), ("bravo", 4, 4.0), ("charlie", 4, 2.0)]);
    let (alpha, bravo, charlie) = (&fleet[0].member, &fleet[1].member, &fleet[2].member);
    let repo = repository();
    // Both members hold a warm copy, so placement prefers them to the caller.
    for member in ["bravo", "charlie"] {
        let output = run(alpha, &repo.root, &["--member", member, "--", "true"]);
        assert!(output.status.success(), "{output:?}");
    }
    break_sync(bravo);
    write(alpha, SECRET, "{\"token\":\"second\"}");

    // bravo is chosen, its sync fails, and the run goes to charlie instead.
    let started = Instant::now();
    let output = where_and_what(alpha, &repo, &["--placeable", "--size", "2"]);
    assert!(output.status.success(), "{output:?}");
    if std::env::var_os("EC_TEST_PERF").is_some() {
        assert!(started.elapsed() < Duration::from_secs(10));
    }
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "charlie\n{\"token\":\"second\"}",
        "charlie was brought up to date first"
    );
    let summary = latest_run(alpha);
    let id = summary["run_id"].as_str().unwrap().to_owned();
    assert!(
        summary["reason"]
            .as_str()
            .unwrap()
            .contains("bravo was left out"),
        "runs shows it: {summary}"
    );
    let record = alpha.ok(&["explain", &id]);
    let fallbacks = record["fallbacks"].as_array().unwrap();
    assert_eq!(fallbacks.len(), 1, "{record}");
    let failed = &fallbacks[0]["failed_sync"];
    assert_eq!(failed["member"], "bravo", "{record}");
    assert_eq!(failed["outcome"], "failed", "{record}");
    assert_eq!(failed["cause"], "unsafe_sync_path", "{record}");
    assert!(failed["duration_ms"].is_u64(), "{record}");
    assert_eq!(fallbacks[0]["moved_to"], "charlie", "{record}");
    assert!(fallbacks[0]["decided_at"].is_string(), "{record}");
    assert!(
        fallbacks[0]["reason"]
            .as_str()
            .unwrap()
            .starts_with("charlie completes earliest"),
        "{record}"
    );
    // The run's own sync is charlie's; the decision leaves bravo out and says so.
    assert_eq!(record["config_sync"]["member"], "charlie", "{record}");
    assert_eq!(record["config_sync"]["outcome"], "pushed", "{record}");
    assert_eq!(record["decision"]["verdict"], "started", "{record}");
    assert_eq!(candidates(&record), ["alpha", "charlie"], "{record}");
    assert!(
        record["decision"]["reason"].as_str().unwrap().ends_with(
            "bravo was left out: its synced configuration could not be brought up to date (unsafe_sync_path)."
        ),
        "{record}"
    );
    assert!(!bravo.home.path().join(".ssh/demo").exists());

    // With charlie broken too, the next run tries bravo again (the exclusion
    // was that request's), then charlie, then runs on the caller.
    break_sync(charlie);
    write(alpha, SECRET, "{\"token\":\"third\"}");
    let output = where_and_what(alpha, &repo, &["--placeable", "--size", "2"]);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "alpha\n{\"token\":\"third\"}"
    );
    let id = latest_run(alpha)["run_id"].as_str().unwrap().to_owned();
    let record = alpha.ok(&["explain", &id]);
    let moves: Vec<(&str, &str)> = record["fallbacks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| {
            (
                f["failed_sync"]["member"].as_str().unwrap(),
                f["moved_to"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        moves,
        [("bravo", "charlie"), ("charlie", "alpha")],
        "{record}"
    );
    assert!(record["config_sync"].is_null(), "the caller syncs nothing");
    assert!(record["placed"].is_null(), "{record}");
    assert_eq!(record["member"], "alpha", "{record}");
    assert_eq!(candidates(&record), ["alpha"], "{record}");
}

/// Holds every core of `member` with a lease until the returned child is killed.
fn hold(member: &TestMember, cores: u32) -> Child {
    let mut lease = crate::processes::command("/bin/sh")
        .arg("-c")
        .arg(format!(
            "\"$EC\" lease acquire --size {cores} --label hold --holder $$ >/dev/null || exit 1; echo held; exec sleep 120"
        ))
        .env("HOME", member.home.path())
        .env("EC", env!("CARGO_BIN_EXE_elsecall"))
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(lease.stdout.as_mut().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert_eq!(line.trim(), "held");
    lease
}

fn release(mut lease: Child) {
    lease.kill().unwrap();
    let _ = lease.wait();
}

#[test]
fn perf_a_fallback_stays_within_the_hold_limit() {
    let fleet = synced_fleet(&[("alpha", 2, 1.0), ("bravo", 2, 4.0)]);
    let (alpha, bravo) = (&fleet[0].member, &fleet[1].member);
    let repo = repository();
    let output = run(alpha, &repo.root, &["--member", "bravo", "--", "true"]);
    assert!(output.status.success(), "{output:?}");
    break_sync(bravo);
    write(alpha, SECRET, "{\"token\":\"second\"}");
    // Both members are full for longer than the hold limit, so the request
    // waits undecided; bravo frees after 3 s and is chosen then, late.
    let here = hold(alpha, 2);
    let there = hold(bravo, 2);
    let started = Instant::now();
    let request = spawn_cli(
        alpha,
        &repo.root,
        &[
            "run",
            "--placeable",
            "--size",
            "2",
            "--hold-limit",
            "8s",
            "--",
            "true",
        ],
    );
    std::thread::sleep(Duration::from_secs(3));
    release(there);
    let output = request.wait_with_output().unwrap();
    let waited = started.elapsed();
    release(here);
    assert_eq!(output.status.code(), Some(125), "{output:?}");
    // The fallback, about 3 s in, did not restart the hold limit: the request
    // ends at its own 8 s, not 3 + 8.
    assert!(waited >= Duration::from_millis(7900), "{waited:?}");
    if std::env::var_os("EC_TEST_PERF").is_some() {
        assert!(waited < Duration::from_millis(9500), "{waited:?}");
    }
    let error = &envelope(&output)["error"];
    assert_eq!(error["code"], "fleet_busy", "{error}");
    assert!(
        error["message"].as_str().unwrap().contains(
            "bravo: Left out: its synced configuration could not be brought up to date (unsafe_sync_path)."
        ),
        "{error}"
    );
    let record = alpha.ok(&["explain", error["run_id"].as_str().unwrap()]);
    let fallback = &record["fallbacks"][0];
    assert_eq!(fallback["failed_sync"]["member"], "bravo", "{record}");
    // The caller could not start it in time either: it waited, undecided.
    assert!(fallback["moved_to"].is_null(), "{record}");
    assert!(
        fallback["reason"]
            .as_str()
            .unwrap()
            .starts_with("No remaining member could start it in time yet"),
        "{record}"
    );
    assert_eq!(record["decision"]["verdict"], "fleet_busy", "{record}");
}

#[test]
fn a_placeable_run_no_other_member_can_take_still_fails_with_config_sync_failed() {
    // Only bravo is large enough, and its sync fails.
    let fleet = synced_fleet(&[("alpha", 2, 1.0), ("bravo", 8, 1.0)]);
    let (alpha, bravo) = (&fleet[0].member, &fleet[1].member);
    let repo = repository();
    break_sync(bravo);
    write(alpha, SECRET, "{\"token\":\"second\"}");
    let output = run(
        alpha,
        &repo.root,
        &["--placeable", "--size", "4", "--", "true"],
    );
    assert_eq!(output.status.code(), Some(125), "{output:?}");
    let error = &envelope(&output)["error"];
    assert_eq!(error["code"], "config_sync_failed", "{error}");
    assert_eq!(error["details"]["member"], "bravo");
    assert_eq!(error["details"]["cause"], "unsafe_sync_path");
    let record = alpha.ok(&["explain", error["run_id"].as_str().unwrap()]);
    assert_eq!(record["config_sync"]["outcome"], "failed", "{record}");
    assert_eq!(
        record["decision"]["verdict"], "config_sync_failed",
        "{record}"
    );
    assert!(
        record["fallbacks"].is_null(),
        "no fallback was made: {record}"
    );
}

fn spawn_cli(member: &TestMember, cwd: &Path, args: &[&str]) -> Child {
    crate::processes::command(env!("CARGO_BIN_EXE_elsecall"))
        .current_dir(cwd)
        .env("HOME", member.home.path())
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

#[test]
fn a_slow_push_to_one_member_does_not_delay_a_run_on_another() {
    let fleet = synced_fleet(&[("alpha", 4, 1.0), ("bravo", 4, 1.0), ("charlie", 4, 1.0)]);
    let (alpha, bravo, charlie) = (&fleet[0].member, &fleet[1].member, &fleet[2].member);
    let repo = repository();

    // `sync push` issues generation 2 and hangs pushing it to the paused bravo.
    write(alpha, SECRET, "{\"token\":\"second\"}");
    let gate = sync_gate::SyncGate::start(bravo);
    gate.enroll(alpha, bravo);
    gate.arm();
    let mut push = spawn_cli(alpha, &repo.root, &["sync", "push"]);
    gate.wait();
    let deadline = Instant::now() + crate::processes::WAIT;
    while read(charlie, SECRET).as_deref() != Some("{\"token\":\"second\"}") {
        assert!(Instant::now() < deadline, "the push reaches charlie");
        std::thread::sleep(Duration::from_millis(50));
    }

    // Meanwhile a run on charlie needs a newer generation: it is issued and
    // pushed while bravo's push still holds generation 2.
    write(alpha, SECRET, "{\"token\":\"third\"}");
    let output = where_and_what(alpha, &repo, &["--member", "charlie"]);
    let still_pushing = push.try_wait().unwrap().is_none();
    gate.release();
    let pushed = push.wait_with_output().unwrap();

    assert!(still_pushing, "the push to bravo was still in progress");
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "charlie\n{\"token\":\"third\"}"
    );
    let id = latest_run(alpha)["run_id"].as_str().unwrap().to_owned();
    let record = alpha.ok(&["explain", &id]);
    assert_eq!(record["config_sync"]["outcome"], "pushed", "{record}");
    assert_eq!(record["config_sync"]["generation"], 3, "{record}");

    // bravo's push completed once bravo resumed: generation 2's content was
    // kept for it although generation 3 was issued meanwhile.
    assert!(pushed.status.success(), "{pushed:?}");
    let round: Value = serde_json::from_slice(&pushed.stdout).unwrap();
    let to_bravo = round["data"]["deliveries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["member"] == "bravo")
        .cloned()
        .unwrap();
    assert_eq!(to_bravo["outcome"], "applied", "{round}");
    assert_eq!(read(bravo, SECRET).unwrap(), "{\"token\":\"second\"}");
}

#[test]
fn pushes_to_the_same_member_stay_serialized() {
    let fleet = synced_fleet(&[("alpha", 4, 1.0), ("bravo", 4, 1.0)]);
    let (alpha, bravo) = (&fleet[0].member, &fleet[1].member);
    let repo = repository();
    let pushes = history_len(bravo);

    write(alpha, SECRET, "{\"token\":\"second\"}");
    let gate = sync_gate::SyncGate::start(bravo);
    gate.enroll(alpha, bravo);
    gate.arm();
    let push = spawn_cli(alpha, &repo.root, &["sync", "push"]);
    gate.wait();
    let exchanges = gate.exchanges();
    // This request must reach its own deadline while the push holds the lock.
    // If serialization is removed it reaches the proxy and syncs concurrently.
    let blocked = run(
        alpha,
        &repo.root,
        &["--member", "bravo", "--hold-limit", "2s", "--", "true"],
    );
    assert_eq!(blocked.status.code(), Some(125), "{blocked:?}");
    assert_eq!(
        envelope(&blocked)["error"]["details"]["cause"],
        "hold_limit_passed"
    );
    assert_eq!(
        gate.exchanges(),
        exchanges,
        "no second sync reached the member while its lock was held"
    );
    // A run on bravo waits for the push in progress, then finds bravo current.
    let mut placed = spawn_cli(
        alpha,
        &repo.root,
        &[
            "run",
            "--member",
            "bravo",
            "--hold-limit",
            "30s",
            "--",
            "/bin/sh",
            "-c",
            "cat \"$HOME/.config/demo/secret.json\"",
        ],
    );
    let deadline = Instant::now() + crate::processes::WAIT;
    while latest_run(alpha)["state"] != "queued" {
        assert!(
            Instant::now() < deadline,
            "the placed run never reached sync"
        );
        assert!(
            placed.try_wait().unwrap().is_none(),
            "the run bypassed the held sync"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let waiting = placed.try_wait().unwrap().is_none();
    gate.release();
    let pushed = push.wait_with_output().unwrap();
    let output = placed.wait_with_output().unwrap();

    assert!(waiting, "the run waited for bravo's push");
    assert!(pushed.status.success(), "{pushed:?}");
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "{\"token\":\"second\"}"
    );
    let id = latest_run(alpha)["run_id"].as_str().unwrap().to_owned();
    let record = alpha.ok(&["explain", &id]);
    let sync = &record["config_sync"];
    assert_eq!(sync["outcome"], "current", "only after the push: {record}");
    assert_eq!(sync["generation"], 2, "{record}");
    assert_eq!(history_len(bravo), pushes + 1, "one push, not two");
}

#[test]
fn service_configuration_waits_for_placed_leftovers_to_finish_without_stopping_them() {
    let (alpha, bravo) = synced_pair();
    let repo = repository();
    let status = repo.root.parent().unwrap().join("status.json");
    let finish = file_gate::FileGate::new(bravo.home.path(), "finish-leftover");
    let output = placed(
        &alpha,
        &repo,
        &status,
        "(while [ -f \"$HOME/finish-leftover\" ]; do sleep 0.1; done) >/dev/null 2>&1 & echo $!",
    );
    assert!(output.status.success(), "{output:?}");
    let pid: i32 = String::from_utf8(output.stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let config = "[defaults.placement]\nkeep_local_margin=0.2\n";
    std::fs::write(alpha.data().join("fleet-config.toml"), config).unwrap();
    let plan = alpha.ok(&["sync", "service"]);
    alpha.ok(&[
        "sync",
        "service",
        "--confirm",
        plan["plan"]["plan_id"].as_str().unwrap(),
    ]);
    std::thread::sleep(Duration::from_millis(1500));
    let state = bravo.view_of("bravo")["status"]["config_sync"]["service"].clone();
    assert_eq!(state["pending"], true, "{state}");
    // SAFETY: kill(pid, 0) only checks this temporary test process's existence.
    assert_eq!(
        unsafe { libc::kill(pid, 0) },
        0,
        "leftovers must survive a configuration change"
    );
    finish.open();
    let deadline = Instant::now() + crate::processes::WAIT;
    loop {
        let view = alpha.run(&["fleet"]);
        if view.status.success()
            && members::json(&view)["data"]["members"]
                .as_array()
                .unwrap()
                .iter()
                .find(|m| m["name"] == "bravo")
                .unwrap()["status"]["config_sync"]["service"]["current"]
                == true
        {
            break;
        }
        assert!(Instant::now() < deadline, "{}", bravo.log());
        std::thread::sleep(Duration::from_millis(100));
    }
    let record = bravo.ok(&[
        "explain",
        status_of(&status)["data"]["run_id"].as_str().unwrap(),
    ]);
    assert_eq!(
        record["placed"]["leftovers"]["ending"]["by"], "finished",
        "{record}"
    );
}
