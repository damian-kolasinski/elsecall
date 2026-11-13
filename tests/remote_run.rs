#![cfg(target_os = "macos")]
//! `elsecall run --member` between real member services in separate temporary
//! homes, with loopback standing in for the tailnet: the command runs in the
//! member's copy of the caller's working tree and behaves as if it ran locally.
mod file_gate;
mod members;
mod processes;
mod run_fragmentation;
mod sealed_fragmentation;
mod sync_gate;

use file_gate::FileGate as ReleaseGate;
use members::{TestMember, enrol};
use serde_json::Value;
use std::{
    io::{BufRead, BufReader, Read, Write},
    os::unix::process::ExitStatusExt,
    path::{Path, PathBuf},
    process::{Child, Output, Stdio},
    time::{Duration, Instant},
};

fn git(dir: &Path, args: &[&str]) -> String {
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
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

/// A caller repository with a commit and uncommitted changes.
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
    std::fs::create_dir_all(root.join("src/deep")).unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    std::fs::write(root.join(".gitignore"), "build/\n").unwrap();
    std::fs::write(root.join("README.md"), "hello\n").unwrap();
    std::fs::write(root.join("src/lib.txt"), "one\n").unwrap();
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-q", "-m", "initial"]);
    std::fs::write(root.join("README.md"), "hello\nstaged\n").unwrap();
    git(&root, &["add", "README.md"]);
    std::fs::write(root.join("src/lib.txt"), "one\nunstaged\n").unwrap();
    std::fs::write(root.join("notes.txt"), "untracked\n").unwrap();
    std::fs::create_dir_all(root.join("build")).unwrap();
    std::fs::write(root.join("build/secret.o"), "ignored").unwrap();
    Repository { _temp: temp, root }
}

/// `elsecall run ARGS` from `cwd` through `caller`'s service.
fn run(caller: &TestMember, cwd: &Path, args: &[&str], input: &[u8]) -> Output {
    let mut child = spawn(caller, cwd, args, &[]);
    let mut stdin = child.stdin.take().unwrap();
    let input = input.to_vec();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&input);
    });
    let output = child.wait_with_output().unwrap();
    writer.join().unwrap();
    output
}

fn spawn(caller: &TestMember, cwd: &Path, args: &[&str], env: &[(&str, &str)]) -> Child {
    crate::processes::command(env!("CARGO_BIN_EXE_elsecall"))
        .current_dir(cwd)
        .env("HOME", caller.home.path())
        .envs(env.iter().copied())
        .arg("run")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

fn envelope(output: &Output) -> Value {
    let text = String::from_utf8_lossy(&output.stderr);
    let last = text
        .lines()
        .last()
        .unwrap_or_else(|| panic!("no envelope: {output:?}"));
    serde_json::from_str(last).unwrap_or_else(|_| panic!("not an envelope: {text}"))
}

fn fleet() -> (TestMember, TestMember) {
    let alpha = TestMember::start("alpha");
    let bravo = TestMember::start("bravo");
    enrol(&alpha, &bravo);
    (alpha, bravo)
}

fn stdout(output: &Output) -> String {
    assert!(output.status.success(), "{output:?}");
    String::from_utf8(output.stdout.clone()).unwrap()
}

#[test]
fn a_sealed_record_can_progress_for_sixteen_seconds_without_releasing_unauthenticated_output() {
    use std::sync::{Arc, Mutex, atomic::Ordering};
    let (alpha, bravo) = fleet();
    let proxy = sealed_fragmentation::Proxy::start(&bravo, sealed_fragmentation::Fault::Slow);
    proxy.enroll(&alpha, &bravo);
    let repo = repository();
    let payload: Vec<u8> = (0..1048576).map(|n| n as u8).collect();
    std::fs::write(repo.root.join("payload.bin"), &payload).unwrap();
    let status_file = repo.root.parent().unwrap().join("status.json");
    let mut child = spawn(
        &alpha,
        &repo.root,
        &[
            "--member",
            "bravo",
            "--status-file",
            status_file.to_str().unwrap(),
            "--",
            "/bin/sh",
            "-c",
            "dd if=payload.bin bs=65536 2>/dev/null; printf '\\000\\377err' >&2; exit 7",
        ],
        &[],
    );
    drop(child.stdin.take());
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let observed = bytes.clone();
    let mut stdout = child.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut buffer = [0; 4096];
        while let Ok(n @ 1..) = stdout.read(&mut buffer) {
            observed.lock().unwrap().extend_from_slice(&buffer[..n]);
        }
    });
    let start = Instant::now();
    until(
        "the final record tag to be held",
        crate::processes::WAIT,
        || proxy.tag_held.load(Ordering::SeqCst) || child.try_wait().unwrap().is_some(),
    );
    if !proxy.tag_held.load(Ordering::SeqCst) {
        let output = child.wait_with_output().unwrap();
        reader.join().unwrap();
        panic!("CLI exited before the authentication-tag gate: {output:?}");
    }
    assert!(start.elapsed() >= Duration::from_secs(16));
    assert!(
        child.try_wait().unwrap().is_none(),
        "positive ciphertext progress must preserve the run"
    );
    // Every earlier ciphertext record may contribute at most its plaintext
    // length to stdout. The selected record is a full 64 KiB stdout payload;
    // releasing it before its tag would complete that frame and exceed this
    // prefix bound. Shorter, already authenticated stdout chunks are allowed.
    let prefix_bound = proxy.prefix_plaintext.load(Ordering::SeqCst);
    let observed_len = bytes.lock().unwrap().len();
    assert!(
        observed_len <= prefix_bound,
        "unauthenticated stdout record was released"
    );
    assert!(
        prefix_bound - observed_len < 65536,
        "fixture control overhead must be smaller than the gated payload"
    );
    proxy.release.store(true, Ordering::SeqCst);
    let output = child.wait_with_output().unwrap();
    reader.join().unwrap();
    assert_eq!(output.status.code(), Some(7), "{output:?}");
    assert_eq!(*bytes.lock().unwrap(), payload);
    assert_eq!(output.stderr, b"\0\xfferr");
    assert_eq!(proxy.attaches.load(Ordering::SeqCst), 1);
    let status = status_of(&status_file);
    let id = status["data"]["run_id"].as_str().unwrap();
    for member in [&alpha, &bravo] {
        until("the exit record", crate::processes::WAIT, || {
            explain(member, id)["outcome"]["exit_code"] == 7
        });
        let record = explain(member, id);
        assert_eq!(record["placed"]["exit_code"], 7, "{record}");
        assert!(
            record["run_phases"]["remote_failure_phase"].is_null(),
            "{record}"
        );
    }
    let phases = explain(&alpha, id)["run_phases"].clone();
    assert!(
        phases["execution_ms"].as_u64().unwrap() >= 16000,
        "{phases}"
    );
    assert!(phases["request_to_result_ms"].is_u64(), "{phases}");
}

#[test]
fn sealed_record_silence_eof_and_tampering_fail_without_payload_or_retry() {
    use sealed_fragmentation::{Fault, Proxy};
    use std::sync::atomic::Ordering;
    for (fault, kind, timeout) in [
        (Fault::PartialSilence, "timeout", true),
        (Fault::BetweenSilence, "timeout", true),
        (Fault::PartialEof, "receive", false),
        (Fault::BetweenEof, "receive", false),
        (Fault::Mac, "authentication", false),
        (Fault::Header, "authentication", false),
    ] {
        let (alpha, bravo) = fleet();
        let proxy = Proxy::start(&bravo, fault);
        proxy.enroll(&alpha, &bravo);
        let repo = repository();
        std::fs::write(repo.root.join("payload.bin"), vec![b'x'; 1048576]).unwrap();
        let status_file = repo.root.parent().unwrap().join("status.json");
        let pid_file = bravo.home.path().join("sealed-pid");
        let script = format!(
            "echo $$ > '{}'; dd if=payload.bin bs=65536 2>/dev/null; sleep 60",
            pid_file.display()
        );
        let mut child = spawn(
            &alpha,
            &repo.root,
            &[
                "--member",
                "bravo",
                "--status-file",
                status_file.to_str().unwrap(),
                "--",
                "/bin/sh",
                "-c",
                &script,
            ],
            &[],
        );
        drop(child.stdin.take());
        until(
            "the proxy to select a full stdout record",
            crate::processes::WAIT,
            || proxy.selected.load(Ordering::SeqCst),
        );
        let pid: i32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let start = Instant::now();
        let output = child.wait_with_output().unwrap();
        assert_eq!(output.status.code(), Some(125), "{output:?}");
        assert!(
            output.stdout.len() <= proxy.prefix_plaintext.load(Ordering::SeqCst),
            "unauthenticated stdout record: {} bytes",
            output.stdout.len()
        );
        assert!(
            start.elapsed() < Duration::from_secs(if timeout { 15 } else { 5 }),
            "late failure: {:?}",
            start.elapsed()
        );
        if timeout {
            assert!(start.elapsed() >= Duration::from_secs(9));
        }
        assert_eq!(proxy.attaches.load(Ordering::SeqCst), 1);
        let failure = envelope(&output);
        let id = failure["error"]["run_id"].as_str().unwrap();
        let caller = explain(&alpha, id);
        assert_eq!(
            caller["run_phases"]["remote_failure_phase"], "execution",
            "{caller}"
        );
        assert!(
            caller["peer_io"]["attempts"]
                .as_array()
                .unwrap()
                .iter()
                .any(|a| a["operation"] == "result_stream" && a["failure_kind"] == kind),
            "{caller}"
        );
        until("member cancellation", crate::processes::WAIT, || {
            explain(&bravo, id)["outcome"]["code"] == "connection_lost" && !alive(pid)
        });
    }
}

#[test]
fn a_slow_fragmented_result_frame_preserves_output_exit_and_phase_records() {
    let (alpha, bravo) = fleet();
    let proxy = run_fragmentation::RunFragmentation::start(&bravo);
    proxy.enroll(&alpha, &bravo);
    let repo = repository();
    let payload: Vec<u8> = (0..524288).map(|n| n as u8).collect();
    std::fs::write(repo.root.join("payload.bin"), &payload).unwrap();
    std::fs::write(
        repo.root.join(".elsecall.toml"),
        "[outputs]\npaths = [\"build/result.bin\"]\n",
    )
    .unwrap();
    let status_file = repo.root.parent().unwrap().join("status.json");
    let output = run(
        &alpha,
        &repo.root,
        &[
            "--member",
            "bravo",
            "--status-file",
            status_file.to_str().unwrap(),
            "--",
            "/bin/sh",
            "-c",
            "mkdir -p build; cp payload.bin build/result.bin; printf '\\377\\000out'; printf '\\000\\377\\200err' >&2; exit 7",
        ],
        b"",
    );
    assert_eq!(
        output.status.code(),
        Some(7),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"\xff\0out");
    assert_eq!(output.stderr, b"\0\xff\x80err");
    let status = status_of(&status_file);
    assert_eq!(status["data"]["state"], "exited", "{status}");
    assert_eq!(status["data"]["exit_code"], 7);
    assert!(status["data"]["warnings"].is_null(), "{status}");
    assert_eq!(proxy.records(), 4);
    assert_eq!(
        std::fs::read(repo.root.join("build/result.bin")).unwrap(),
        payload
    );
    let id = status["data"]["run_id"].as_str().unwrap();
    for member in [&alpha, &bravo] {
        until(
            "the member to record the exit",
            crate::processes::WAIT,
            || explain(member, id)["outcome"]["exit_code"] == 7,
        );
        let record = explain(member, id);
        assert_eq!(record["placed"]["exit_code"], 7, "{record}");
        assert!(
            record["run_phases"]["remote_failure_phase"].is_null(),
            "{record}"
        );
    }
    let caller = explain(&alpha, id);
    // Successful phase durations belong to the caller that measured them.
    assert!(caller["run_phases"]["execution_ms"].is_u64(), "{caller}");
    assert!(explain(&bravo, id)["run_phases"].is_null());
    assert!(
        caller["run_phases"]["result_return_ms"].as_u64().unwrap() > 10000,
        "{caller}"
    );
    assert!(
        caller["run_phases"]["request_to_result_ms"].is_u64(),
        "{caller}"
    );
}

#[test]
fn output_backpressure_longer_than_the_grace_period_preserves_both_streams() {
    let (alpha, bravo) = fleet();
    let repo = repository();
    let mut child = spawn(
        &alpha,
        &repo.root,
        &[
            "--member",
            "bravo",
            "--",
            "/bin/sh",
            "-c",
            "echo $ELSECALL_RUN_ID; head -c 2097152 /dev/zero; head -c 2097152 /dev/zero >&2; exit 7",
        ],
        &[],
    );
    let (id, mut reader) = first_line(&mut child);
    std::thread::sleep(Duration::from_secs(12));
    let backpressured = child.try_wait().unwrap().is_none();
    let stdout = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        bytes
    });
    let output = child.wait_with_output().unwrap();
    assert!(backpressured, "unread output must keep the command blocked");
    assert_eq!(output.status.code(), Some(7));
    assert_eq!(stdout.join().unwrap(), vec![0; 2097152]);
    assert_eq!(output.stderr, vec![0; 2097152]);
    for member in [&alpha, &bravo] {
        until(
            "the member to record the exit",
            crate::processes::WAIT,
            || explain(member, &id)["outcome"]["exit_code"] == 7,
        );
        let record = explain(member, &id);
        assert!(
            record["run_phases"]["remote_failure_phase"].is_null(),
            "{record}"
        );
    }
}

#[test]
fn a_placed_command_runs_in_the_members_copy_of_the_dirty_tree() {
    let (alpha, bravo) = fleet();
    let repo = repository();
    let local = git(
        &repo.root.join("src/deep"),
        &[
            "status",
            "--porcelain=v2",
            "--branch",
            "--untracked-files=all",
        ],
    );
    let output = run(
        &alpha,
        &repo.root.join("src/deep"),
        &[
            "--member",
            "bravo",
            "--",
            "/bin/sh",
            "-c",
            "git status --porcelain=v2 --branch --untracked-files=all; pwd -P >&2; echo \"$ELSECALL_MEMBER\" >&2; test ! -e ../../build/secret.o",
        ],
        b"",
    );
    let out = stdout(&output);
    assert_eq!(out, local, "git sees the same tree on the member");
    let err = String::from_utf8_lossy(&output.stderr);
    let mut lines = err.lines();
    let cwd = lines.next().unwrap();
    assert!(
        cwd.starts_with(bravo.home.path().canonicalize().unwrap().to_str().unwrap()),
        "{cwd}"
    );
    assert!(
        cwd.ends_with("/src/deep"),
        "the same relative directory: {cwd}"
    );
    assert_eq!(lines.next(), Some("bravo"));
}

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

fn until(what: &str, limit: Duration, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + limit;
    while !condition() {
        assert!(
            Instant::now() < deadline,
            "{what} did not happen within {limit:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Reads the first line of a running command's stdout.
fn first_line(child: &mut Child) -> (String, BufReader<std::process::ChildStdout>) {
    let stdout = child.stdout.take().unwrap();
    next_line(child, BufReader::new(stdout))
}

fn next_line(
    child: &mut Child,
    mut reader: BufReader<std::process::ChildStdout>,
) -> (String, BufReader<std::process::ChildStdout>) {
    let (sent, received) = std::sync::mpsc::channel();
    let reading = std::thread::spawn(move || {
        let mut line = String::new();
        let result = reader.read_line(&mut line).map(|_| (line, reader));
        let _ = sent.send(result);
    });
    let result = match received.recv_timeout(crate::processes::WAIT) {
        Ok(Ok((line, reader))) => (line.trim().to_owned(), reader),
        _ => {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the command never wrote its first line");
        }
    };
    reading.join().unwrap();
    result
}

fn wait_child(child: &mut Child) {
    let deadline = Instant::now() + crate::processes::WAIT;
    loop {
        if child.try_wait().unwrap().is_some() {
            return;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the command did not exit");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The record of `run_id` as `member` keeps it.
fn explain(member: &TestMember, run_id: &str) -> Value {
    member.ok(&["explain", run_id])
}

#[test]
fn streams_exit_codes_and_failures_pass_through_exactly() {
    let (alpha, _bravo) = fleet();
    let repo = repository();
    let input: Vec<u8> = (0..300_000u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect();
    let output = run(
        &alpha,
        &repo.root,
        &[
            "--member",
            "bravo",
            "--",
            "/bin/sh",
            "-c",
            r"cat; printf '\377\000\200err' >&2",
        ],
        &input,
    );
    assert!(
        output.status.success(),
        "{:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout == input, "stdout differs");
    assert_eq!(
        output.stderr, b"\xff\x00\x80err",
        "stderr is separate and exact"
    );

    // One destination for both streams keeps their order exactly.
    let merged = tempfile::NamedTempFile::new().unwrap();
    let file = merged.reopen().unwrap();
    let status = crate::processes::command(env!("CARGO_BIN_EXE_elsecall"))
        .current_dir(&repo.root)
        .env("HOME", alpha.home.path())
        .args(["run", "--member", "bravo", "--", "/bin/sh", "-c"])
        .arg("i=0; while [ $i -lt 300 ]; do echo out$i; echo err$i >&2; i=$((i+1)); done")
        .stdin(Stdio::null())
        .stdout(Stdio::from(file.try_clone().unwrap()))
        .stderr(Stdio::from(file))
        .status()
        .unwrap();
    assert!(status.success());
    let expected: String = (0..300).map(|i| format!("out{i}\nerr{i}\n")).collect();
    assert_eq!(std::fs::read_to_string(merged.path()).unwrap(), expected);

    // Exit codes pass through; the status file tells the command's 125 apart.
    let status_file = repo.root.parent().unwrap().join("status.json");
    let status_path = status_file.to_str().unwrap();
    let output = run(
        &alpha,
        &repo.root,
        &[
            "--member",
            "bravo",
            "--status-file",
            status_path,
            "--",
            "/bin/sh",
            "-c",
            "exit 125",
        ],
        b"",
    );
    assert_eq!(output.status.code(), Some(125));
    assert!(
        output.stderr.is_empty(),
        "no envelope for the command's own 125"
    );
    let status: Value = serde_json::from_slice(&std::fs::read(&status_file).unwrap()).unwrap();
    assert_eq!(status["data"]["state"], "exited");
    assert_eq!(status["data"]["exit_code"], 125);
    assert_eq!(status["data"]["member"], "bravo");
    let output = run(
        &alpha,
        &repo.root,
        &["--member", "bravo", "--", "/bin/sh", "-c", "exit 7"],
        b"",
    );
    assert_eq!(output.status.code(), Some(7));

    // Elsecall's own failures exit 125 with the envelope, and the status file says so.
    let output = run(
        &alpha,
        &repo.root,
        &[
            "--member",
            "nobody",
            "--status-file",
            status_path,
            "--",
            "true",
        ],
        b"",
    );
    assert_eq!(output.status.code(), Some(125));
    assert_eq!(envelope(&output)["error"]["code"], "member_unavailable");
    let status: Value = serde_json::from_slice(&std::fs::read(&status_file).unwrap()).unwrap();
    assert_eq!(status["error"]["code"], "member_unavailable");
    let output = run(
        &alpha,
        &repo.root,
        &["--member", "bravo", "--", "elsecall-no-such-command"],
        b"",
    );
    assert_eq!(output.status.code(), Some(125));
    assert_eq!(envelope(&output)["error"]["code"], "invalid_request");
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains("elsecall-no-such-command"),
        "arguments are never echoed"
    );
}

#[test]
fn signals_reach_the_command_on_the_member() {
    let (alpha, _bravo) = fleet();
    let repo = repository();
    let mut child = spawn(
        &alpha,
        &repo.root,
        &[
            "--member",
            "bravo",
            "--",
            "/bin/sh",
            "-c",
            "trap 'echo trapped; exit 42' INT; echo ready; while :; do sleep 0.1; done",
        ],
        &[],
    );
    let (line, mut rest) = first_line(&mut child);
    assert_eq!(line, "ready");
    // SAFETY: kill has no memory-safety preconditions.
    unsafe { libc::kill(child.id() as i32, libc::SIGINT) };
    let status = child.wait().unwrap();
    let mut remaining = String::new();
    rest.read_to_string(&mut remaining).unwrap();
    assert_eq!(remaining, "trapped\n");
    assert_eq!(status.code(), Some(42));

    // An untrapped termination ends the command, and Elsecall dies of it too.
    let mut child = spawn(
        &alpha,
        &repo.root,
        &[
            "--member",
            "bravo",
            "--",
            "/bin/sh",
            "-c",
            "echo ready; exec sleep 30",
        ],
        &[],
    );
    let (line, _) = first_line(&mut child);
    assert_eq!(line, "ready");
    // SAFETY: as above.
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    let status = child.wait().unwrap();
    assert_eq!(status.signal(), Some(libc::SIGTERM), "{status:?}");
}

fn environment_of(output: &Output) -> std::collections::HashMap<String, String> {
    stdout(output)
        .split('\0')
        .filter_map(|pair| pair.split_once('='))
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect()
}

#[test]
fn placed_work_gets_the_allowlist_the_members_identity_and_its_own_variables() {
    let alpha = TestMember::start("alpha");
    let bravo = TestMember::start_with("bravo", "", &[("TMPDIR", "/tmp/bravo-tmp/")]);
    let base = tempfile::Builder::new()
        .prefix("ec-same-")
        .tempdir_in("/tmp")
        .unwrap();
    let base_path = base.path().canonicalize().unwrap();
    let charlie = TestMember::start_with(
        "charlie",
        "[workspace]\nlocation = \"same-path\"\n[placed_runs]\npath = \"/usr/bin:/bin:/usr/sbin\"\n",
        &[("ELSECALL_SAME_PATH_BASE", base_path.to_str().unwrap())],
    );
    enrol(&alpha, &bravo);
    enrol(&alpha, &charlie);
    let repo = repository();
    std::fs::write(
        repo.root.join(".elsecall.toml"),
        "[environment]\nnames = [\"CI\", \"HOME\"]\nprefixes = [\"APP_\"]\n",
    )
    .unwrap();
    let caller_env = [
        ("CI", "1"),
        ("APP_TOKEN", "t"),
        ("SECRET", "s"),
        ("ELSECALL_MEMBER", "forged"),
    ];
    let run_env = |member: &str| {
        let mut child = spawn(
            &alpha,
            &repo.root.join("src"),
            &["--member", member, "--", "/usr/bin/env", "-0"],
            &caller_env,
        );
        drop(child.stdin.take());
        child.wait_with_output().unwrap()
    };
    let output = run_env("bravo");
    let env = environment_of(&output);
    assert_eq!(env.get("CI").map(String::as_str), Some("1"));
    assert_eq!(env.get("APP_TOKEN").map(String::as_str), Some("t"));
    assert!(!env.contains_key("SECRET"), "not on the allowlist");
    assert_eq!(
        env["HOME"],
        bravo.home.path().to_str().unwrap(),
        "identity comes from the member"
    );
    assert_eq!(env["TMPDIR"], "/tmp/bravo-tmp/");
    assert_eq!(env["ELSECALL_MEMBER"], "bravo");
    assert_eq!(env["ELSECALL_CALLER_MEMBER"], "alpha");
    assert_eq!(
        env["ELSECALL_CALLER_WORKSPACE"],
        repo.root.to_str().unwrap()
    );
    assert!(env["ELSECALL_RUN_ID"].starts_with("r-"));
    let copy = PathBuf::from(&env["ELSECALL_WORKSPACE_COPY"]);
    assert!(
        copy.starts_with(bravo.home.path().join(".elsecall/workspaces/alpha")),
        "{copy:?}"
    );
    assert_eq!(env["PWD"], copy.join("src").to_str().unwrap());
    assert!(
        !env.contains_key("ELSECALL_TAILNET_NAME"),
        "the service's own variables stay"
    );

    let output = run_env("charlie");
    let env = environment_of(&output);
    assert_eq!(env["ELSECALL_MEMBER"], "charlie");
    let same = base_path.join(repo.root.strip_prefix("/").unwrap());
    assert_eq!(
        env["ELSECALL_WORKSPACE_COPY"],
        same.to_str().unwrap(),
        "same-path location"
    );
    assert_eq!(
        env["PATH"], "/usr/bin:/bin:/usr/sbin",
        "the member's PATH for placed work"
    );
    assert_eq!(env["HOME"], charlie.home.path().to_str().unwrap());
}

#[test]
fn only_the_master_places_and_a_member_refuses_while_its_own_requests_wait() {
    let alpha = TestMember::start("alpha");
    // Four scripted performance cores and a reserve of two: capacity 2 on any host.
    let load_dir = tempfile::tempdir().unwrap();
    let load = load_dir.path().join("load.json");
    std::fs::write(&load, r#"{"performance_cores":4}"#).unwrap();
    let script = format!("file:{}", load.display());
    let bravo = TestMember::start_with(
        "bravo",
        "[capacity]\nreserved_cores = 2\n",
        &[("ELSECALL_MACHINE_LOAD", &script)],
    );
    enrol(&alpha, &bravo);
    let capacity = bravo.ok(&["runs"])["member"]["capacity"].clone();
    assert_eq!(capacity["capacity"], 2.0, "{capacity}");
    let repo = repository();

    // A non-master member runs placeable work itself and records why.
    let output = run(
        &bravo,
        &repo.root,
        &[
            "--member",
            "alpha",
            "--",
            "/bin/sh",
            "-c",
            "echo $ELSECALL_RUN_ID $ELSECALL_MEMBER",
        ],
        b"",
    );
    let out = stdout(&output);
    let (run_id, member) = out.trim().split_once(' ').unwrap();
    assert_eq!(member, "bravo");
    let record = explain(&bravo, run_id);
    assert!(
        record["placement_note"]
            .as_str()
            .unwrap()
            .contains("not the master"),
        "{record}"
    );

    // Outside a working tree, placeable work runs on the calling member.
    let outside = tempfile::tempdir().unwrap();
    let out = stdout(&run(
        &alpha,
        outside.path(),
        &[
            "--member",
            "bravo",
            "--",
            "/bin/sh",
            "-c",
            "echo $ELSECALL_MEMBER",
        ],
        b"",
    ));
    assert_eq!(out.trim(), "alpha");

    // bravo holds one core and queues a request for two of its own: it refuses
    // placed work even though one core is free.
    // Separate holders, since a holder's own second request would nest.
    let hold = |size: &str| {
        crate::processes::command("/bin/sh")
            .arg("-c")
            .arg(format!(
                "\"$EC\" lease acquire --size {size} --holder $$ >/dev/null; sleep 60"
            ))
            .env("HOME", bravo.home.path())
            .env("EC", env!("CARGO_BIN_EXE_elsecall"))
            .spawn()
            .unwrap()
    };
    let holding = hold("1");
    until("the first lease", crate::processes::WAIT, || {
        bravo.ok(&["runs"])["member"]["in_use"] == 1.0
    });
    let queued = hold("2");
    until("the queued request", crate::processes::WAIT, || {
        bravo.ok(&["runs"])["member"]["queued"]["interactive"] == 1
    });
    let started = Instant::now();
    let output = run(
        &alpha,
        &repo.root,
        &["--member", "bravo", "--hold-limit", "2s", "--", "true"],
        b"",
    );
    assert_eq!(output.status.code(), Some(125));
    let failure = envelope(&output);
    assert_eq!(failure["error"]["code"], "fleet_busy", "{failure}");
    assert!(
        failure["error"]["message"]
            .as_str()
            .unwrap()
            .contains("bravo"),
        "{failure}"
    );
    assert!(started.elapsed() >= Duration::from_secs(2));
    let record = explain(&alpha, failure["error"]["run_id"].as_str().unwrap());
    assert_eq!(
        record["decision"]["candidates"][0]["member"], "bravo",
        "{record}"
    );
    for mut holder in [holding, queued] {
        holder.kill().unwrap();
        holder.wait().unwrap();
    }
    let out = stdout(&run(
        &alpha,
        &repo.root,
        &[
            "--member",
            "bravo",
            "--hold-limit",
            "20s",
            "--",
            "/bin/sh",
            "-c",
            "echo $ELSECALL_MEMBER",
        ],
        b"",
    ));
    assert_eq!(out.trim(), "bravo");

    // Too large for the member is known at once.
    let output = run(
        &alpha,
        &repo.root,
        &["--member", "bravo", "--size", "5", "--", "true"],
        b"",
    );
    assert_eq!(envelope(&output)["error"]["code"], "request_too_large");
}

/// Runs `script` on bravo from `repo`'s root; its stdout's lines.
fn on_bravo(alpha: &TestMember, repo: &Repository, args: &[&str], script: &str) -> Vec<String> {
    let mut all = vec!["--member", "bravo"];
    all.extend_from_slice(args);
    all.extend_from_slice(&["--", "/bin/sh", "-c", script]);
    let output = run(alpha, &repo.root, &all, b"");
    stdout(&output).lines().map(str::to_owned).collect()
}

/// The leftovers bravo's record of `run_id` keeps once they ended.
fn ended_leftovers(bravo: &TestMember, run_id: &str) -> Value {
    let mut record = Value::Null;
    until("the leftovers' record", crate::processes::WAIT, || {
        record = explain(bravo, run_id);
        record["placed"]["leftovers"]["state"] == "ended"
    });
    record["placed"]["leftovers"].clone()
}

#[test]
fn leftovers_finish_on_the_member_after_the_caller_returned_and_capacity_freed() {
    let (alpha, bravo) = fleet();
    let repo = repository();
    // The leftover keeps stdout open and writes to it once the caller is gone:
    // its output is dropped, and it neither fails nor is stopped.
    let release = ReleaseGate::new(bravo.home.path(), "release-natural");
    let started = Instant::now();
    let lines = on_bravo(
        &alpha,
        &repo,
        &[],
        "mkdir -p build; echo \"$ELSECALL_WORKSPACE_COPY\"; echo \"$ELSECALL_RUN_ID\"; \
         (while [ -f \"$HOME/release-natural\" ]; do sleep 0.02; done; echo late; echo late >&2; touch build/done) &",
    );
    let returned = started.elapsed();
    let (copy, run_id) = (PathBuf::from(&lines[0]), lines[1].clone());
    assert_eq!(lines.len(), 2, "{lines:?}");
    let done = copy.join("build/done");
    assert!(!done.exists(), "the caller returned at once ({returned:?})");
    until(
        "bravo to free the run's capacity",
        crate::processes::WAIT,
        || bravo.ok(&["runs"])["member"]["in_use"].as_f64() == Some(0.0),
    );
    let record = explain(&bravo, &run_id);
    assert_eq!(record["state"], "ended", "{record}");
    assert_eq!(record["outcome"]["exit_code"], 0, "{record}");
    assert_eq!(
        record["placed"]["leftovers"]["state"], "running",
        "{record}"
    );
    assert!(!done.exists(), "still running when the capacity was freed");
    release.open();
    until(
        "the leftover to finish in the member's copy",
        crate::processes::WAIT,
        || done.exists(),
    );
    let leftovers = ended_leftovers(&bravo, &run_id);
    assert_eq!(leftovers["ending"]["by"], "finished", "{leftovers}");
    assert_eq!(leftovers["cap_ms"], 600_000, "the default cap");
    assert!(leftovers["duration_ms"].is_u64(), "{leftovers}");
    assert!(leftovers["cpu_ms"].is_u64(), "{leftovers}");
    // The caller's record has no leftovers: they are the member's.
    assert!(explain(&alpha, &run_id)["placed"]["leftovers"].is_null());
}

#[test]
fn perf_leftovers_are_stopped_at_the_cap_and_reports_name_labels_that_leave_heavy_work() {
    let alpha = TestMember::start("alpha");
    let bravo = TestMember::start_with("bravo", "[placed_runs]\nleftovers_cap = \"1s\"\n", &[]);
    enrol(&alpha, &bravo);
    let repo = repository();
    let mut run_ids = Vec::new();
    for n in 0..2 {
        let release = ReleaseGate::new(bravo.home.path(), &format!("release-cap-{n}"));
        let _keep_until_capped = &release;
        // TERM is ignored: the cap's kill follows the grace period.
        let lines = on_bravo(
            &alpha,
            &repo,
            &["--label", "telemetry ingest"],
            &format!(
                "mkdir -p build; rm -f build/trap-ready; (trap '' TERM; touch build/trap-ready; while [ -f \"$HOME/release-cap-{n}\" ]; do sleep 1; done) >/dev/null 2>&1 & pid=$!; while [ ! -f build/trap-ready ]; do sleep 0.02; done; echo $pid; echo \"$ELSECALL_RUN_ID\""
            ),
        );
        let pid: i32 = lines[0].parse().unwrap();
        until(
            "the cap to stop the leftover",
            crate::processes::WAIT,
            || !alive(pid),
        );
        let leftovers = ended_leftovers(&bravo, &lines[1]);
        assert_eq!(leftovers["ending"]["by"], "capped", "{leftovers}");
        assert_eq!(leftovers["cap_ms"], 1_000);
        let duration = leftovers["duration_ms"].as_u64().unwrap();
        assert!(
            duration >= 6_000,
            "the cap and TERM grace both elapsed: {leftovers}"
        );
        if std::env::var_os("EC_TEST_PERF").is_some() {
            assert!(duration < 15_000, "{leftovers}");
        }
        run_ids.push(lines[1].clone());
    }
    let report = alpha.ok(&["report", "--since", "1h"]);
    let leftovers = &report["leftovers"];
    assert_eq!(
        (leftovers["runs"].as_u64(), leftovers["capped"].as_u64()),
        (Some(2), Some(2)),
        "{report}"
    );
    assert_eq!(leftovers["by_label"][0]["label"], "telemetry ingest");
    let finding = report["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["kind"] == "heavy_leftovers")
        .unwrap_or_else(|| panic!("no finding: {report}"));
    assert_eq!(finding["label"], "telemetry ingest");
    assert_eq!(finding["metric"], "leftovers.by_label.capped");
    assert!(
        finding["message"]
            .as_str()
            .unwrap()
            .contains("the cap stopped 2 of them (100%)"),
        "{finding}"
    );
}

#[test]
fn a_lost_connection_still_stops_the_command_and_everything_it_started_at_once() {
    let (alpha, bravo) = fleet();
    let repo = repository();
    let mut child = spawn(
        &alpha,
        &repo.root,
        &[
            "--member",
            "bravo",
            "--",
            "/bin/sh",
            "-c",
            "sleep 60 >/dev/null 2>&1 & echo $!; echo \"$ELSECALL_RUN_ID\"; echo $$; exec sleep 60",
        ],
        &[],
    );
    let (background, mut rest) = first_line(&mut child);
    let mut lines = (&mut rest).lines().map(Result::unwrap);
    let run_id = lines.next().unwrap();
    let command: i32 = lines.next().unwrap().parse().unwrap();
    let background: i32 = background.parse().unwrap();
    assert!(alive(background) && alive(command));
    child.kill().unwrap();
    let _ = child.wait();
    until(
        "the command and its background process to stop",
        crate::processes::WAIT,
        || !alive(background) && !alive(command),
    );
    let mut record = Value::Null;
    until("bravo's record to end", crate::processes::WAIT, || {
        record = explain(&bravo, &run_id);
        record["state"] == "ended"
    });
    assert_eq!(record["outcome"]["code"], "connection_lost", "{record}");
    assert!(record["placed"]["leftovers"].is_null(), "{record}");
}

#[test]
fn the_next_run_of_a_copy_waits_for_leftovers_then_stops_them() {
    let (alpha, bravo) = fleet();
    let repo = repository();
    // Leftovers that end within the wait finish first: the next run sees their work.
    // They keep the command's stdout open, which must not keep the copy claimed:
    // the next run is admitted while they still run, and waits for them.
    let release = ReleaseGate::new(bravo.home.path(), "release-next");
    let first = on_bravo(
        &alpha,
        &repo,
        &[],
        "mkdir -p build; rm -f build/first build/release; echo \"$ELSECALL_RUN_ID\"; echo \"$ELSECALL_WORKSPACE_COPY\"; (while [ -f \"$HOME/release-next\" ]; do sleep 0.02; done; touch build/first) &",
    );
    let copy = PathBuf::from(&first[1]);
    until(
        "the first copy claim to release",
        crate::processes::WAIT,
        || {
            bravo.ok(&["copies"])["copies"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c["location"] == first[1] && c["state"] == "idle")
        },
    );
    let mut second = spawn(
        &alpha,
        &repo.root,
        &[
            "--member",
            "bravo",
            "--label",
            "after-leftover",
            "--",
            "/bin/sh",
            "-c",
            "echo \"$ELSECALL_RUN_ID\"; test -e build/first && echo finished first",
        ],
        &[],
    );
    let mut next_id = String::new();
    until(
        "the next run to claim the copy",
        crate::processes::WAIT,
        || {
            assert!(
                second.try_wait().unwrap().is_none(),
                "the command ran before the leftover finished"
            );
            let runs = bravo.ok(&["runs"]);
            if let Some(run) = runs["runs"]
                .as_array()
                .unwrap()
                .iter()
                .find(|r| r["label"] == "after-leftover" && r["state"] == "running")
            {
                next_id = run["run_id"].as_str().unwrap().to_owned();
                true
            } else {
                false
            }
        },
    );
    until(
        "the next run to enter its leftovers wait",
        crate::processes::WAIT,
        || {
            bravo
                .log()
                .lines()
                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .any(|event| {
                    event["event"] == "waiting_for_leftovers" && event["run_id"] == next_id
                })
        },
    );
    assert!(!copy.join("build/first").exists());
    release.open();
    wait_child(&mut second);
    let output = second.wait_with_output().unwrap();
    let second: Vec<_> = stdout(&output).lines().map(str::to_owned).collect();
    assert_eq!(second[1], "finished first", "{second:?}");
    let leftovers = ended_leftovers(&bravo, &first[0]);
    assert_eq!(leftovers["ending"]["by"], "finished", "{leftovers}");
    let waited = explain(&bravo, &second[0])["placed"]["leftovers_wait_ms"].clone();
    assert!(
        waited.as_u64().unwrap() > 0,
        "the actual wait was recorded: {waited}"
    );

    // Leftovers that outlast the wait are stopped before the copy is synced.
    let third = on_bravo(
        &alpha,
        &repo,
        &[],
        "sleep 60 >/dev/null 2>&1 & echo $!; echo \"$ELSECALL_RUN_ID\"",
    );
    let pid: i32 = third[0].parse().unwrap();
    let started = Instant::now();
    let fourth = on_bravo(&alpha, &repo, &[], "echo \"$ELSECALL_RUN_ID\"");
    assert!(
        started.elapsed() >= Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
    assert!(!alive(pid), "stopped before the next run started");
    let leftovers = ended_leftovers(&bravo, &third[1]);
    assert_eq!(leftovers["ending"]["by"], "next_run", "{leftovers}");
    assert_eq!(leftovers["ending"]["run_id"], fourth[0].as_str());
}

#[test]
fn a_service_restart_stops_leftovers() {
    let (alpha, mut bravo) = fleet();
    let repo = repository();
    let lines = on_bravo(
        &alpha,
        &repo,
        &[],
        "sleep 60 >/dev/null 2>&1 & echo $!; echo \"$ELSECALL_RUN_ID\"",
    );
    let pid: i32 = lines[0].parse().unwrap();
    until("the leftovers' record", crate::processes::WAIT, || {
        explain(&bravo, &lines[1])["placed"]["leftovers"]["state"] == "running"
    });
    bravo.stop();
    assert!(alive(pid), "a crashed service leaves them to its restart");
    bravo.restart();
    until(
        "the restart to stop the leftover",
        crate::processes::WAIT,
        || !alive(pid),
    );
    let record = explain(&bravo, &lines[1]);
    let leftovers = &record["placed"]["leftovers"];
    assert_eq!(leftovers["state"], "ended", "{record}");
    assert_eq!(leftovers["ending"]["by"], "service_restarted", "{record}");
    assert!(
        warnings_of(&record).contains("they were stopped"),
        "{record}"
    );
}

#[test]
fn one_run_uses_a_copy_at_a_time() {
    let (alpha, _bravo) = fleet();
    let repo = repository();
    // Hold the first run's lock until explicitly released. A second request
    // must time out for the occupied copy, rather than execute and fail mkdir.
    let script =
        "mkdir -p build; mkdir build/lock || exit 9; echo ready; read finish; rmdir build/lock";
    let mut first = spawn(
        &alpha,
        &repo.root,
        &["--member", "bravo", "--", "/bin/sh", "-c", script],
        &[],
    );
    let (ready, _reader) = first_line(&mut first);
    assert_eq!(ready, "ready");
    let blocked = run(
        &alpha,
        &repo.root,
        &[
            "--member",
            "bravo",
            "--hold-limit",
            "15s",
            "--",
            "/bin/sh",
            "-c",
            script,
        ],
        b"finish\n",
    );
    assert_eq!(blocked.status.code(), Some(125), "{blocked:?}");
    let error = envelope(&blocked);
    assert_eq!(error["error"]["code"], "fleet_busy", "{error}");
    assert!(
        error["error"]["message"].as_str().unwrap().contains("copy"),
        "{error}"
    );
    first.stdin.take().unwrap().write_all(b"finish\n").unwrap();
    let deadline = Instant::now() + crate::processes::WAIT;
    loop {
        if let Some(status) = first.try_wait().unwrap() {
            assert!(status.success(), "{status}");
            break;
        }
        if Instant::now() >= deadline {
            let _ = first.kill();
            let _ = first.wait();
            panic!("released command did not exit");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let second = run(
        &alpha,
        &repo.root,
        &["--member", "bravo", "--", "/bin/sh", "-c", script],
        b"finish\n",
    );
    assert!(second.status.success(), "{second:?}");
}

#[test]
fn perf_a_killed_member_service_or_caller_stops_the_command() {
    let (alpha, mut bravo) = fleet();
    let repo = repository();

    // The member's service dies: the caller fails at once, and the restarted
    // service stops the orphaned command.
    let mut child = spawn(
        &alpha,
        &repo.root,
        &[
            "--member",
            "bravo",
            "--",
            "/bin/sh",
            "-c",
            "echo $$; exec sleep 60",
        ],
        &[],
    );
    let (pid, _rest) = first_line(&mut child);
    let pid: i32 = pid.parse().unwrap();
    assert!(alive(pid));
    bravo.stop();
    let started = Instant::now();
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(125));
    let failure = envelope(&output);
    assert_eq!(failure["error"]["code"], "connection_lost", "{failure}");
    if std::env::var_os("EC_TEST_PERF").is_some() {
        assert!(started.elapsed() < Duration::from_secs(5));
    }
    let run_id = failure["error"]["run_id"].as_str().unwrap().to_owned();
    let record = explain(&alpha, &run_id);
    assert_eq!(record["outcome"]["code"], "connection_lost");
    assert_eq!(record["placed"]["reached"], "started", "{record}");
    assert_eq!(record["run_phases"]["remote_failure_phase"], "execution");
    assert!(record["run_phases"]["execution_ms"].is_null());
    assert!(record["run_phases"]["result_return_ms"].is_null());
    assert!(
        record["peer_io"]["attempts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["member"] == "bravo"
                && a["operation"] == "result_stream"
                && a["failure_kind"] == "receive"),
        "{record}"
    );
    bravo.restart();
    let report = alpha.ok(&["report", "--since", "5m"]);
    assert!(
        report["peer_io"]["members"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["member"] == "alpha")
            .unwrap()["history"]["attempts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["operation"] == "result_stream" && a["failure_kind"] == "receive"),
        "{report}"
    );
    until(
        "the orphaned command to stop",
        crate::processes::WAIT,
        || !alive(pid),
    );
    let record = explain(&bravo, &run_id);
    assert_eq!(record["outcome"]["code"], "connection_lost", "{record}");
    // Only the member knows its own restart ended the run; the caller saw the
    // member go silent, which counts against the member.
    assert_eq!(
        record["placed"]["interrupted"], "service_restart",
        "{record}"
    );
    let caller = explain(&alpha, &run_id);
    assert!(caller["placed"].get("interrupted").is_none(), "{caller}");

    // The caller dies: the member stops the command.
    let mut child = spawn(
        &alpha,
        &repo.root,
        &[
            "--member",
            "bravo",
            "--",
            "/bin/sh",
            "-c",
            "echo $$; exec sleep 60",
        ],
        &[],
    );
    let (pid, _rest) = first_line(&mut child);
    let pid: i32 = pid.parse().unwrap();
    child.kill().unwrap();
    let _ = child.wait();
    until(
        "the command to stop after its caller died",
        crate::processes::WAIT,
        || !alive(pid),
    );
}

#[test]
fn a_member_service_stops_promptly_while_a_placed_command_runs() {
    let (alpha, mut bravo) = fleet();
    let repo = repository();
    let mut child = spawn(
        &alpha,
        &repo.root,
        &[
            "--member",
            "bravo",
            "--",
            "/bin/sh",
            "-c",
            "echo $$; exec sleep 60",
        ],
        &[],
    );
    let (pid, _rest) = first_line(&mut child);
    let pid: i32 = pid.parse().unwrap();
    assert!(alive(pid));
    // Nothing waiting for the command may hold the service up.
    assert!(
        bravo.terminate_within(crate::processes::WAIT),
        "the service did not stop while a placed command ran"
    );
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(125));
    bravo.restart();
    until("the command to stop", crate::processes::WAIT, || {
        !alive(pid)
    });
}

#[test]
fn a_learned_size_shrinks_to_a_smaller_required_member() {
    let alpha = TestMember::start("alpha");
    let load = tempfile::tempdir().unwrap();
    let observation = load.path().join("load.json");
    std::fs::write(&observation, r#"{"performance_cores": 1}"#).unwrap();
    let bravo = TestMember::start_with(
        "bravo",
        "[capacity]\nreserved_cores = 0.75\n",
        &[(
            "ELSECALL_MACHINE_LOAD",
            &format!("file:{}", observation.display()),
        )],
    );
    enrol(&alpha, &bravo);
    let repo = repository();
    // Three runs keeping two processes busy teach alpha the label's size.
    let busy = "yes >/dev/null & a=$!; yes >/dev/null & b=$!; sleep 1.2; kill $a $b; wait";
    for _ in 0..3 {
        let output = run(
            &alpha,
            &repo.root,
            &["--label", "busy", "--", "/bin/sh", "-c", busy],
            b"",
        );
        assert!(output.status.success(), "{output:?}");
    }
    let output = run(
        &alpha,
        &repo.root,
        &[
            "--member",
            "bravo",
            "--label",
            "busy",
            "--",
            "/bin/sh",
            "-c",
            "echo $ELSECALL_RUN_ID",
        ],
        b"",
    );
    let run_id = stdout(&output).trim().to_owned();
    let caller = explain(&alpha, &run_id);
    assert_eq!(caller["size_basis"]["from"], "learned", "{caller}");
    assert_eq!(
        caller["size"], 0.25,
        "the learned minimum of 0.5 must shrink: {caller}"
    );
    let member = explain(&bravo, &run_id);
    assert_eq!(member["size"], caller["size"]);
    assert_eq!(member["size_basis"]["from"], "learned");
}

#[test]
fn perf_a_silent_member_is_a_lost_connection_after_the_grace_period() {
    let (alpha, bravo) = fleet();
    let repo = repository();
    let mut child = spawn(
        &alpha,
        &repo.root,
        &[
            "--member",
            "bravo",
            "--",
            "/bin/sh",
            "-c",
            "echo $$; exec sleep 60",
        ],
        &[],
    );
    let (pid, _rest) = first_line(&mut child);
    let pid: i32 = pid.parse().unwrap();
    let service = bravo.service_pid() as i32;
    // SAFETY: kill has no memory-safety preconditions.
    unsafe { libc::kill(service, libc::SIGSTOP) };
    let started = Instant::now();
    let output = child.wait_with_output().unwrap();
    let silent_for = started.elapsed();
    // SAFETY: as above.
    unsafe { libc::kill(service, libc::SIGCONT) };
    assert_eq!(envelope(&output)["error"]["code"], "connection_lost");
    assert!(silent_for >= Duration::from_secs(9), "{silent_for:?}");
    if std::env::var_os("EC_TEST_PERF").is_some() {
        assert!(silent_for < Duration::from_secs(20), "{silent_for:?}");
    }
    // Once it runs again, the member finds the caller gone and stops the command.
    until(
        "the command to stop once the member resumed",
        crate::processes::WAIT,
        || !alive(pid),
    );
}

#[test]
fn both_members_record_the_placed_run_and_work_nests_inside_it() {
    let (alpha, bravo) = fleet();
    let repo = repository();
    std::fs::write(
        repo.root.join(".elsecall.toml"),
        "[environment]\nnames = [\"EC\"]\n",
    )
    .unwrap();
    let script = "\"$EC\" lease acquire --size 1 --label inner --holder $$ | tr -d '\\n'; echo; echo $ELSECALL_RUN_ID";
    let mut child = spawn(
        &alpha,
        &repo.root,
        &[
            "--member",
            "bravo",
            "--label",
            "placed test",
            "--",
            "/bin/sh",
            "-c",
            script,
        ],
        &[("EC", env!("CARGO_BIN_EXE_elsecall"))],
    );
    drop(child.stdin.take());
    let output = child.wait_with_output().unwrap();
    let out = stdout(&output);
    let mut lines = out.lines();
    let lease: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    let run_id = lines.next().unwrap();
    assert_eq!(
        lease["data"]["nested_in"], run_id,
        "a lease inside a placed run nests in it"
    );
    assert_eq!(lease["data"]["member"], "bravo");

    let caller = explain(&alpha, run_id);
    assert_eq!(caller["member"], "bravo");
    assert_eq!(caller["outcome"]["exit_code"], 0);
    assert_eq!(caller["placed"]["side"], "caller");
    assert_eq!(caller["placed"]["reached"], "finished");
    let sync_ms = caller["placed"]["sync"]["duration_ms"].as_u64().unwrap();
    let phases = &caller["run_phases"];
    let execution_ms = phases["execution_ms"].as_u64().unwrap();
    let total_ms = phases["request_to_result_ms"].as_u64().unwrap();
    assert!(phases["result_return_ms"].is_null(), "{caller}");
    assert!(sync_ms + execution_ms <= total_ms, "{caller}");
    let runs = alpha.ok(&["runs"]);
    let summary = runs["runs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["run_id"] == run_id)
        .unwrap();
    assert_eq!(summary["run_phases"], *phases);
    assert_eq!(summary["placed"]["sync_ms"], sync_ms);
    assert!(caller["waited_ms"].is_u64());
    assert!(caller["placed"]["sync"]["sent_bytes"].as_u64().unwrap() > 0);
    assert_eq!(caller["decision"]["candidates"][0]["member"], "bravo");
    assert_eq!(
        caller["placed"]["warmth"], "cold",
        "the first run set the copy up"
    );
    assert!(
        caller["placed"]["repository"]
            .as_str()
            .unwrap()
            .ends_with("/.git")
    );
    assert!(
        caller["cpu_ms"].is_u64(),
        "the member measured the command's CPU time: {caller}"
    );
    let member = explain(&bravo, run_id);
    assert_eq!(member["cpu_ms"], caller["cpu_ms"]);
    assert_eq!(member["calling_member"], "alpha");
    assert_eq!(member["label"], "placed test");
    assert_eq!(member["outcome"]["exit_code"], 0);
    assert_eq!(member["placed"]["side"], "member");
    assert_eq!(
        member["placed"]["caller_workspace"],
        repo.root.to_str().unwrap()
    );
    assert!(member["placed"]["sync"]["copy_files"].as_u64().unwrap() >= 4);
    assert!(
        member["placed"]["copy"]
            .as_str()
            .unwrap()
            .contains(".elsecall/workspaces/alpha/")
    );

    // A second run of the unchanged tree moves only digests.
    let output = run(
        &alpha,
        &repo.root,
        &[
            "--member",
            "bravo",
            "--",
            "/bin/sh",
            "-c",
            "echo $ELSECALL_RUN_ID",
        ],
        b"",
    );
    let second = explain(&alpha, stdout(&output).trim());
    assert_eq!(second["placed"]["warmth"], "warm");
    let sync = &second["placed"]["sync"];
    assert_eq!(sync["files_written"], 0, "{sync}");
    assert_eq!(sync["full_listing"], false, "{sync}");
}

fn status_of(path: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

#[test]
fn declared_outputs_come_back_after_any_exit_and_only_when_changed() {
    let (alpha, _bravo) = fleet();
    let repo = repository();
    std::fs::write(
        repo.root.join(".elsecall.toml"),
        "[outputs]\npaths = [\"build/reports\", \"build/result.txt\", \"build/link\"]\n",
    )
    .unwrap();
    let status_file = repo.root.parent().unwrap().join("status.json");
    let status = status_file.to_str().unwrap();
    let produce = "mkdir -p build/reports/sub; echo pass > build/reports/a.xml; printf '\\001\\002' > build/reports/sub/b.bin; chmod +x build/reports/sub/b.bin; echo r1 > build/result.txt; ln -sf /etc build/reports/escape; ln -sfn /tmp build/link; exit 3";
    let output = run(
        &alpha,
        &repo.root,
        &[
            "--member",
            "bravo",
            "--status-file",
            status,
            "--",
            "/bin/sh",
            "-c",
            produce,
        ],
        b"",
    );
    assert_eq!(output.status.code(), Some(3), "{output:?}");
    assert!(
        output.stderr.is_empty(),
        "no extra output: {:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    let build = repo.root.join("build");
    assert_eq!(
        std::fs::read_to_string(build.join("reports/a.xml")).unwrap(),
        "pass\n"
    );
    assert_eq!(
        std::fs::read(build.join("reports/sub/b.bin")).unwrap(),
        b"\x01\x02"
    );
    use std::os::unix::fs::PermissionsExt;
    assert_ne!(
        std::fs::metadata(build.join("reports/sub/b.bin"))
            .unwrap()
            .permissions()
            .mode()
            & 0o111,
        0
    );
    assert_eq!(
        std::fs::read_to_string(build.join("result.txt")).unwrap(),
        "r1\n"
    );
    assert!(
        std::fs::symlink_metadata(build.join("reports/escape")).is_err(),
        "links never come back"
    );
    assert!(std::fs::symlink_metadata(build.join("link")).is_err());
    let warnings = status_of(&status_file)["data"]["warnings"].clone();
    let text = warnings.to_string();
    assert!(
        text.contains("build/reports/escape") && text.contains("build/link"),
        "{warnings}"
    );
    assert!(
        build.join("secret.o").exists(),
        "the caller's own ignored files stay"
    );

    // Unchanged on the member: the caller's own edit stays.
    std::fs::write(build.join("result.txt"), "local edit\n").unwrap();
    stdout(&run(
        &alpha,
        &repo.root,
        &["--member", "bravo", "--", "true"],
        b"",
    ));
    assert_eq!(
        std::fs::read_to_string(build.join("result.txt")).unwrap(),
        "local edit\n"
    );

    // Removed on the member: removed here too. A changed directory is replaced whole.
    let output = run(
        &alpha,
        &repo.root,
        &[
            "--member",
            "bravo",
            "--",
            "/bin/sh",
            "-c",
            "rm build/result.txt; rm -r build/reports/sub",
        ],
        b"",
    );
    stdout(&output);
    assert!(!build.join("result.txt").exists());
    assert!(!build.join("reports/sub").exists());
    assert_eq!(
        std::fs::read_to_string(build.join("reports/a.xml")).unwrap(),
        "pass\n"
    );

    // A link in the caller's tree is never written through.
    let outside = tempfile::tempdir().unwrap();
    std::fs::remove_dir_all(build.join("reports")).unwrap();
    std::os::unix::fs::symlink(outside.path(), build.join("reports")).unwrap();
    std::fs::remove_file(repo.root.join(".elsecall.toml")).unwrap();
    std::fs::write(
        repo.root.join(".elsecall.toml"),
        "[outputs]\npaths = [\"build/reports/x.txt\"]\n",
    )
    .unwrap();
    let output = run(
        &alpha,
        &repo.root,
        &[
            "--member",
            "bravo",
            "--status-file",
            status,
            "--",
            "/bin/sh",
            "-c",
            "mkdir -p build/reports; echo x > build/reports/x.txt",
        ],
        b"",
    );
    stdout(&output);
    assert!(
        !outside.path().join("x.txt").exists(),
        "written through a link"
    );
    assert!(
        status_of(&status_file)["data"]["warnings"]
            .to_string()
            .contains("build/reports/x.txt")
    );
}

#[test]
fn outputs_of_many_files_come_back() {
    let (alpha, _bravo) = fleet();
    let repo = repository();
    std::fs::write(
        repo.root.join(".elsecall.toml"),
        "[outputs]\npaths = [\"build/many\"]\n",
    )
    .unwrap();
    // Far more than one frame can describe.
    let make = "mkdir -p build/many && cd build/many && i=0; while [ $i -lt 12000 ]; do echo $i > f$i.txt; i=$((i+1)); done";
    stdout(&run(
        &alpha,
        &repo.root,
        &["--member", "bravo", "--", "/bin/sh", "-c", make],
        b"",
    ));
    let many = repo.root.join("build/many");
    assert_eq!(std::fs::read_dir(&many).unwrap().count(), 12000);
    assert_eq!(
        std::fs::read_to_string(many.join("f11999.txt")).unwrap(),
        "11999\n"
    );
}

#[test]
fn many_skipped_links_make_one_warning_and_the_record_keeps_its_exit() {
    let (alpha, bravo) = fleet();
    let repo = repository();
    std::fs::write(
        repo.root.join(".elsecall.toml"),
        "[outputs]\npaths = [\"build/bin\"]\n",
    )
    .unwrap();
    let status_file = repo.root.parent().unwrap().join("links.json");
    let make = "mkdir -p build/bin && i=0; while [ $i -lt 70 ]; do ln -sf /bin/sh build/bin/tool$i; i=$((i+1)); done; echo $ELSECALL_RUN_ID; exit 0";
    let output = run(
        &alpha,
        &repo.root,
        &[
            "--member",
            "bravo",
            "--status-file",
            status_file.to_str().unwrap(),
            "--",
            "/bin/sh",
            "-c",
            make,
        ],
        b"",
    );
    assert!(output.status.success(), "{output:?}");
    let run_id = String::from_utf8(output.stdout).unwrap().trim().to_owned();
    let warnings = status_of(&status_file)["data"]["warnings"].clone();
    assert_eq!(warnings.as_array().unwrap().len(), 1, "{warnings}");
    assert!(
        warnings[0].as_str().unwrap().contains("and 69 more"),
        "{warnings}"
    );
    for member in [&alpha, &bravo] {
        let record = explain(member, &run_id);
        assert_eq!(record["outcome"]["exit_code"], 0, "{record}");
        assert_eq!(record["placed"]["reached"], "finished");
        assert_eq!(
            record["run_phases"]["remote_failure_phase"],
            "result_return"
        );
        assert!(record["run_phases"]["result_return_ms"].is_null());
        assert!(record["run_phases"]["request_to_result_ms"].is_null());
    }
    let report = bravo.ok(&["report", "--since", "5m"]);
    let phases = &report["fleet"]["run_phases"];
    assert_eq!(phases["remote_failures"]["result_return"], 1);
    assert_eq!(phases["remote"]["execution"]["count"], 1);
    assert_eq!(phases["remote"]["result_return"]["count"], 0);
}

/// The run's output-staging areas left in the member's copy.
fn staging_areas(copy: &Path) -> Vec<String> {
    std::fs::read_dir(copy.join(".git/elsecall-outputs"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect()
}

fn warnings_of(record: &Value) -> String {
    record["warnings"].to_string()
}

#[test]
fn outputs_come_back_as_they_were_at_exit_whatever_leftover_processes_do() {
    let (alpha, bravo) = fleet();
    let repo = repository();
    std::fs::write(
        repo.root.join(".elsecall.toml"),
        "[outputs]\npaths = [\"build/out/a\", \"build/out/b\"]\n",
    )
    .unwrap();
    let status_file = repo.root.parent().unwrap().join("leftover.json");
    // A sub-window of stdout lets the command exit, while the caller's full
    // stdout pipe delays output-return until the parent drains it below.
    let release = ReleaseGate::new(bravo.home.path(), "release-snapshot");
    let make = "mkdir -p build/out; echo a > build/out/a; echo b > build/out/b; (while [ -f \"$HOME/release-snapshot\" ]; do sleep 0.02; done; rm build/out/b) & echo $ELSECALL_RUN_ID; echo $ELSECALL_WORKSPACE_COPY; head -c 524288 /dev/zero";
    let mut child = spawn(
        &alpha,
        &repo.root,
        &[
            "--member",
            "bravo",
            "--status-file",
            status_file.to_str().unwrap(),
            "--",
            "/bin/sh",
            "-c",
            make,
        ],
        &[],
    );
    let (run_id, reader) = first_line(&mut child);
    let (copy, mut reader) = next_line(&mut child, reader);
    let copy = PathBuf::from(copy);
    let snapshot = copy
        .join(".git/elsecall-outputs")
        .join(&run_id)
        .join("build/out/b");
    until(
        "the output snapshot to contain b",
        crate::processes::WAIT,
        || std::fs::read(&snapshot).is_ok_and(|bytes| bytes == b"b\n"),
    );
    release.open();
    until("the leftover to remove b", crate::processes::WAIT, || {
        !copy.join("build/out/b").exists()
    });
    // The command has already exited. Keep its stdout backpressured long
    // enough to distinguish stream drain from declared output return.
    let drain_delay = Duration::from_millis(150);
    std::thread::sleep(drain_delay);
    let (sent, received) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = reader.read_to_end(&mut bytes).map(|_| bytes);
        let _ = sent.send(result);
    });
    wait_child(&mut child);
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    let padding = received
        .recv_timeout(crate::processes::WAIT)
        .unwrap()
        .unwrap();
    assert_eq!(padding, vec![0; 524288]);
    let out = repo.root.join("build/out");
    assert_eq!(std::fs::read_to_string(out.join("a")).unwrap(), "a\n");
    assert_eq!(std::fs::read_to_string(out.join("b")).unwrap(), "b\n");
    assert!(
        !copy.join("build/out/b").exists(),
        "the leftover process removed it on the member"
    );
    let status = status_of(&status_file);
    assert!(status["data"].get("warnings").is_none(), "{status}");
    for member in [&alpha, &bravo] {
        until(
            "the member exit to be recorded",
            crate::processes::WAIT,
            || explain(member, &run_id)["placed"]["exit_code"] == 0,
        );
        let record = explain(member, &run_id);
        assert_eq!(record["warnings"], serde_json::json!([]), "{record}");
        assert_eq!(record["placed"]["exit_code"], 0, "{record}");
    }
    let caller = explain(&alpha, &run_id);
    let phases = &caller["run_phases"];
    assert!(phases["execution_ms"].as_u64().unwrap() >= drain_delay.as_millis() as u64);
    let sync_ms = caller["placed"]["sync"]["duration_ms"].as_u64().unwrap();
    let returned_ms = phases["result_return_ms"].as_u64().unwrap();
    let total_ms = phases["request_to_result_ms"].as_u64().unwrap();
    assert!(sync_ms + phases["execution_ms"].as_u64().unwrap() + returned_ms <= total_ms);
    assert_eq!(staging_areas(&copy), Vec::<String>::new());
}

#[test]
fn outputs_that_cannot_come_back_never_keep_the_others_and_both_records_say_why() {
    let (alpha, bravo) = fleet();
    let repo = repository();
    std::fs::write(
        repo.root.join(".elsecall.toml"),
        "[outputs]\npaths = [\"build/a.txt\", \"build/logs\", \"build/via/x.txt\"]\n",
    )
    .unwrap();
    let status_file = repo.root.parent().unwrap().join("partial.json");
    let make = "mkdir -p build/logs build/elsewhere; echo ok > build/logs/ok.txt; echo s > build/logs/locked; chmod 000 build/logs/locked; ln -sfn elsewhere build/via; echo x > build/via/x.txt; echo a > build/a.txt; echo $ELSECALL_RUN_ID; echo $ELSECALL_WORKSPACE_COPY; exit 7";
    let output = run(
        &alpha,
        &repo.root,
        &[
            "--member",
            "bravo",
            "--status-file",
            status_file.to_str().unwrap(),
            "--",
            "/bin/sh",
            "-c",
            make,
        ],
        b"",
    );
    assert_eq!(output.status.code(), Some(7), "{output:?}");
    assert!(
        output.stderr.is_empty(),
        "warnings never reach the command's output: {output:?}"
    );
    let text = String::from_utf8(output.stdout).unwrap();
    let mut lines = text.lines();
    let (run_id, copy) = (lines.next().unwrap(), PathBuf::from(lines.next().unwrap()));
    let build = repo.root.join("build");
    assert_eq!(std::fs::read_to_string(build.join("a.txt")).unwrap(), "a\n");
    assert_eq!(
        std::fs::read_to_string(build.join("logs/ok.txt")).unwrap(),
        "ok\n"
    );
    assert!(!build.join("logs/locked").exists());
    assert!(!build.join("via").exists());
    let unreadable = "build/logs/locked was not returned: it could not be read on the member";
    let outside = "build/via/x.txt was not returned: a directory on its way is a link";
    let status = status_of(&status_file)["data"]["warnings"].to_string();
    assert!(
        status.contains(unreadable) && status.contains(outside),
        "{status}"
    );
    for member in [&alpha, &bravo] {
        let record = explain(member, run_id);
        let warnings = warnings_of(&record);
        assert!(
            warnings.contains(unreadable) && warnings.contains(outside),
            "{record}"
        );
        assert_eq!(record["placed"]["exit_code"], 7, "{record}");
    }
    assert_eq!(staging_areas(&copy), Vec::<String>::new());
    // Readable again, so the member's temporary home can be removed.
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(
        copy.join("build/logs/locked"),
        std::fs::Permissions::from_mode(0o600),
    );
}

#[test]
fn a_cleanup_warning_keeps_the_measurement_when_outputs_are_usable() {
    use std::os::unix::fs::PermissionsExt;
    let (alpha, _bravo) = fleet();
    let repo = repository();
    std::fs::write(
        repo.root.join(".elsecall.toml"),
        "[outputs]\npaths = [\"build/out\"]\n",
    )
    .unwrap();
    let out = repo.root.join("build/out");
    std::fs::create_dir(&out).unwrap();
    std::fs::write(out.join("old"), "old").unwrap();
    std::fs::set_permissions(&out, std::fs::Permissions::from_mode(0o000)).unwrap();
    let output = run(
        &alpha,
        &repo.root,
        &[
            "--member",
            "bravo",
            "--",
            "/bin/sh",
            "-c",
            "mkdir -p build/out; echo new > build/out/new; echo $ELSECALL_RUN_ID",
        ],
        b"",
    );
    for entry in std::fs::read_dir(repo.root.join("build"))
        .unwrap()
        .flatten()
    {
        if entry.path().is_dir() {
            std::fs::set_permissions(entry.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        }
    }
    let run_id = stdout(&output);
    assert_eq!(std::fs::read_to_string(out.join("new")).unwrap(), "new\n");
    let record = explain(&alpha, run_id.trim());
    assert!(
        warnings_of(&record).contains("old version could not be removed"),
        "{record}"
    );
    let phases = &record["run_phases"];
    assert!(phases["result_return_ms"].is_u64(), "{record}");
    assert!(phases["request_to_result_ms"].is_u64(), "{record}");
    assert!(phases["remote_failure_phase"].is_null(), "{record}");
}

#[test]
fn a_connection_lost_before_the_workspace_is_synced_is_attributed_to_sync() {
    let (alpha, bravo) = fleet();
    let repo = repository();
    let gate = sync_gate::SyncGate::start_run(&bravo);
    gate.enroll(&alpha, &bravo);
    gate.arm();
    let child = spawn(
        &alpha,
        &repo.root,
        &[
            "--member",
            "bravo",
            "--label",
            "broken sync",
            "--",
            "/bin/echo",
            "ran",
        ],
        &[],
    );
    gate.wait();
    drop(gate);
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(125), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    let failure = envelope(&output);
    assert_eq!(failure["error"]["code"], "connection_lost", "{failure}");
    let run_id = failure["error"]["run_id"].as_str().unwrap();
    let caller = explain(&alpha, run_id);
    assert_eq!(caller["placed"]["reached"], "placed", "{caller}");
    assert!(caller["placed"]["sync"].is_null(), "{caller}");
    assert!(caller["waited_ms"].is_u64(), "{caller}");
    assert_eq!(
        caller["run_phases"],
        serde_json::json!({"remote_failure_phase": "workspace_sync"})
    );
    let report = alpha.ok(&["report", "--since", "5m", "--label", "broken sync"]);
    let phases = &report["fleet"]["run_phases"];
    assert_eq!(phases["remote_failures"]["workspace_sync"], 1);
    assert_eq!(phases["remote"]["admission_wait"]["count"], 1);
    assert_eq!(phases["remote"]["workspace_sync"]["count"], 0);
}

#[test]
fn nothing_comes_back_after_a_lost_connection() {
    let (alpha, mut bravo) = fleet();
    let repo = repository();
    std::fs::write(
        repo.root.join(".elsecall.toml"),
        "[outputs]\npaths = [\"build/out.txt\"]\n",
    )
    .unwrap();
    let mut child = spawn(
        &alpha,
        &repo.root,
        &[
            "--member",
            "bravo",
            "--",
            "/bin/sh",
            "-c",
            "echo made > build/out.txt; echo ready; exec sleep 60",
        ],
        &[],
    );
    let (line, _rest) = first_line(&mut child);
    assert_eq!(line, "ready");
    bravo.stop();
    let output = child.wait_with_output().unwrap();
    assert_eq!(envelope(&output)["error"]["code"], "connection_lost");
    assert!(!repo.root.join("build/out.txt").exists());
}

#[test]
fn consistency_warnings_go_to_records_the_fleet_view_and_the_status_file() {
    let (alpha, bravo) = fleet();
    let repo = repository();
    for (member, version) in [(&alpha, "16.1"), (&bravo, "16.2")] {
        std::fs::write(member.home.path().join("toolchain-version"), version).unwrap();
        std::fs::write(member.home.path().join("shared.cfg"), "same").unwrap();
    }
    std::fs::write(
        repo.root.join(".elsecall.toml"),
        "[consistency]\nconfiguration = [\"~/shared.cfg\"]\n[consistency.toolchains]\nxcode = [\"/bin/sh\", \"-c\", \"cat \\\"$HOME/toolchain-version\\\"\"]\n",
    )
    .unwrap();
    let status_file = repo.root.parent().unwrap().join("status.json");
    let output = run(
        &alpha,
        &repo.root,
        &[
            "--member",
            "bravo",
            "--status-file",
            status_file.to_str().unwrap(),
            "--",
            "/bin/sh",
            "-c",
            "echo $ELSECALL_RUN_ID",
        ],
        b"",
    );
    let run_id = stdout(&output).trim().to_owned();
    assert!(output.stderr.is_empty(), "never in the command's output");
    let warnings = status_of(&status_file)["data"]["warnings"].clone();
    assert_eq!(warnings.as_array().unwrap().len(), 1, "{warnings}");
    assert!(
        warnings[0]
            .as_str()
            .unwrap()
            .contains("Toolchain xcode differs between alpha and bravo")
    );
    assert!(
        !warnings.to_string().contains("16."),
        "fingerprints, never values"
    );
    for member in [&alpha, &bravo] {
        let record = explain(member, &run_id);
        assert!(
            record["warnings"].to_string().contains("Toolchain xcode"),
            "{record}"
        );
    }
    let view = alpha.view_of("bravo");
    assert_eq!(view["latest_placed_run"]["run_id"], run_id.as_str());
    assert!(
        view["latest_placed_run"]["warnings"]
            .to_string()
            .contains("Toolchain xcode"),
        "{view}"
    );

    // Matching toolchains and differing configuration.
    std::fs::write(bravo.home.path().join("toolchain-version"), "16.1").unwrap();
    std::fs::write(bravo.home.path().join("shared.cfg"), "changed").unwrap();
    run(
        &alpha,
        &repo.root,
        &[
            "--member",
            "bravo",
            "--status-file",
            status_file.to_str().unwrap(),
            "--",
            "true",
        ],
        b"",
    );
    let warnings = status_of(&status_file)["data"]["warnings"].clone();
    assert_eq!(warnings.as_array().unwrap().len(), 1, "{warnings}");
    assert!(
        warnings[0]
            .as_str()
            .unwrap()
            .contains("Configuration ~/shared.cfg differs"),
        "{warnings}"
    );
}

/// The caller's work on branch `work`, whose upstream is `origin/work`, while
/// others moved `origin/main` on and pushed a release branch and a feature branch.
fn repository_with_a_remote() -> (Repository, PathBuf) {
    let repo = repository();
    let base = repo.root.parent().unwrap().to_owned();
    let remote = base.join("remote.git");
    git(
        &base,
        &[
            "init",
            "-q",
            "--bare",
            "-b",
            "main",
            remote.to_str().unwrap(),
        ],
    );
    let root = &repo.root;
    git(root, &["remote", "add", "origin", remote.to_str().unwrap()]);
    git(root, &["push", "-q", "origin", "main"]);
    let other = base.join("other");
    git(
        &base,
        &[
            "clone",
            "-q",
            remote.to_str().unwrap(),
            other.to_str().unwrap(),
        ],
    );
    for branch in ["main", "release/1.0", "their-feature"] {
        git(&other, &["switch", "-q", "-C", branch, "origin/main"]);
        for n in 1..=2 {
            std::fs::write(other.join("theirs.txt"), format!("{branch} {n}\n")).unwrap();
            git(&other, &["add", "-A"]);
            git(&other, &["commit", "-q", "-m", &format!("{branch} {n}")]);
        }
        git(&other, &["push", "-q", "origin", branch]);
    }
    git(root, &["switch", "-q", "-c", "work"]);
    git(root, &["commit", "-q", "-m", "pushed work"]);
    git(root, &["push", "-q", "-u", "origin", "work"]);
    git(root, &["commit", "-q", "--allow-empty", "-m", "local work"]);
    git(root, &["fetch", "-q", "origin"]);
    git(root, &["remote", "set-head", "origin", "main"]);
    (repo, other)
}

/// What scripts comparing against the default branch and the upstream ask.
const COMPARISONS: &str = "git merge-base HEAD origin/main; git rev-parse origin/HEAD; git rev-list --count HEAD..origin/main; git rev-parse @{upstream}; git rev-parse origin/release/1.0";
/// What a copy does not carry: other remote branches and the remote's URL.
const ABSENT: &str = "git rev-parse -q --verify origin/their-feature || echo not carried; git config --get remote.origin.url || echo no url; git fetch -q origin 2>/dev/null || echo fetch failed";

#[test]
fn a_placed_run_compares_against_the_callers_default_branch_and_upstream() {
    let (alpha, bravo) = fleet();
    let (repo, other) = repository_with_a_remote();
    std::fs::write(
        repo.root.join(".elsecall.toml"),
        "[workspace]\nrefs = [\"refs/remotes/origin/release/*\"]\n",
    )
    .unwrap();
    let placed = |script: &str| {
        let output = run(
            &alpha,
            &repo.root,
            &[
                "--member",
                "bravo",
                "--",
                "/bin/sh",
                "-c",
                &format!("{script}; echo $ELSECALL_RUN_ID >&2"),
            ],
            b"",
        );
        let run_id = String::from_utf8_lossy(&output.stderr)
            .lines()
            .last()
            .unwrap()
            .to_owned();
        (stdout(&output), run_id)
    };
    let local = |root: &Path| {
        let output = crate::processes::command("/bin/sh")
            .current_dir(root)
            .args(["-c", COMPARISONS])
            .output()
            .unwrap();
        String::from_utf8(output.stdout).unwrap()
    };
    let expected = local(&repo.root);
    assert_eq!(expected.lines().nth(2), Some("2"), "{expected}");
    let (out, run_id) = placed(&format!("{COMPARISONS}; {ABSENT}"));
    assert_eq!(out, expected + "not carried\nno url\nfetch failed\n");
    for member in [&alpha, &bravo] {
        let sync = &explain(member, &run_id)["placed"]["sync"];
        assert_eq!(
            sync["refs"],
            serde_json::json!([
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/main",
                "refs/remotes/origin/release/1.0",
                "refs/remotes/origin/work"
            ]),
            "{sync}"
        );
    }
    let sync = &explain(&alpha, &run_id)["placed"]["sync"];
    assert_eq!(sync["refs_only_commits"], 4, "{sync}");

    // The caller fetches main again; the next run compares against it.
    git(&other, &["switch", "-q", "main"]);
    git(&other, &["commit", "-q", "--allow-empty", "-m", "main 3"]);
    git(&other, &["push", "-q", "origin", "main"]);
    git(&repo.root, &["fetch", "-q", "origin"]);
    let expected = local(&repo.root);
    assert_eq!(expected.lines().nth(2), Some("3"), "{expected}");
    assert_eq!(placed(COMPARISONS).0, expected);

    // A declared ref outside refs/remotes/ and refs/tags/ is refused before the
    // run, saying how to fix it.
    std::fs::write(
        repo.root.join(".elsecall.toml"),
        "[workspace]\nrefs = [\"refs/heads/*\"]\n",
    )
    .unwrap();
    let output = run(
        &alpha,
        &repo.root,
        &["--member", "bravo", "--", "true"],
        b"",
    );
    assert_eq!(output.status.code(), Some(125), "{output:?}");
    let refused = envelope(&output);
    assert_eq!(refused["error"]["code"], "invalid_request", "{refused}");
    assert!(
        refused["error"]["message"]
            .as_str()
            .unwrap()
            .contains("[workspace] refs"),
        "{refused}"
    );
}

#[test]
fn a_slow_upstream_sealed_record_preserves_workspace_sync_and_binary_result() {
    use std::sync::atomic::Ordering;
    let (alpha, bravo) = fleet();
    let proxy =
        sealed_fragmentation::Proxy::start(&bravo, sealed_fragmentation::Fault::UpstreamSlow);
    proxy.enroll(&alpha, &bravo);
    let repo = repository();
    let payload: Vec<u8> = (0..524288).map(|n| n as u8).collect();
    std::fs::write(repo.root.join("payload.bin"), &payload).unwrap();
    let mut child = spawn(
        &alpha,
        &repo.root,
        &[
            "--member",
            "bravo",
            "--",
            "/bin/sh",
            "-c",
            r"cat payload.bin; printf '\000\377err' >&2; exit 7",
        ],
        &[],
    );
    drop(child.stdin.take());
    let start = Instant::now();
    until("the upstream tag gate", crate::processes::WAIT, || {
        proxy.tag_held.load(Ordering::SeqCst)
    });
    assert!(start.elapsed() >= Duration::from_secs(16));
    assert!(child.try_wait().unwrap().is_none());
    proxy.release.store(true, Ordering::SeqCst);
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(7), "{output:?}");
    assert_eq!(output.stdout, payload);
    assert_eq!(output.stderr, b"\0\xfferr");
    assert_eq!(proxy.attaches.load(Ordering::SeqCst), 1);
}

#[test]
fn a_stalled_local_relay_still_expires_the_runnable_cli() {
    struct Resume(u32);
    impl Drop for Resume {
        fn drop(&mut self) {
            unsafe {
                libc::kill(self.0 as i32, libc::SIGCONT);
            }
        }
    }
    let (alpha, _bravo) = fleet();
    let repo = repository();
    let mut child = spawn(
        &alpha,
        &repo.root,
        &[
            "--member",
            "bravo",
            "--",
            "/bin/sh",
            "-c",
            "echo $$; while :; do sleep 0.1; done",
        ],
        &[],
    );
    let (pid, _stdout) = first_line(&mut child);
    let pid = pid.parse().unwrap();
    let resume = Resume(alpha.service_pid());
    assert_eq!(unsafe { libc::kill(resume.0 as i32, libc::SIGSTOP) }, 0);
    let start = Instant::now();
    let output = child.wait_with_output().unwrap();
    let elapsed = start.elapsed();
    drop(resume);
    assert_eq!(output.status.code(), Some(125), "{output:?}");
    assert_eq!(envelope(&output)["error"]["code"], "connection_lost");
    assert!(
        elapsed >= Duration::from_secs(9) && elapsed < Duration::from_secs(25),
        "{elapsed:?}"
    );
    until("the member command to stop", crate::processes::WAIT, || {
        !alive(pid)
    });
}

/// A real pre-negotiation executable is supplied explicitly; never download or
/// rebuild historical source during the ordinary test suite.
#[test]
#[ignore = "requires EC_LEGACY_BIN pointing to a pre-negotiation Elsecall executable"]
fn real_legacy_cli_and_service_each_execute_once_on_the_original_attach() {
    use std::sync::atomic::Ordering;
    let legacy = PathBuf::from(std::env::var_os("EC_LEGACY_BIN").expect("set EC_LEGACY_BIN"));
    assert!(legacy.is_file());
    for old_service in [true, false] {
        let alpha = if old_service {
            TestMember::install("alpha", &legacy)
        } else {
            TestMember::start("alpha")
        };
        let bravo = if old_service {
            TestMember::install("bravo", &legacy)
        } else {
            TestMember::start("bravo")
        };
        enrol(&alpha, &bravo);
        let proxy = sealed_fragmentation::Proxy::start(&bravo, sealed_fragmentation::Fault::Pass);
        proxy.enroll(&alpha, &bravo);
        let repo = repository();
        let marker = bravo.home.path().join("executions");
        let script = format!(
            "echo executed >> '{}'; printf '\\000\\377out'; printf '\\377\\000err' >&2; exit 7",
            marker.display()
        );
        let executable = if old_service {
            Path::new(env!("CARGO_BIN_EXE_elsecall"))
        } else {
            legacy.as_path()
        };
        let output = crate::processes::command(executable)
            .env("HOME", alpha.home.path())
            .current_dir(&repo.root)
            .args(["run", "--member", "bravo", "--", "/bin/sh", "-c", &script])
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(7),
            "old service={old_service}: {output:?}"
        );
        assert_eq!(output.stdout, b"\0\xffout");
        assert_eq!(output.stderr, b"\xff\0err");
        assert_eq!(std::fs::read_to_string(marker).unwrap(), "executed\n");
        assert_eq!(proxy.attaches.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn stuck_raw_sends_expire_despite_incoming_heartbeats_and_queued_interrupts() {
    use std::sync::atomic::Ordering;
    let (alpha, bravo) = fleet();
    let proxy =
        sealed_fragmentation::Proxy::start(&bravo, sealed_fragmentation::Fault::StopUpstream);
    proxy.enroll(&alpha, &bravo);
    let repo = repository();
    let mut child = spawn(
        &alpha,
        &repo.root,
        &[
            "--member",
            "bravo",
            "--",
            "/bin/sh",
            "-c",
            "trap 'exit 42' INT; echo $$; cat > /dev/null",
        ],
        &[],
    );
    let (pid, _stdout) = first_line(&mut child);
    let pid: i32 = pid.parse().unwrap();
    proxy.release.store(true, Ordering::SeqCst);
    let mut input = child.stdin.take().unwrap();
    let writer = std::thread::spawn(move || {
        let _ = input.write_all(&vec![1; 16 * 1024 * 1024]);
    });
    until(
        "the upstream receiver to stop reading",
        crate::processes::WAIT,
        || proxy.selected.load(Ordering::SeqCst),
    );
    let before = proxy.received_records.load(Ordering::SeqCst);
    let start = Instant::now();
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGINT) }, 0);
    let output = child.wait_with_output().unwrap();
    writer.join().unwrap();
    assert_eq!(output.status.code(), Some(125), "{output:?}");
    let failure = envelope(&output);
    assert_eq!(failure["error"]["code"], "connection_lost", "{failure}");
    // The service watches the actual TCP send independently of the CLI's
    // Unix-send timer. Whichever detects first must leave a typed send timeout
    // in the caller-side peer history; a service close itself reaches CLI as EOF.
    let id = failure["error"]["run_id"].as_str().unwrap();
    let record = explain(&alpha, id);
    assert!(
        record["peer_io"]["attempts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["phase"] == "send" && a["failure_kind"] == "timeout"),
        "{record}"
    );
    assert!(
        start.elapsed() >= Duration::from_secs(9) && start.elapsed() < Duration::from_secs(20),
        "{:?}",
        start.elapsed()
    );
    assert!(
        proxy.received_records.load(Ordering::SeqCst) > before + 5,
        "incoming heartbeats must continue"
    );
    until(
        "the disconnected member command to stop",
        crate::processes::WAIT,
        || !alive(pid),
    );
    assert_eq!(proxy.attaches.load(Ordering::SeqCst), 1);
}

#[test]
fn authenticated_record_failure_during_output_return_keeps_the_confirmed_exit_and_warning() {
    use std::sync::atomic::Ordering;
    let (alpha, bravo) = fleet();
    let proxy = sealed_fragmentation::Proxy::start(&bravo, sealed_fragmentation::Fault::Mac);
    proxy.enroll(&alpha, &bravo);
    let repo = repository();
    std::fs::write(repo.root.join("payload.bin"), vec![b'x'; 524288]).unwrap();
    std::fs::write(
        repo.root.join(".elsecall.toml"),
        "[outputs]\npaths = [\"build/result.bin\"]\n",
    )
    .unwrap();
    let plan=serde_json::json!({"kind":"sequential","planned_units":1,"stop_policy":"continue_all","estimate_key":"a".repeat(64),"variant_key":"b".repeat(64)}).to_string();
    let script = format!(
        "'{}' workload observe --observation '{{\"status\":\"all_units_observed\",\"coverage\":{{\"attempted_units\":1,\"terminal_units\":1,\"failed_units\":1}}}}' >/dev/null || exit 99; mkdir -p build; cp payload.bin build/result.bin; printf '\\000\\377out'; exit 7",
        env!("CARGO_BIN_EXE_elsecall")
    );
    let status_file = repo.root.parent().unwrap().join("status.json");
    let output = run(
        &alpha,
        &repo.root,
        &[
            "--member",
            "bravo",
            "--workload",
            &plan,
            "--status-file",
            status_file.to_str().unwrap(),
            "--",
            "/bin/sh",
            "-c",
            &script,
        ],
        b"",
    );
    assert_eq!(output.status.code(), Some(7), "{output:?}");
    assert_eq!(output.stdout, b"\0\xffout");
    assert!(!repo.root.join("build/result.bin").exists());
    let status = status_of(&status_file);
    assert_eq!(status["data"]["exit_code"], 7, "{status}");
    assert!(
        !status["data"]["warnings"].as_array().unwrap().is_empty(),
        "{status}"
    );
    assert_eq!(proxy.attaches.load(Ordering::SeqCst), 1);
    let id = status["data"]["run_id"].as_str().unwrap();
    let caller = explain(&alpha, id);
    assert_eq!(caller["plan_observation"]["status"], "all_units_observed");
    assert_eq!(
        explain(&bravo, id)["plan_observation"],
        caller["plan_observation"]
    );
    assert_eq!(caller["outcome"]["exit_code"], 7, "{caller}");
    assert_eq!(
        caller["run_phases"]["remote_failure_phase"], "result_return",
        "{caller}"
    );
    assert!(caller["run_phases"]["execution_ms"].is_u64(), "{caller}");
    assert!(
        !caller["warnings"].as_array().unwrap().is_empty(),
        "{caller}"
    );
}

#[test]
fn a_member_raw_send_stall_is_bounded_while_caller_heartbeats_keep_arriving() {
    use std::sync::atomic::Ordering;
    let (alpha, bravo) = fleet();
    let proxy =
        sealed_fragmentation::Proxy::start(&bravo, sealed_fragmentation::Fault::StopDownstream);
    proxy.enroll(&alpha, &bravo);
    let repo = repository();
    std::fs::write(repo.root.join("payload.bin"), vec![7; 4 * 1024 * 1024]).unwrap();
    let mut child = spawn(
        &alpha,
        &repo.root,
        &[
            "--member",
            "bravo",
            "--",
            "/bin/sh",
            "-c",
            "echo $$; dd if=payload.bin bs=65536 2>/dev/null; sleep 60",
        ],
        &[],
    );
    let (pid, _stdout) = first_line(&mut child);
    let pid: i32 = pid.parse().unwrap();
    until(
        "the proxy's bounded downstream prefix",
        crate::processes::WAIT,
        || proxy.selected.load(Ordering::SeqCst),
    );
    let start = Instant::now();
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(125), "{output:?}");
    assert!(
        start.elapsed() >= Duration::from_secs(9) && start.elapsed() < Duration::from_secs(20),
        "{:?}",
        start.elapsed()
    );
    let failure = envelope(&output);
    let id = failure["error"]["run_id"].as_str().unwrap();
    until(
        "the member send watchdog to stop the command",
        crate::processes::WAIT,
        || !alive(pid),
    );
    assert_eq!(explain(&bravo, id)["outcome"]["code"], "connection_lost");
    assert_eq!(proxy.attaches.load(Ordering::SeqCst), 1);
}

#[test]
fn workload_contract_round_trips_over_the_authorized_run_stream() {
    use serde_json::json;
    let (alpha, bravo) = fleet();
    let repo = repository();
    let status_file = repo.root.parent().unwrap().join("workload-status.json");
    for (key, observation, ending, status) in [
        (
            'a',
            Some(
                json!({"status":"all_units_observed","coverage":{"attempted_units":3,"terminal_units":3,"failed_units":1}}),
            ),
            "exit 1",
            "all_units_observed",
        ),
        (
            'c',
            Some(
                json!({"status":"prefix_observed","coverage":{"attempted_units":1,"terminal_units":1,"failed_units":1},"reason":"failure"}),
            ),
            "exit 1",
            "prefix_observed",
        ),
        ('d', None, "exit 0", "unknown"),
        ('e', None, "kill -TERM $$", "censored"),
        (
            'f',
            Some(
                json!({"status":"all_units_observed","coverage":{"attempted_units":3,"terminal_units":3,"failed_units":0}}),
            ),
            "kill -TERM $$",
            "all_units_observed",
        ),
    ] {
        let plan = json!({"kind":"sequential","planned_units":3,"stop_policy":"stop_on_failure","estimate_key":key.to_string().repeat(64),"variant_key":"b".repeat(64)});
        let body = observation
            .map(|o| {
                format!(
                    "'{}' workload observe --observation '{}' >/dev/null || exit 99; {ending}",
                    env!("CARGO_BIN_EXE_elsecall"),
                    o
                )
            })
            .unwrap_or(ending.into());
        let output = run(
            &alpha,
            &repo.root,
            &[
                "--member",
                "bravo",
                "--label",
                "same display",
                "--workload",
                &plan.to_string(),
                "--status-file",
                status_file.to_str().unwrap(),
                "--",
                "/bin/sh",
                "-c",
                &body,
            ],
            b"",
        );
        if ending.contains("kill") {
            assert_eq!(output.status.signal(), Some(libc::SIGTERM), "{output:?}");
        } else {
            assert_eq!(
                output.status.code(),
                Some(if ending == "exit 0" { 0 } else { 1 }),
                "{output:?}"
            );
        }
        let result = status_of(&status_file);
        let id = result["data"]["run_id"].as_str().unwrap();
        let caller = explain(&alpha, id);
        let member = explain(&bravo, id);
        assert_eq!(caller["workload"], plan);
        assert_eq!(member["workload"], plan);
        assert_eq!(
            caller["workspace"]["repository"],
            member["workspace"]["repository"]
        );
        assert_eq!(caller["plan_observation"]["status"], status, "{caller}");
        assert_eq!(caller["plan_observation"], member["plan_observation"]);
        assert_eq!(caller["outcome"], member["outcome"]);
        assert!(caller["run_phases"]["execution_ms"].is_number());
    }
}

#[test]
#[ignore = "requires EC_LEGACY_BIN pointing to the pre-metadata production executable"]
fn workload_mixed_versions_preserve_legacy_runs_and_reject_structured_work_before_execution() {
    use std::sync::atomic::Ordering;
    let legacy = PathBuf::from(std::env::var_os("EC_LEGACY_BIN").expect("set EC_LEGACY_BIN"));
    for (old_caller_service, old_member, old_cli, structured) in [
        (true, true, false, false),
        (false, false, true, false),
        (false, true, false, true),
        (true, true, false, true),
    ] {
        let alpha = if old_caller_service {
            TestMember::start_with("alpha", "", &[("EC_TEST_BINARY", legacy.to_str().unwrap())])
        } else {
            TestMember::start("alpha")
        };
        let bravo = if old_member {
            TestMember::start_with("bravo", "", &[("EC_TEST_BINARY", legacy.to_str().unwrap())])
        } else {
            TestMember::start("bravo")
        };
        enrol(&alpha, &bravo);
        let proxy = sealed_fragmentation::Proxy::start(&bravo, sealed_fragmentation::Fault::Pass);
        proxy.enroll(&alpha, &bravo);
        let repo = repository();
        let marker = bravo.home.path().join("executions");
        let script = format!(
            "echo executed >> '{}'; printf 'legacy-output'; exit 7",
            marker.display()
        );
        let binary = if old_cli {
            legacy.as_path()
        } else {
            Path::new(env!("CARGO_BIN_EXE_elsecall"))
        };
        let mut command = processes::command(binary);
        command
            .env("HOME", alpha.home.path())
            .current_dir(&repo.root)
            .args(["run", "--member", "bravo"]);
        if structured {
            command.args(["--workload", r#"{"kind":"opaque"}"#]);
        }
        let output = command
            .args(["--", "/bin/sh", "-c", &script])
            .output()
            .unwrap();
        if structured {
            assert_eq!(output.status.code(), Some(125), "{output:?}");
            assert!(!marker.exists(), "unsupported metadata cannot execute");
            assert_eq!(
                proxy.attaches.load(Ordering::SeqCst),
                0,
                "no attach or silent legacy retry"
            );
        } else {
            assert_eq!(output.status.code(), Some(7), "{output:?}");
            assert_eq!(output.stdout, b"legacy-output");
            assert_eq!(std::fs::read_to_string(marker).unwrap(), "executed\n");
            assert_eq!(proxy.attaches.load(Ordering::SeqCst), 1);
        }
    }
}
