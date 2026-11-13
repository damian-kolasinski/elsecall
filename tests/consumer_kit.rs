#![cfg(target_os = "macos")]
//! The consumer integration kit through the real CLI and, where admission is
//! involved, a real service: label rules from `.elsecall.toml`, running anyway
//! when the service is unavailable, `ELSECALL=off`, a holder acquiring again,
//! agent-ready failure lines and the run ID file. Only machine load is faked.
mod processes;
mod support;

use serde_json::Value;
use std::{
    io::Write,
    os::unix::process::ExitStatusExt,
    path::{Path, PathBuf},
    process::{Output, Stdio},
    time::{Duration, Instant},
};
use support::{Fixture, in_use, queued, record};

/// A working tree holding `.elsecall.toml`, found by its `.git` entry.
fn repository(configuration: &str) -> tempfile::TempDir {
    let root = tempfile::Builder::new()
        .prefix("ec-repo-")
        .tempdir_in("/tmp")
        .unwrap();
    std::fs::create_dir(root.path().join(".git")).unwrap();
    std::fs::write(root.path().join(".elsecall.toml"), configuration).unwrap();
    root
}

fn home() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("ec-")
        .tempdir_in("/tmp")
        .unwrap()
}

/// Runs `elsecall ARGS` in `directory` with `input` on stdin and only the given
/// `ELSECALL*` controls set.
fn elsecall(
    home: &Path,
    directory: &Path,
    env: &[(&str, &str)],
    args: &[&str],
    input: &[u8],
) -> Output {
    let mut child = crate::processes::command(env!("CARGO_BIN_EXE_elsecall"))
        .env("HOME", home)
        .env("DIR", directory)
        .env_remove("ELSECALL")
        .env_remove("ELSECALL_HOLD_LIMIT")
        .env_remove("ELSECALL_WHEN_UNAVAILABLE")
        .env_remove("ELSECALL_RUN_ID")
        .envs(env.iter().copied())
        .current_dir(directory)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let input = input.to_vec();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&input);
    });
    let output = child.wait_with_output().unwrap();
    writer.join().unwrap();
    output
}

async fn elsecall_async(
    home: &Path,
    directory: &Path,
    env: &[(&str, &str)],
    args: &[&str],
) -> Output {
    let (home, directory) = (home.to_owned(), directory.to_owned());
    let env: Vec<(String, String)> = env
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    tokio::task::spawn_blocking(move || {
        let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        elsecall(&home, &directory, &env, &args, b"")
    })
    .await
    .unwrap()
}

fn json(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes)
        .unwrap_or_else(|_| panic!("not JSON: {}", String::from_utf8_lossy(bytes)))
}

fn stderr_lines(output: &Output) -> Vec<String> {
    String::from_utf8_lossy(&output.stderr)
        .lines()
        .map(str::to_owned)
        .collect()
}

async fn explain(fx: &Fixture, run_id: &str) -> Value {
    let (code, explained) = fx.cli(&["explain", run_id]).await;
    assert_eq!(code, 0, "{explained}");
    explained["data"].clone()
}

fn read_id(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap()
}

/// A binary round trip through `sh`, with separate stderr and exit code 7.
const ROUND_TRIP: &[&str] = &[
    "--",
    "/bin/sh",
    "-c",
    r"cat; printf '\377\000err' >&2; exit 7",
];
fn binary_input() -> Vec<u8> {
    (0..200_000u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect()
}

const RULES: &str = r#"
[[labels]]
match = "build *"
size = 4
hold_limit = "5m"
[[labels]]
match = "warm *"
size = 2
urgency = "deferrable"
deadline = "30m"
at_deadline = "start"
[[labels]]
match = "section *"
size = 3
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn label_rules_apply_to_runs_and_leases_and_explain_names_the_rule() {
    let fx = Fixture::start(8).await;
    let repo = repository(RULES);
    let nested = repo.path().join("Sources/App");
    std::fs::create_dir_all(&nested).unwrap();
    let id_file = repo.path().join("run-id");
    let id_path = id_file.to_str().unwrap();

    // The rule supplies size and hold limit; the run ID file matches the run's.
    let output = elsecall_async(
        fx.home.path(),
        &nested,
        &[],
        &[
            "run",
            "--label",
            "build App",
            "--run-id-file",
            id_path,
            "--",
            "/bin/sh",
            "-c",
            "printf %s \"$ELSECALL_RUN_ID\"",
        ],
    )
    .await;
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "nothing extra on success");
    let run_id = String::from_utf8(output.stdout).unwrap();
    assert_eq!(read_id(&id_file), format!("{run_id}\n"));
    let build = explain(&fx, &run_id).await;
    assert_eq!(build["size"], 4.0);
    assert_eq!(build["hold_limit"]["seconds"], 300.0);
    assert_eq!(
        build["settings"],
        serde_json::json!({
            "rule": {"number": 1, "match": "build *"},
            "size": "rule", "urgency": "rule", "hold_limit": "rule", "placement": "default"
        })
    );

    // A flag overrides the rule, and the environment overrides the rule's hold limit.
    let output = elsecall_async(
        fx.home.path(),
        repo.path(),
        &[("ELSECALL_HOLD_LIMIT", "45s")],
        &[
            "run",
            "--label",
            "build App",
            "--size",
            "1",
            "--run-id-file",
            id_path,
            "--",
            "true",
        ],
    )
    .await;
    assert!(output.status.success(), "{output:?}");
    let overridden = explain(&fx, read_id(&id_file).trim()).await;
    assert_eq!(overridden["size"], 1.0);
    assert_eq!(overridden["hold_limit"]["seconds"], 45.0);
    assert_eq!(overridden["settings"]["size"], "cli");
    assert_eq!(overridden["settings"]["hold_limit"], "environment");
    assert_eq!(overridden["settings"]["rule"]["number"], 1);

    // A deferrable rule makes the run deferrable; nothing waits, so it starts.
    let output = elsecall_async(
        fx.home.path(),
        repo.path(),
        &[],
        &[
            "run",
            "--label",
            "warm Cache",
            "--run-id-file",
            id_path,
            "--",
            "true",
        ],
    )
    .await;
    assert!(output.status.success(), "{output:?}");
    let warm = explain(&fx, read_id(&id_file).trim()).await;
    assert!(
        warm["urgency"]["deferrable"]["at_deadline"] == "start",
        "{warm}"
    );
    assert_eq!(warm["settings"]["deadline"], "rule");
    assert_eq!(warm["size"], 2.0);

    // Leases follow the rules too.
    let output = elsecall_async(
        fx.home.path(),
        repo.path(),
        &[],
        &["lease", "acquire", "--label", "section Generate"],
    )
    .await;
    assert!(output.status.success(), "{output:?}");
    let lease = json(&output.stdout)["data"].clone();
    assert_eq!(lease["state"], "granted");
    assert_eq!(lease["size"], 3.0);
    let explained = explain(&fx, lease["run_id"].as_str().unwrap()).await;
    assert_eq!(explained["settings"]["rule"]["match"], "section *");
    assert_eq!(explained["settings"]["hold_limit"], "default");
    // The holder (this test's spawn_blocking thread's process) is the test
    // process itself, which keeps running: release the lease explicitly.
    let (code, _) = fx
        .cli(&["lease", "release", lease["run_id"].as_str().unwrap()])
        .await;
    assert_eq!(code, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_rules_fail_with_invalid_request_and_a_precise_hint() {
    let fx = Fixture::start(2).await;
    let repo = repository(
        "[[labels]]\nmatch = \"build *\"\nurgency = \"deferrable\"\ndeadline = \"30m\"\n",
    );
    let marker = repo.path().join("ran");
    let output = elsecall_async(
        fx.home.path(),
        repo.path(),
        &[],
        &[
            "run",
            "--label",
            "build App",
            "--",
            "/bin/sh",
            "-c",
            "touch \"$DIR/ran\"",
        ],
    )
    .await;
    assert_eq!(output.status.code(), Some(125));
    assert!(!marker.exists(), "the command never runs");
    let lines = stderr_lines(&output);
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(
        lines[0].starts_with("elsecall: invalid_request: ")
            && lines[0].contains("[[labels]] rule 1 (match = \"build *\"): urgency = \"deferrable\" needs both deadline and at_deadline."),
        "{}",
        lines[0]
    );
    let envelope = json(lines[1].as_bytes());
    assert_eq!(envelope["error"]["code"], "invalid_request");
    let hint = envelope["error"]["hint"].as_str().unwrap();
    assert!(
        hint.starts_with(
            "Add deadline (such as \"30m\") and at_deadline (\"start\" or \"skip\") in rule 1 in "
        ) && hint.ends_with(".elsecall.toml, then rerun the same command."),
        "{hint}"
    );
    assert!(lines[0].ends_with(hint), "the line ends with the fix");

    // Every request in that working tree fails the same way, leases included.
    let output = elsecall_async(
        fx.home.path(),
        repo.path(),
        &[],
        &["lease", "acquire", "--label", "anything"],
    )
    .await;
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(json(&output.stdout)["error"]["code"], "invalid_request");
    let lines = stderr_lines(&output);
    assert_eq!(lines.len(), 1);
    assert!(lines[0].starts_with("elsecall: invalid_request: "));
}

#[test]
fn work_runs_anyway_when_the_service_is_unavailable_only_if_the_consumer_chose_it() {
    let home = home();
    let plain = repository("");
    let input = binary_input();

    // The default is unchanged: fail with service_unavailable, without running.
    let output = elsecall(
        home.path(),
        plain.path(),
        &[],
        &["run", "--", "/bin/sh", "-c", "touch \"$DIR/ran\""],
        b"",
    );
    assert_eq!(output.status.code(), Some(125));
    assert!(!plain.path().join("ran").exists());
    let lines = stderr_lines(&output);
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(
        lines[0].starts_with("elsecall: service_unavailable: The local service could not be reached. Run elsecall doctor;")
            && lines[0].ends_with("set ELSECALL_WHEN_UNAVAILABLE=run."),
        "{}",
        lines[0]
    );
    assert_eq!(
        json(lines[1].as_bytes())["error"]["code"],
        "service_unavailable"
    );

    // Chosen in .elsecall.toml, in the environment or with a flag, the command
    // runs directly: the same bytes, its exit code, and one notice line first.
    let notice = "elsecall: unmanaged: The local service could not be reached. The command runs without admission. Run `elsecall doctor` to restore admission.\n";
    let chose = repository("[admission]\nwhen_unavailable = \"run\"\n");
    for (directory, env, flag) in [
        (chose.path(), vec![], vec![]),
        (
            plain.path(),
            vec![("ELSECALL_WHEN_UNAVAILABLE", "run")],
            vec![],
        ),
        (plain.path(), vec![], vec!["--when-unavailable", "run"]),
    ] {
        let status = directory.join("status.json");
        let mut args = vec!["run", "--status-file", status.to_str().unwrap()];
        args.extend(flag.iter().copied());
        args.extend(ROUND_TRIP);
        let output = elsecall(home.path(), directory, &env, &args, &input);
        assert_eq!(output.status.code(), Some(7), "{:?}", stderr_lines(&output));
        assert!(
            output.stdout == input,
            "stdout passes through byte for byte"
        );
        let mut expected = notice.as_bytes().to_vec();
        expected.extend_from_slice(b"\xff\x00err");
        assert_eq!(
            output.stderr, expected,
            "exactly one notice line, then the command's stderr"
        );
        let status = json(&std::fs::read(status).unwrap());
        assert_eq!(
            status["data"],
            serde_json::json!({"state": "exited", "run_id": null, "member": null, "admission": "unmanaged", "exit_code": 7})
        );
    }

    // A flag overrides the environment, which overrides the configuration.
    let output = elsecall(
        home.path(),
        chose.path(),
        &[("ELSECALL_WHEN_UNAVAILABLE", "fail")],
        &["run", "--", "true"],
        b"",
    );
    assert_eq!(output.status.code(), Some(125));
    let output = elsecall(
        home.path(),
        plain.path(),
        &[("ELSECALL_WHEN_UNAVAILABLE", "run")],
        &["run", "--when-unavailable", "fail", "--", "true"],
        b"",
    );
    assert_eq!(output.status.code(), Some(125));

    // A lease is not held, and acquire and release still succeed.
    let id_file = chose.path().join("run-id");
    std::fs::write(&id_file, "r-stale0000000\n").unwrap();
    let output = elsecall(
        home.path(),
        chose.path(),
        &[],
        &[
            "lease",
            "acquire",
            "--size",
            "2",
            "--run-id-file",
            id_file.to_str().unwrap(),
        ],
        b"",
    );
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        json(&output.stdout)["data"],
        serde_json::json!({"state": "unmanaged", "run_id": null, "member": null, "size": null, "nested_in": null, "waited_ms": 0, "placement_note": null})
    );
    assert_eq!(
        stderr_lines(&output),
        [
            "elsecall: unmanaged: The local service could not be reached. No lease is held, and the work runs without admission. Run `elsecall doctor` to restore admission."
        ]
    );
    assert_eq!(read_id(&id_file), "", "no record, so no run ID");
    let output = elsecall(home.path(), chose.path(), &[], &["lease", "release"], b"");
    assert!(output.status.success(), "{output:?}");
    assert_eq!(json(&output.stdout)["data"]["state"], "unmanaged");
    let output = elsecall(home.path(), plain.path(), &[], &["lease", "acquire"], b"");
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(json(&output.stdout)["error"]["code"], "service_unavailable");

    // Work required on a named member is bound to it, so it never runs here.
    let output = elsecall(
        home.path(),
        chose.path(),
        &[],
        &[
            "run",
            "--member",
            "devbox",
            "--",
            "/bin/sh",
            "-c",
            "touch \"$DIR/ran\"",
        ],
        b"",
    );
    assert_eq!(output.status.code(), Some(125));
    assert!(!chose.path().join("ran").exists());
    assert_eq!(
        json(stderr_lines(&output)[1].as_bytes())["error"]["code"],
        "service_unavailable"
    );

    // A lease acquire that fails before admission never leaves a stale run ID.
    for (env, args) in [
        (vec![("ELSECALL", "maybe")], vec!["lease", "acquire"]),
        (vec![], vec!["lease", "acquire", "--sise", "2"]),
    ] {
        std::fs::write(&id_file, "r-stale0000000\n").unwrap();
        let mut args = args.clone();
        args.extend(["--run-id-file", id_file.to_str().unwrap()]);
        let output = elsecall(home.path(), chose.path(), &env, &args, b"");
        assert!(!output.status.success());
        assert_eq!(read_id(&id_file), "", "{args:?}");
    }
}

#[test]
fn elsecall_off_runs_commands_directly_and_makes_leases_no_ops() {
    let home = home();
    let repo = repository("");
    let off = [("ELSECALL", "off")];
    let input = binary_input();
    let status = repo.path().join("status.json");
    let mut args = vec![
        "run",
        "--size",
        "64",
        "--status-file",
        status.to_str().unwrap(),
    ];
    args.extend(ROUND_TRIP);
    // No service is installed, and the size is larger than any machine: off
    // never contacts Elsecall.
    let output = elsecall(home.path(), repo.path(), &off, &args, &input);
    assert_eq!(output.status.code(), Some(7));
    assert!(output.stdout == input);
    assert_eq!(
        output.stderr, b"\xff\x00err",
        "nothing but the command's stderr"
    );
    assert_eq!(
        json(&std::fs::read(&status).unwrap())["data"]["admission"],
        "off"
    );

    let output = elsecall(
        home.path(),
        repo.path(),
        &[
            ("ELSECALL", "off"),
            ("ELSECALL_RUN_ID", "r-outer0000000"),
            ("ELSECALL_ADMISSION", "granted"),
        ],
        &[
            "run",
            "--",
            "/bin/sh",
            "-c",
            "printf %s \"$ELSECALL_RUN_ID $ELSECALL_ADMISSION\"; kill -TERM $$",
        ],
        b"",
    );
    assert_eq!(
        output.stdout, b"r-outer0000000 off",
        "the environment is untouched but for the admission marker"
    );
    assert_eq!(
        output.status.signal(),
        Some(libc::SIGTERM),
        "dies as the command did"
    );

    let output = elsecall(
        home.path(),
        repo.path(),
        &off,
        &["lease", "acquire", "--size", "4"],
        b"",
    );
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty());
    assert_eq!(json(&output.stdout)["data"]["state"], "off");
    assert_eq!(json(&output.stdout)["data"]["run_id"], Value::Null);
    let output = elsecall(home.path(), repo.path(), &off, &["lease", "release"], b"");
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        json(&output.stdout)["data"],
        serde_json::json!({"state": "off", "released": []})
    );

    // Off still checks what the consumer wrote, so its tests catch mistakes.
    let output = elsecall(
        home.path(),
        repo.path(),
        &off,
        &["run", "--size", "lots", "--", "true"],
        b"",
    );
    assert_eq!(output.status.code(), Some(125));
    let output = elsecall(
        home.path(),
        repo.path(),
        &[("ELSECALL", "0")],
        &["run", "--", "true"],
        b"",
    );
    assert_eq!(output.status.code(), Some(125));
    let lines = stderr_lines(&output);
    assert!(
        lines[0].starts_with(
            "elsecall: invalid_request: The environment variable ELSECALL has an unsupported value."
        ),
        "{lines:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_holder_that_acquires_again_gets_its_lease_back_without_queueing() {
    let mut fx = Fixture::start(1).await;
    let script: PathBuf = fx.path("retry.sh");
    std::fs::write(
        &script,
        r#"if [ -z "$AGAIN" ]; then
  "$EC" lease acquire --label retry > "$DIR/first.json" || exit 9
  while [ ! -f "$DIR/reexec" ]; do sleep 0.02; done
  AGAIN=1 exec /bin/sh "$0"
fi
"$EC" lease acquire --label retry > "$DIR/second.json" || exit 8
while [ ! -f "$DIR/go" ]; do sleep 0.02; done
"$EC" lease release > "$DIR/released.json" || exit 7
while [ ! -f "$DIR/end" ]; do sleep 0.02; done
"#,
    )
    .unwrap();
    let retry = fx.script(r#"exec /bin/sh "$DIR/retry.sh""#);
    let first = json(fx.wait_for_file("first.json").await.as_bytes());
    // Another request queues for the only core.
    let outsider = fx.script(
        r#""$EC" lease acquire --label outsider >/dev/null || exit 9
while [ ! -f "$DIR/end" ]; do sleep 0.02; done"#,
    );
    fx.until(|r| queued(r, "interactive") == 1).await;

    // After the re-exec, the same holder acquires again: it gets its lease back
    // at once, ahead of the queued outsider, and nothing new is recorded.
    fx.touch("reexec");
    let second = json(fx.wait_for_file("second.json").await.as_bytes());
    assert_eq!(first["data"]["state"], "granted");
    assert_eq!(second["data"]["state"], "reused");
    assert_eq!(second["data"]["run_id"], first["data"]["run_id"]);
    assert_eq!(second["data"]["waited_ms"], 0);
    let runs = fx.runs().await;
    assert_eq!(in_use(&runs), 1.0);
    assert_eq!(queued(&runs, "interactive"), 1, "the outsider still waits");
    let retries = runs["runs"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["label"] == "retry")
        .count();
    assert_eq!(retries, 1, "acquiring again records nothing new");
    let id = first["data"]["run_id"].as_str().unwrap();
    assert_eq!(explain(&fx, id).await["reacquired"], 1);

    // One release ends the lease, and the outsider starts.
    fx.touch("go");
    let released = json(fx.wait_for_file("released.json").await.as_bytes());
    assert_eq!(released["data"]["released"], serde_json::json!([id]));
    let runs = fx
        .until(|r| record(r, "outsider")["state"] == "running")
        .await;
    assert_eq!(record(&runs, "retry")["outcome"]["kind"], "released");
    fx.touch("end");
    assert_eq!(fx.exit_code(retry).await, 0);
    assert_eq!(fx.exit_code(outsider).await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failures_write_one_agent_ready_line_before_the_envelope() {
    let mut fx = Fixture::start(1).await;
    let repo = repository("");
    fx.script(
        r#""$EC" lease acquire --label holding > "$DIR/holding.json" || exit 9
while [ ! -f "$DIR/release" ]; do sleep 0.02; done"#,
    );
    fx.wait_for_file("holding.json").await;
    let id_file = repo.path().join("run-id");

    let started = Instant::now();
    let output = elsecall_async(
        fx.home.path(),
        repo.path(),
        &[],
        &[
            "run",
            "--label",
            "waiter",
            "--hold-limit",
            "1s",
            "--run-id-file",
            id_file.to_str().unwrap(),
            "--",
            "true",
        ],
    )
    .await;
    assert!(started.elapsed() >= Duration::from_secs(1));
    assert_eq!(output.status.code(), Some(125));
    let lines = stderr_lines(&output);
    assert_eq!(lines.len(), 2, "{lines:?}");
    let envelope = json(lines[1].as_bytes());
    let error = &envelope["error"];
    assert_eq!(error["code"], "fleet_busy");
    let run_id = error["run_id"].as_str().unwrap();
    let retry = error["details"]["retry_after_seconds"].as_u64().unwrap();
    assert_eq!(
        lines[0],
        format!(
            "elsecall: fleet_busy: {} Wait about {retry} s, then rerun the same command once. `elsecall runs` shows what is running and queued; `elsecall explain {run_id}` shows why.",
            error["message"].as_str().unwrap()
        )
    );
    assert_eq!(
        read_id(&id_file),
        format!("{run_id}\n"),
        "the failed request has a record"
    );

    // lease acquire writes the same line on stderr; its envelope is the JSON on stdout.
    let output = elsecall_async(
        fx.home.path(),
        repo.path(),
        &[],
        &[
            "lease",
            "acquire",
            "--label",
            "waiter",
            "--hold-limit",
            "1s",
        ],
    )
    .await;
    assert_eq!(output.status.code(), Some(1));
    let error = json(&output.stdout)["error"].clone();
    let lines = stderr_lines(&output);
    assert_eq!(lines.len(), 1);
    assert!(
        lines[0].starts_with("elsecall: fleet_busy: No capacity within the 1 s hold limit")
            && lines[0].ends_with(&format!(
                "`elsecall explain {}` shows why.",
                error["run_id"].as_str().unwrap()
            )),
        "{}",
        lines[0]
    );

    // Other codes follow the same form.
    let output = elsecall_async(
        fx.home.path(),
        repo.path(),
        &[],
        &["run", "--size", "2", "--", "true"],
    )
    .await;
    assert_eq!(output.status.code(), Some(125));
    let lines = stderr_lines(&output);
    assert_eq!(lines.len(), 2);
    assert!(
        lines[0].starts_with("elsecall: request_too_large: Size 2 is larger than this member's capacity of 1 cores. Lower --size"),
        "{}",
        lines[0]
    );
    assert_eq!(
        json(lines[1].as_bytes())["error"]["code"],
        "request_too_large"
    );
    let output = elsecall_async(
        fx.home.path(),
        repo.path(),
        &[],
        &["run", "--sise", "2", "--", "true"],
    )
    .await;
    let lines = stderr_lines(&output);
    assert_eq!(lines.len(), 2);
    assert!(lines[0].starts_with("elsecall: invalid_request: Invalid run options;"));
    let output = elsecall_async(
        fx.home.path(),
        repo.path(),
        &[],
        &["lease", "acquire", "--sise", "2"],
    )
    .await;
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        stderr_lines(&output),
        [
            "elsecall: invalid_arguments: Invalid lease acquire options. Run elsecall lease acquire --help for valid options, then rerun the corrected command."
        ]
    );
    fx.touch("release");
}
