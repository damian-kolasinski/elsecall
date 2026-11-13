#![cfg(target_os = "macos")]
//! `elsecall run` through the real CLI and service: the command must behave as if
//! run directly, and Elsecall's own failures must be distinguishable.
mod processes;
mod support;

use serde_json::Value;
use std::{
    io::Write,
    os::unix::process::ExitStatusExt,
    process::{Command, Output, Stdio},
    time::{Duration, Instant},
};
use support::{Fixture, in_use, record};

fn elsecall(home: &std::path::Path) -> Command {
    let mut command = crate::processes::command(env!("CARGO_BIN_EXE_elsecall"));
    command
        .env("HOME", home)
        .current_dir(home)
        .env("DIR", home)
        .env("EC", env!("CARGO_BIN_EXE_elsecall"));
    command
}

/// Runs `elsecall run ARGS` with `input` on stdin, capturing stdout and stderr.
fn run(home: &std::path::Path, args: &[&str], input: &[u8]) -> Output {
    run_with(home, &[], args, input)
}

fn run_with(home: &std::path::Path, env: &[(&str, &str)], args: &[&str], input: &[u8]) -> Output {
    let mut child = elsecall(home)
        .envs(env.iter().copied())
        .arg("run")
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

/// The last stderr line, which must be Elsecall's envelope.
fn envelope(output: &Output) -> Value {
    let text = String::from_utf8_lossy(&output.stderr);
    let last = text.lines().last().expect("stderr has a line");
    serde_json::from_str(last).unwrap_or_else(|_| panic!("not an envelope: {text}"))
}

fn status(fx: &Fixture) -> Value {
    serde_json::from_slice(&std::fs::read(fx.path("status.json")).unwrap()).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_streams_pass_through_byte_for_byte_and_separately() {
    let fx = Fixture::start(2).await;
    let home = fx.home.path().to_owned();
    let input: Vec<u8> = (0..300_000u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect();
    let output = tokio::task::spawn_blocking(move || {
        run(
            &home,
            &["--", "/bin/sh", "-c", r"cat; printf '\377\000\200err' >&2"],
            &input,
        )
    })
    .await
    .unwrap();
    assert!(output.status.success(), "{output:?}");
    let expected: Vec<u8> = (0..300_000u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect();
    assert!(output.stdout == expected, "stdout differs");
    assert_eq!(output.stderr, b"\xff\x00\x80err", "no Elsecall output");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn merged_streams_keep_their_relative_order() {
    let fx = Fixture::start(2).await;
    let status = crate::processes::command("/bin/sh")
        .arg("-c")
        .arg(
            r#""$EC" run -- /bin/sh -c 'echo out1; echo err1 >&2; echo out2; echo err2 >&2' > "$DIR/merged" 2>&1"#,
        )
        .env("HOME", fx.home.path())
        .env("DIR", fx.home.path())
        .env("EC", env!("CARGO_BIN_EXE_elsecall"))
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(
        std::fs::read_to_string(fx.path("merged")).unwrap(),
        "out1\nerr1\nout2\nerr2\n"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exit_codes_pass_through_and_the_status_file_tells_125s_apart() {
    let mut fx = Fixture::start(1).await;
    let home = fx.home.path().to_owned();
    let status_file = fx.path("status.json");
    let status_arg = status_file.to_str().unwrap().to_owned();

    let output = run(&home, &["--", "/bin/sh", "-c", "exit 3"], b"");
    assert_eq!(output.status.code(), Some(3));
    assert!(output.stderr.is_empty() && output.stdout.is_empty());

    // The command's own 125.
    let output = run(
        &home,
        &[
            "--status-file",
            &status_arg,
            "--",
            "/bin/sh",
            "-c",
            "exit 125",
        ],
        b"",
    );
    assert_eq!(output.status.code(), Some(125));
    assert!(output.stderr.is_empty());
    let own = status(&fx);
    assert_eq!(own["data"]["state"], "exited");
    assert_eq!(own["data"]["exit_code"], 125);
    assert!(own.get("error").is_none());
    let run_id = own["data"]["run_id"].as_str().unwrap();
    let (code, explained) = fx.cli(&["explain", run_id]).await;
    assert_eq!(code, 0);
    let record = &explained["data"];
    assert!(record["waited_ms"].is_u64());
    let phases = &record["run_phases"];
    assert!(
        phases["execution_ms"].as_u64().unwrap()
            <= phases["request_to_result_ms"].as_u64().unwrap()
    );
    assert!(phases["result_return_ms"].is_null());
    assert!(record["placed"].is_null());

    // Elsecall's 125: no capacity within the hold limit; the command never runs.
    fx.script(
        r#""$EC" lease acquire >/dev/null; echo held > "$DIR/held"; while [ ! -f "$DIR/release" ]; do sleep 0.02; done"#,
    );
    fx.wait_for_file("held").await;
    let output = run(
        &home,
        &[
            "--status-file",
            &status_arg,
            "--hold-limit",
            "1s",
            "--",
            "/bin/sh",
            "-c",
            "touch \"$DIR/ran\"",
        ],
        b"",
    );
    assert_eq!(output.status.code(), Some(125));
    assert!(output.stdout.is_empty());
    let error = envelope(&output);
    assert_eq!(error["schema_version"], 1);
    let last_line = String::from_utf8_lossy(&output.stderr);
    assert!(
        last_line
            .lines()
            .last()
            .unwrap()
            .starts_with(r#"{"schema_version":1,"error":{"code":"fleet_busy""#),
        "the envelope keeps the contract's key order"
    );
    assert_eq!(error["error"]["code"], "fleet_busy");
    assert!(error["error"]["run_id"].is_string());
    assert!(!fx.path("ran").exists());
    let theirs = status(&fx);
    assert_eq!(theirs["error"], error["error"]);
    assert!(theirs.get("data").is_none());
    fx.touch("release");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_arguments_and_unknown_commands_exit_125_without_running() {
    let fx = Fixture::start(1).await;
    let home = fx.home.path();
    for args in [
        &["--size", "lots", "--", "true"][..],
        &["--bogus-option", "--", "true"],
        &[],
        &["--deferrable", "--", "true"],
        &["--", "elsecall-no-such-command"],
        &["--member", "another-machine", "--", "true"],
    ] {
        let output = run(home, args, b"");
        assert_eq!(output.status.code(), Some(125), "{args:?}");
        assert!(output.stdout.is_empty());
        let code = envelope(&output)["error"]["code"].clone();
        let expected = if args.contains(&"another-machine") {
            "member_unavailable"
        } else {
            "invalid_request"
        };
        assert_eq!(code, expected, "{args:?}");
        assert!(
            !String::from_utf8_lossy(&output.stderr).contains("lots"),
            "argument values are not echoed"
        );
    }
    let runs = fx.runs().await;
    assert_eq!(
        runs["runs"].as_array().unwrap().len(),
        1,
        "only the member_unavailable request reached admission"
    );
}

#[test]
fn service_down_fails_with_125_and_never_runs_the_command() {
    let home = tempfile::Builder::new()
        .prefix("ec-")
        .tempdir_in("/tmp")
        .unwrap();
    let status_file = home.path().join("status.json");
    let output = run(
        home.path(),
        &[
            "--status-file",
            status_file.to_str().unwrap(),
            "--",
            "/bin/sh",
            "-c",
            "touch \"$DIR/ran\"",
        ],
        b"",
    );
    assert_eq!(output.status.code(), Some(125));
    let error = envelope(&output);
    assert_eq!(error["error"]["code"], "service_unavailable");
    assert_eq!(error["error"]["run_id"], Value::Null);
    assert!(!home.path().join("ran").exists());
    let status: Value = serde_json::from_slice(&std::fs::read(status_file).unwrap()).unwrap();
    assert_eq!(status["error"]["code"], "service_unavailable");
}

/// A child killed when the test ends, even by a failed assertion, so it never
/// keeps the test's output open.
struct Running(std::process::Child);
impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Starts `elsecall run` with a command that reports readiness and how it ended.
fn signal_target(fx: &Fixture, args: &[&str]) -> std::process::Child {
    elsecall(fx.home.path())
        .arg("run")
        .args(args)
        .stdin(Stdio::null())
        .spawn()
        .unwrap()
}

fn wait(child: &mut std::process::Child) -> std::process::ExitStatus {
    let deadline = Instant::now() + crate::processes::WAIT;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        assert!(Instant::now() < deadline, "elsecall did not exit");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupt_and_termination_are_forwarded_to_the_command() {
    let fx = Fixture::start(1).await;
    for (signal, name, code) in [(libc::SIGINT, "INT", 7), (libc::SIGTERM, "TERM", 8)] {
        let ready = format!("ready-{name}");
        let script = format!(
            r#"trap 'echo {name} > "$DIR/got"; exit {code}' {name}; echo up > "$DIR/{ready}"; while :; do sleep 0.02; done"#
        );
        let mut child = signal_target(&fx, &["--", "/bin/sh", "-c", &script]);
        fx.wait_for_file(&ready).await;
        // SAFETY: kill has no memory-safety preconditions.
        unsafe { libc::kill(child.id() as i32, signal) };
        let status = wait(&mut child);
        assert_eq!(status.code(), Some(code), "{name}");
        assert_eq!(fx.wait_for_file("got").await, format!("{name}\n"));
        std::fs::remove_file(fx.path("got")).unwrap();
    }
    // A command that dies of the signal makes Elsecall die of it too.
    let mut child = signal_target(
        &fx,
        &[
            "--",
            "/bin/sh",
            "-c",
            r#"echo up > "$DIR/ready"; exec sleep 30"#,
        ],
    );
    fx.wait_for_file("ready").await;
    // SAFETY: as above.
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    assert_eq!(wait(&mut child).signal(), Some(libc::SIGTERM));
    let runs = fx.until(|r| in_use(r) == 0.0).await;
    let latest = &runs["runs"][0];
    assert_eq!(latest["outcome"]["kind"], "signaled");
    assert_eq!(latest["outcome"]["signal"], libc::SIGTERM);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runs_set_variables_nest_and_outlive_a_killed_caller() {
    let mut fx = Fixture::start(1).await;
    let home = fx.home.path().to_owned();
    let secret_argument = "argument-s3cr3t";
    let secret_value = "environment-s3cr3t";
    let output = run_with(
        &home,
        &[("SECRET_TOKEN", secret_value)],
        &[
            "--placeable",
            "--label",
            "outer",
            "--",
            "/bin/sh",
            "-c",
            r#"echo "$ELSECALL_RUN_ID $ELSECALL_MEMBER"; "$EC" run --label inner -- /bin/sh -c 'echo "$ELSECALL_RUN_ID"' "$1""#,
            "sh",
            secret_argument,
        ],
        b"",
    );
    assert!(output.status.success(), "{output:?}");
    let text = String::from_utf8(output.stdout).unwrap();
    let mut lines = text.lines();
    let (outer_id, member) = lines.next().unwrap().split_once(' ').unwrap();
    let inner_id = lines.next().unwrap();
    assert_eq!(member, "test-member");
    assert_ne!(outer_id, inner_id);
    let (_, outer) = fx.cli(&["explain", outer_id]).await;
    assert_eq!(outer["data"]["outcome"]["kind"], "exited");
    assert_eq!(outer["data"]["outcome"]["exit_code"], 0);
    assert_eq!(
        outer["data"]["placement_note"],
        "This member is in no fleet with other members, so placeable work runs on it."
    );
    let (_, inner) = fx.cli(&["explain", inner_id]).await;
    assert_eq!(
        inner["data"]["nested_in"], outer_id,
        "capacity 1 yet no queueing"
    );

    // SIGKILL of the caller: the grant lasts as long as the command does.
    // The script execs elsecall, so its PID is the caller's.
    let caller = fx.script(
        r#"exec "$EC" run --label orphan -- /bin/sh -c 'echo up > "$DIR/up"; while [ ! -f "$DIR/finish" ]; do sleep 0.02; done'"#,
    );
    fx.wait_for_file("up").await;
    let runs = fx.runs().await;
    let orphan = record(&runs, "orphan")["run_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let deadline = Instant::now() + crate::processes::WAIT;
    while fx.cli(&["explain", &orphan]).await.1["data"]["processes"]
        .as_array()
        .unwrap()
        .len()
        < 2
    {
        assert!(Instant::now() < deadline, "the command was never bound");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // SAFETY: kill has no memory-safety preconditions.
    unsafe { libc::kill(caller as i32, libc::SIGKILL) };
    tokio::time::sleep(Duration::from_millis(300)).await;
    let runs = fx.runs().await;
    assert_eq!(in_use(&runs), 1.0, "the orphaned command still runs");
    assert_eq!(record(&runs, "orphan")["state"], "running");
    fx.touch("finish");
    let runs = fx.until(|r| in_use(r) == 0.0).await;
    assert_eq!(record(&runs, "orphan")["outcome"]["kind"], "holder_exited");

    let records = fx.paths.records();
    let wal = records.with_extension("sqlite-wal");
    let bytes = support::read_all(&[&records, &wal]);
    for leaked in [secret_argument, secret_value, "ELSECALL_RUN_ID", "/bin/sh"] {
        assert!(
            !bytes.windows(leaked.len()).any(|w| w == leaked.as_bytes()),
            "records hold no arguments: {leaked}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_files_track_progress_and_usage_errors_replace_stale_ones() {
    let fx = Fixture::start(1).await;
    let home = fx.home.path().to_owned();
    let status_arg = fx.path("status.json").to_str().unwrap().to_owned();
    std::fs::write(
        fx.path("status.json"),
        r#"{"schema_version":1,"data":{"state":"exited","exit_code":125}}"#,
    )
    .unwrap();
    let output = run(
        &home,
        &["--status-file", &status_arg, "--sise", "2", "--", "true"],
        b"",
    );
    assert_eq!(output.status.code(), Some(125));
    assert_eq!(status(&fx)["error"]["code"], "invalid_request");

    let mut child = Running(
        elsecall(&home)
            .args(["run", "--status-file", &status_arg, "--", "/bin/sh", "-c"])
            .arg(r#"while [ ! -f "$DIR/finish" ]; do sleep 0.02; done"#)
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + crate::processes::WAIT;
    let running = loop {
        let current = status(&fx);
        if current["data"]["state"] == "running" {
            break current;
        }
        assert!(Instant::now() < deadline, "never running: {current}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert!(running["data"]["run_id"].is_string());
    fx.touch("finish");
    assert!(wait(&mut child.0).success());
    assert_eq!(status(&fx)["data"]["state"], "exited");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ignored_signals_stay_ignored_and_core_dumping_signals_exit_128_plus_n() {
    let fx = Fixture::start(1).await;
    // A non-interactive shell's background job ignores SIGINT; so must the command.
    let status = crate::processes::command("/bin/sh")
        .arg("-c")
        .arg(
            r#""$EC" run -- /bin/sh -c 'echo up > "$DIR/up"; while [ ! -f "$DIR/finish" ]; do sleep 0.02; done; echo survived > "$DIR/survived"' &
pid=$!
while [ ! -f "$DIR/up" ]; do sleep 0.02; done
kill -INT $pid
sleep 0.3
touch "$DIR/finish"
wait $pid"#,
        )
        .env("HOME", fx.home.path())
        .env("DIR", fx.home.path())
        .env("EC", env!("CARGO_BIN_EXE_elsecall"))
        .status()
        .unwrap();
    assert!(status.success(), "{status:?}");
    assert_eq!(fx.wait_for_file("survived").await, "survived\n");

    let output = run(
        fx.home.path(),
        &["--", "/bin/sh", "-c", "kill -SEGV $$"],
        b"",
    );
    assert_eq!(output.status.code(), Some(128 + libc::SIGSEGV));
    assert!(output.stderr.is_empty());
}

/// The label's size once three runs measured how many cores it keeps busy: the
/// upper quartile of CPU time over run time, rounded up to half a core.
fn learned_size(records: &[Value]) -> f64 {
    let mut busy: Vec<f64> = records
        .iter()
        .map(|r| r["cpu_ms"].as_f64().unwrap() / r["run_ms"].as_f64().unwrap())
        .collect();
    busy.sort_by(f64::total_cmp);
    let upper = busy[(busy.len() * 3).div_ceil(4) - 1];
    ((upper * 1000.0).ceil() / 500.0).ceil().max(1.0) * 0.5
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_labels_size_is_learned_from_the_cpu_time_its_runs_used() {
    let fx = Fixture::start(8).await;
    let home = fx.home.path().to_owned();
    // Each waited child consumes a CPU budget, independent of how often the
    // scheduler runs it. The final sleep keeps every sample above the learning
    // minimum of one wall second, even on a fast or otherwise idle machine.
    let busy = r#"python3 -c 'import time
end = time.process_time() + 0.25
while time.process_time() < end: pass' & a=$!
python3 -c 'import time
end = time.process_time() + 0.25
while time.process_time() < end: pass' & b=$!
wait "$a" || exit
wait "$b" || exit
sleep 1.1; :"#;
    let mut ids = Vec::new();
    for _ in 0..4 {
        let home = home.clone();
        let output = tokio::task::spawn_blocking(move || {
            run(
                &home,
                &["--label", "two busy", "--", "/bin/sh", "-c", busy],
                b"",
            )
        })
        .await
        .unwrap();
        assert!(output.status.success(), "{output:?}");
        let runs = fx.runs().await;
        ids.push(
            record(&runs, "two busy")["run_id"]
                .as_str()
                .unwrap()
                .to_owned(),
        );
    }
    let mut explained = Vec::new();
    for id in &ids {
        let (code, json) = fx.cli(&["explain", id]).await;
        assert_eq!(code, 0, "{json}");
        explained.push(json["data"].clone());
    }
    for first in &explained[..3] {
        assert_eq!(first["size"], 1.0, "{first}");
        assert_eq!(first["size_basis"]["from"], "default");
        let cpu = first["cpu_ms"].as_u64().expect("CPU time was measured");
        assert!(cpu > 300, "two busy processes were measured: {first}");
    }
    let fourth = &explained[3];
    assert_eq!(fourth["size_basis"]["from"], "learned", "{fourth}");
    assert_eq!(fourth["size_basis"]["samples"], 3);
    assert_eq!(fourth["size"].as_f64(), Some(learned_size(&explained[..3])));
    // A declared size always wins.
    let output = tokio::task::spawn_blocking(move || {
        run(
            &home,
            &["--label", "two busy", "--size", "1", "--", "/usr/bin/true"],
            b"",
        )
    })
    .await
    .unwrap();
    assert!(output.status.success());
    let runs = fx.runs().await;
    let declared = record(&runs, "two busy");
    assert_eq!(declared["size"], 1.0);
    // The member reports its measured speed.
    assert_eq!(runs["member"]["speed"], 1.0, "{runs}");
}
