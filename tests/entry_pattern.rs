#![cfg(target_os = "macos")]
//! The README's entry pattern, a script that re-execs itself through Elsecall at
//! its top, run as a consumer would: the README's guard and entry lines verbatim,
//! the real CLI, and real member services where admission is involved. Each mode
//! must run the script's body exactly once, run its pre-admission check once, and
//! let a signal to the script's original PID stop it.
mod members;
mod processes;

use members::{TestMember, enrol};
use serde_json::Value;
use std::{
    io::{BufRead, BufReader, Read},
    os::unix::{fs::PermissionsExt, process::ExitStatusExt},
    path::{Path, PathBuf},
    process::{Child, Stdio},
    time::{Duration, Instant},
};

const README: &str = include_str!("../README.md");

/// The README's `sh` block that starts with `first_line`.
fn readme_block(first_line: &str) -> &'static str {
    README
        .split("```sh\n")
        .skip(1)
        .map(|block| block.split("\n```").next().unwrap())
        .find(|block| block.starts_with(first_line))
        .unwrap_or_else(|| panic!("README has no sh block starting with {first_line}"))
}

/// The README's line in `block` that starts with `prefix`.
fn readme_line(block: &str, prefix: &str) -> String {
    block
        .lines()
        .find(|line| line.starts_with(prefix))
        .unwrap_or_else(|| panic!("README block has no line starting with {prefix}"))
        .to_owned()
}

/// The wrapper: the README's check and entry lines between test probes. `$PLACE`
/// adds placement options to the entry line, empty except for the placed run.
/// The depth limit turns a wrapper that wraps itself forever into exit 99.
fn wrapper() -> String {
    let entry = readme_block("#!/bin/sh\n# Scripts/build.sh");
    let check = readme_line(entry, r#"[ -n "${ELSECALL_ADMISSION:-}" ] || check_inputs"#);
    let line = readme_line(
        entry,
        r#"[ -n "${ELSECALL_ADMISSION:-}" ] || [ ! -x "$EC" ]"#,
    );
    assert!(line.contains(r#"exec "$EC" run --label"#), "{line}");
    let line = line.replace(r#""$EC" run "#, r#""$EC" run $PLACE "#);
    format!(
        r#"#!/bin/sh
echo "start ${{ELSECALL_ADMISSION:-none}} $$"
DEPTH=$((${{DEPTH:-0}} + 1)); export DEPTH; [ "$DEPTH" -le 3 ] || exit 99
check_inputs() {{ echo check; }}
. Scripts/lib/elsecall.sh
{check}
{line}
echo "body ${{ELSECALL_ADMISSION:-none}} ${{ELSECALL_MEMBER:--}}"
ec run --label inner -- /bin/sh -c 'echo "inner $ELSECALL_ADMISSION ${{ELSECALL_MEMBER:--}}"'
[ "$2" != hang ] || {{ echo "hanging $$"; while :; do sleep 0.05; done; }}
exit 3
"#
    )
}

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

/// A consumer repository with the README's guard as `Scripts/lib/elsecall.sh`, the
/// wrapper as `Scripts/build.sh`, and running anyway chosen.
struct Consumer {
    _temp: tempfile::TempDir,
    root: PathBuf,
    /// A directory holding only `elsecall`, for PATH.
    bin: PathBuf,
}
fn consumer() -> Consumer {
    let temp = tempfile::Builder::new()
        .prefix("ec-entry-")
        .tempdir_in("/tmp")
        .unwrap();
    let base = temp.path().canonicalize().unwrap();
    let bin = base.join("bin");
    std::fs::create_dir(&bin).unwrap();
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_elsecall"), bin.join("elsecall")).unwrap();
    let root = base.join("app");
    std::fs::create_dir_all(root.join("Scripts/lib")).unwrap();
    std::fs::write(
        root.join("Scripts/lib/elsecall.sh"),
        format!(
            "{}\n",
            readme_block("# Scripts/lib/elsecall.sh, sourced by the repository's scripts.")
        ),
    )
    .unwrap();
    let script = root.join("Scripts/build.sh");
    std::fs::write(&script, wrapper()).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(
        root.join(".elsecall.toml"),
        "[admission]\nwhen_unavailable = \"run\"\n",
    )
    .unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-q", "-m", "initial"]);
    Consumer {
        _temp: temp,
        root,
        bin,
    }
}

fn empty_home() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("ec-")
        .tempdir_in("/tmp")
        .unwrap()
}

/// How the test invokes the wrapper.
struct Mode<'a> {
    home: &'a Path,
    /// Whether `elsecall` is on PATH; otherwise the guard finds nothing.
    installed: bool,
    environment: &'a [(&'a str, &'a str)],
}

/// Starts `Scripts/build.sh App EXTRA...` from the repository root, as an agent
/// would: by a relative path, in a process whose PID is the wrapper's. Nothing of
/// the test's own environment leaks in, even when the tests run under Elsecall.
fn start(consumer: &Consumer, mode: &Mode, extra: &[&str]) -> Child {
    let path = match mode.installed {
        true => format!("{}:/usr/bin:/bin", consumer.bin.display()),
        false => "/usr/bin:/bin".to_owned(),
    };
    crate::processes::command("/bin/sh")
        .args(["-c", r#"exec Scripts/build.sh "$@""#, "sh", "App"])
        .args(extra)
        .current_dir(&consumer.root)
        .env_clear()
        .env("HOME", mode.home)
        .env("PATH", path)
        .env("TMPDIR", std::env::temp_dir())
        .envs(mode.environment.iter().copied())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

/// Runs the wrapper to its end: its stdout lines and its exit code. A `start`
/// line ends in `PID` for the wrapper's original process, `child` for another.
fn run(consumer: &Consumer, mode: &Mode) -> (Vec<String>, i32) {
    let child = start(consumer, mode, &[]);
    let pid = child.id().to_string();
    let output = child.wait_with_output().unwrap();
    let lines = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| match line.rsplit_once(' ') {
            Some((start, process)) if line.starts_with("start ") => {
                let process = if process == pid { "PID" } else { "child" };
                format!("{start} {process}")
            }
            _ => line.to_owned(),
        })
        .collect();
    let code = output.status.code().unwrap_or_else(|| {
        panic!(
            "killed by a signal: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    });
    (lines, code)
}

fn alive(pid: i32) -> bool {
    // SAFETY: kill has no memory-safety preconditions; signal 0 only checks.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Starts the wrapper so that it hangs in its body, sends `signal` to the wrapper's
/// original PID, and checks that the hanging body stops and the original process
/// dies of the same signal.
fn signal_stops_it(consumer: &Consumer, mode: &Mode, signal: i32) {
    let mut child = start(consumer, mode, &["hang"]);
    let original = child.id() as i32;
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut seen = String::new();
    let body = loop {
        let mut line = String::new();
        if stdout.read_line(&mut line).unwrap() == 0 {
            let mut stderr = String::new();
            child
                .stderr
                .take()
                .unwrap()
                .read_to_string(&mut stderr)
                .unwrap();
            panic!("the wrapper ended before hanging: {seen}{stderr}");
        }
        seen.push_str(&line);
        if let Some(pid) = line.trim().strip_prefix("hanging ") {
            break pid.parse::<i32>().unwrap();
        }
    };
    // SAFETY: kill has no memory-safety preconditions.
    unsafe { libc::kill(original, signal) };
    let status = child.wait().unwrap();
    assert_eq!(status.signal(), Some(signal), "{status:?}; output {seen}");
    let deadline = Instant::now() + crate::processes::WAIT;
    while alive(body) {
        assert!(Instant::now() < deadline, "the wrapper's body still runs");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn runs(member: &TestMember) -> Vec<Value> {
    member.ok(&["runs", "--limit", "100"])["runs"]
        .as_array()
        .unwrap()
        .clone()
}

fn labelled<'a>(runs: &'a [Value], label: &str) -> Vec<&'a Value> {
    runs.iter().filter(|r| r["label"] == label).collect()
}

#[test]
fn the_entry_line_runs_the_script_once_in_every_mode_and_signals_stop_it() {
    let consumer = consumer();

    // Managed: one admitted request for the whole script; calls inside it nest.
    let alpha = TestMember::start("alpha");
    let managed = Mode {
        home: alpha.home.path(),
        installed: true,
        environment: &[],
    };
    let (lines, code) = run(&consumer, &managed);
    assert_eq!(
        lines,
        [
            "start none PID",
            "check",
            "start granted child",
            "body granted alpha",
            "inner nested alpha",
        ]
    );
    assert_eq!(code, 3);
    let recorded = runs(&alpha);
    assert_eq!(labelled(&recorded, "build App").len(), 1, "{recorded:?}");
    signal_stops_it(&consumer, &managed, libc::SIGHUP);
    signal_stops_it(&consumer, &managed, libc::SIGTERM);

    // ELSECALL=off: no service is involved, and the marker still stops the re-exec.
    let home = empty_home();
    let off = Mode {
        home: home.path(),
        installed: true,
        environment: &[("ELSECALL", "off")],
    };
    let (lines, code) = run(&consumer, &off);
    assert_eq!(
        lines,
        [
            "start none PID",
            "check",
            "start off child",
            "body off -",
            "inner off -",
        ]
    );
    assert_eq!(code, 3);
    signal_stops_it(&consumer, &off, libc::SIGHUP);

    // Unmanaged: the service cannot be reached and the consumer chose to run anyway.
    let unmanaged = Mode {
        home: home.path(),
        installed: true,
        environment: &[],
    };
    let (lines, code) = run(&consumer, &unmanaged);
    assert_eq!(
        lines,
        [
            "start none PID",
            "check",
            "start unmanaged child",
            "body unmanaged -",
            "inner unmanaged -",
        ]
    );
    assert_eq!(code, 3);
    signal_stops_it(&consumer, &unmanaged, libc::SIGTERM);

    // Not installed: the script continues in place, and `ec` marks what it runs.
    let missing = Mode {
        home: home.path(),
        installed: false,
        environment: &[],
    };
    let (lines, code) = run(&consumer, &missing);
    assert_eq!(
        lines,
        ["start none PID", "check", "body none -", "inner missing -"]
    );
    assert_eq!(code, 3);
    signal_stops_it(&consumer, &missing, libc::SIGTERM);
}

#[test]
fn a_placed_script_does_not_wrap_itself_again_on_the_member() {
    let consumer = consumer();
    let path = format!("{}:/usr/bin:/bin", consumer.bin.display());
    // Placed commands on bravo find `elsecall` through the PATH of its service, so
    // a wrapper without the marker there would wrap itself again.
    let alpha = TestMember::start("alpha");
    let bravo = TestMember::start_with("bravo", "", &[("PATH", &path)]);
    enrol(&alpha, &bravo);
    let placed = Mode {
        home: alpha.home.path(),
        installed: true,
        environment: &[("PLACE", "--member bravo")],
    };
    let (lines, code) = run(&consumer, &placed);
    assert_eq!(
        lines,
        [
            "start none PID",
            "check",
            "start granted child",
            "body granted bravo",
            "inner nested bravo",
        ]
    );
    assert_eq!(code, 3);
    let recorded = runs(&alpha);
    let build = labelled(&recorded, "build App");
    assert_eq!(build.len(), 1, "{recorded:?}");
    assert_eq!(build[0]["member"], "bravo");
    let on_bravo = runs(&bravo);
    assert_eq!(labelled(&on_bravo, "build App").len(), 1, "{on_bravo:?}");
    assert_eq!(labelled(&on_bravo, "inner").len(), 1, "{on_bravo:?}");

    signal_stops_it(&consumer, &placed, libc::SIGHUP);
}

/// The repository's actual mise entry scripts use the same consumer contract as
/// the documented example. A tiny cargo executable stands in for the toolchain;
/// admission, policy lookup and script re-entry still use the real service/CLI.
#[test]
fn development_tasks_admit_once_and_preserve_tool_failure() {
    let consumer = consumer();
    std::fs::create_dir_all(consumer.root.join("scripts/lib")).unwrap();
    for (name, content) in [
        (
            "scripts/lib/elsecall.sh",
            include_str!("../scripts/lib/elsecall.sh"),
        ),
        ("scripts/check.sh", include_str!("../scripts/check.sh")),
        ("scripts/build.sh", include_str!("../scripts/build.sh")),
        ("scripts/perf.sh", include_str!("../scripts/perf.sh")),
        (
            "scripts/test-environment.py",
            include_str!("../scripts/test-environment.py"),
        ),
        (".elsecall.toml", include_str!("../.elsecall.toml")),
    ] {
        let path = consumer.root.join(name);
        std::fs::write(&path, content).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let cargo = consumer.bin.join("cargo");
    std::fs::write(&cargo, "#!/bin/sh\nprintf '%s:%s:%s\\n' \"${ELSECALL_ADMISSION:-clean}\" \"${EC_TEST_PERF:-0}\" \"$*\"\n[ \"$1\" != clippy ] || exit \"${LINT_EXIT:-0}\"\n").unwrap();
    std::fs::set_permissions(cargo, std::fs::Permissions::from_mode(0o755)).unwrap();
    let member = TestMember::start("developer");
    // The policy must fit this fixture independent of the host's core count.
    // Real host sizing is measured separately; here preserve exact label rules.
    let mut policy: toml::Value = toml::from_str(include_str!("../.elsecall.toml")).unwrap();
    for rule in policy["labels"].as_array_mut().unwrap() {
        rule["size"] = toml::Value::Integer(1);
    }
    std::fs::write(
        consumer.root.join(".elsecall.toml"),
        toml::to_string(&policy).unwrap(),
    )
    .unwrap();
    let invoke = |task: &str, home: &Path, mode: Option<&str>, lint_exit: &str| {
        let mut command = crate::processes::command("/bin/sh");
        command
            .arg(format!("scripts/{task}.sh"))
            .current_dir(&consumer.root)
            .env("HOME", home)
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    consumer.bin.display(),
                    std::env::var("PATH").unwrap()
                ),
            )
            .env("LINT_EXIT", lint_exit);
        if let Some(mode) = mode {
            command.env("ELSECALL", mode);
        }
        command.output().unwrap()
    };
    for (task, expected) in [
        (
            "check",
            "granted:0:fmt --all -- --check\ngranted:0:clippy --workspace --locked --all-targets -- -D warnings\nclean:0:test --workspace --locked\n",
        ),
        ("build", "granted:0:build --locked --release\n"),
        (
            "perf",
            "clean:1:test --locked --test workspace_copy --test placement --test pre_run_sync --test remote_run --test rehearse perf_ -- --test-threads=1 --nocapture\n",
        ),
    ] {
        let output = invoke(task, member.home.path(), None, "0");
        assert!(output.status.success(), "{output:?}");
        assert_eq!(String::from_utf8(output.stdout).unwrap(), expected);
    }
    let recorded = runs(&member);
    assert_eq!(
        labelled(&recorded, "elsecall check").len(),
        2,
        "{recorded:?}"
    );
    assert_eq!(
        labelled(&recorded, "elsecall build").len(),
        1,
        "{recorded:?}"
    );
    let output = invoke("check", member.home.path(), None, "7");
    assert_eq!(output.status.code(), Some(7), "{output:?}");
    assert!(
        !String::from_utf8(output.stdout)
            .unwrap()
            .contains("test --locked")
    );
    let home = empty_home();
    for (mode, admission) in [(Some("off"), "off"), (None, "unmanaged")] {
        let output = invoke("build", home.path(), mode, "0");
        assert!(output.status.success(), "{output:?}");
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            format!("{admission}:0:build --locked --release\n")
        );
    }
}
