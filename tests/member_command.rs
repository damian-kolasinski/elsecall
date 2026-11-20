#![cfg(target_os = "macos")]
//! `elsecall run --member` from outside a git working tree, between real member
//! services in separate temporary homes: the command runs on the required member,
//! in its home directory with no copy, and behaves as if it ran locally.
//! Placeable work outside a working tree still runs on the calling member.
use crate::members::{TestMember, enrol};
use serde_json::Value;
use std::{
    io::{BufRead, BufReader, Read, Write},
    os::unix::process::ExitStatusExt,
    path::Path,
    process::{Child, Output, Stdio},
};

fn spawn(caller: &TestMember, cwd: &Path, args: &[&str]) -> Child {
    crate::processes::command(env!("CARGO_BIN_EXE_elsecall"))
        .current_dir(cwd)
        .env("HOME", caller.home.path())
        .arg("run")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

fn run(caller: &TestMember, cwd: &Path, args: &[&str], input: &[u8]) -> Output {
    let mut child = spawn(caller, cwd, args);
    let mut stdin = child.stdin.take().unwrap();
    let input = input.to_vec();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&input);
    });
    let output = child.wait_with_output().unwrap();
    writer.join().unwrap();
    output
}

/// A directory outside any git working tree.
fn outside() -> tempfile::TempDir {
    let directory = tempfile::tempdir().unwrap();
    let inside = crate::processes::command("git")
        .current_dir(directory.path())
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
        .unwrap();
    assert!(
        !inside.status.success(),
        "the test directory is in a working tree"
    );
    directory
}

fn canonical(path: &Path) -> String {
    path.canonicalize().unwrap().to_str().unwrap().to_owned()
}

fn run_id(output: &Output) -> String {
    let status = String::from_utf8_lossy(&output.stdout);
    status
        .lines()
        .find_map(|line| line.strip_prefix("run "))
        .unwrap_or_else(|| panic!("no run ID in {output:?}"))
        .to_owned()
}

#[test]
fn a_required_member_runs_work_from_outside_a_working_tree_in_its_home() {
    let alpha = TestMember::start("alpha");
    let bravo = TestMember::start("bravo");
    enrol(&alpha, &bravo);
    let directory = outside();
    let output = run(
        &alpha,
        directory.path(),
        &[
            "--member",
            "bravo",
            "--",
            "/bin/sh",
            "-c",
            "echo \"run $ELSECALL_RUN_ID\"; pwd -P; echo \"$ELSECALL_MEMBER $ELSECALL_CALLER_MEMBER\"; \
             echo \"copy ${ELSECALL_WORKSPACE_COPY-none} ${ELSECALL_CALLER_WORKSPACE-none}\"; \
             cat; echo to-stderr >&2; exit 3",
        ],
        b"from the caller\n",
    );
    assert_eq!(output.status.code(), Some(3), "{output:?}");
    let stdout = String::from_utf8(output.stdout.clone()).unwrap();
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines[1], canonical(bravo.home.path()), "{stdout}");
    assert_eq!(lines[2], "bravo alpha");
    assert_eq!(lines[3], "copy none none", "no workspace variables");
    assert_eq!(lines[4], "from the caller", "standard input passes through");
    assert_eq!(String::from_utf8_lossy(&output.stderr), "to-stderr\n");

    let id = run_id(&output);
    let caller = alpha.ok(&["explain", &id]);
    assert_eq!(caller["member"], "bravo", "{caller}");
    assert_eq!(caller["placement_note"], Value::Null, "{caller}");
    assert_eq!(caller["placed"]["location"], "home", "{caller}");
    assert_eq!(caller["placed"]["caller_workspace"], Value::Null);
    assert_eq!(caller["placed"]["sync"], Value::Null, "nothing was synced");
    assert_eq!(caller["placed"]["exit_code"], 3);
    let member = bravo.ok(&["explain", &id]);
    assert_eq!(member["placed"]["side"], "member", "{member}");
    assert_eq!(member["placed"]["location"], "home");
    assert_eq!(member["workspace"], Value::Null);
}

#[test]
fn signals_reach_a_command_required_on_a_member_from_outside_a_working_tree() {
    let alpha = TestMember::start("alpha");
    let bravo = TestMember::start("bravo");
    enrol(&alpha, &bravo);
    let directory = outside();
    let mut child = spawn(
        &alpha,
        directory.path(),
        &[
            "--member",
            "bravo",
            "--",
            "/bin/sh",
            "-c",
            "trap 'echo trapped; exit 42' INT; echo \"$ELSECALL_MEMBER\"; while :; do sleep 0.1; done",
        ],
    );
    let mut reader = BufReader::new(child.stdout.take().unwrap());
    let mut first = String::new();
    reader.read_line(&mut first).unwrap();
    assert_eq!(first, "bravo\n");
    // SAFETY: kill has no memory-safety preconditions.
    unsafe { libc::kill(child.id() as i32, libc::SIGINT) };
    let status = child.wait().unwrap();
    let mut rest = String::new();
    reader.read_to_string(&mut rest).unwrap();
    assert_eq!(rest, "trapped\n");
    assert_eq!(status.code(), Some(42), "{status:?}");
    assert_eq!(status.signal(), None);
}

#[test]
fn placeable_work_from_outside_a_working_tree_stays_on_the_calling_member() {
    let alpha = TestMember::start("alpha");
    let bravo = TestMember::start("bravo");
    enrol(&alpha, &bravo);
    let directory = outside();
    let output = run(
        &alpha,
        directory.path(),
        &[
            "--placeable",
            "--",
            "/bin/sh",
            "-c",
            "echo \"run $ELSECALL_RUN_ID\"; echo \"$ELSECALL_MEMBER\"; pwd -P",
        ],
        b"",
    );
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout.clone()).unwrap();
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines[1], "alpha");
    assert_eq!(
        lines[2],
        canonical(directory.path()),
        "in the caller's directory"
    );
    let record = alpha.ok(&["explain", &run_id(&output)]);
    assert!(
        record["placement_note"]
            .as_str()
            .unwrap()
            .contains("outside a git working tree"),
        "{record}"
    );
}
