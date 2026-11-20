//! A placed run that fails because its member lacks a tool raises a
//! `missing_tool_pin` attention item on the member that placed it, naming the
//! run, the member, the tool and the file to pin it in, and the item clears once
//! the tool is pinned. Output passes through exactly as before.
use crate::members::{TestMember, enrol};
use serde_json::Value;
use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Output, Stdio},
};

const TOOL: &str = "ec-test-tool";

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

/// A committed repository, and a directory holding `TOOL`, which only the
/// caller's PATH reaches: the master has the tool, the member does not.
struct Setup {
    _temp: tempfile::TempDir,
    root: PathBuf,
    bin: PathBuf,
}

fn setup() -> Setup {
    let temp = tempfile::Builder::new()
        .prefix("ec-missing-")
        .tempdir_in("/tmp")
        .unwrap();
    let base = temp.path().canonicalize().unwrap();
    let root = base.join("acme-ios");
    std::fs::create_dir_all(&root).unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    std::fs::write(root.join("README.md"), "hello\n").unwrap();
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-q", "-m", "initial"]);
    let bin = base.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let tool = bin.join(TOOL);
    std::fs::write(&tool, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
    Setup {
        _temp: temp,
        root,
        bin,
    }
}

fn run(caller: &TestMember, setup: &Setup, args: &[&str]) -> Output {
    let path = format!(
        "{}:{}",
        setup.bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    crate::processes::command(env!("CARGO_BIN_EXE_elsecall"))
        .current_dir(&setup.root)
        .env("HOME", caller.home.path())
        .env("PATH", path)
        .arg("run")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn items(member: &TestMember) -> Vec<Value> {
    member.fleet()["attention"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["category"] == "missing_tool_pin")
        .cloned()
        .collect()
}

#[test]
fn a_placed_run_failing_on_a_missing_tool_asks_to_pin_it_until_pinned() {
    let alpha = TestMember::start("alpha");
    let bravo = TestMember::start("bravo");
    enrol(&alpha, &bravo);
    let setup = setup();

    // A typo, or a tool the master lacks too, is not a missing pin.
    let script = "printf 'zsh: command not found: ec-typo-tool\\n' >&2; exit 127";
    let output = run(
        &alpha,
        &setup,
        &["--member", "bravo", "--", "/bin/sh", "-c", script],
    );
    assert_eq!(output.status.code(), Some(127));
    assert!(items(&alpha).is_empty(), "{:?}", alpha.fleet()["attention"]);

    // The member's shell cannot find the tool: output and exit pass through as is.
    let line = format!("zsh: command not found: {TOOL}\n");
    let script = format!("echo building; printf '{}' >&2; exit 127", line.trim_end());
    let script = format!("{script}\n");
    let output = run(
        &alpha,
        &setup,
        &["--member", "bravo", "--", "/bin/sh", "-c", &script],
    );
    assert_eq!(output.status.code(), Some(127));
    assert_eq!(output.stdout, b"building\n");
    assert_eq!(
        output.stderr,
        line.trim_end().as_bytes(),
        "exactly what the command wrote"
    );

    let found = items(&alpha);
    assert_eq!(found.len(), 1, "{found:?}");
    let item = &found[0];
    assert_eq!(item["id"], format!("missing_tool_pin:bravo:{TOOL}"));
    assert_eq!(item["member"], "bravo");
    assert_eq!(item["subject"], TOOL);
    assert_eq!(item["code"], "missing_tool_pin");
    assert!(item["since"].is_string(), "{item}");
    let message = item["message"].as_str().unwrap();
    assert!(
        message.contains("failed on bravo") && message.contains(TOOL),
        "{item}"
    );
    let pin_in = setup.root.join("mise.toml");
    assert!(
        item["hint"]
            .as_str()
            .unwrap()
            .contains(&*pin_in.to_string_lossy()),
        "{item}"
    );
    assert_eq!(item["actions"][0]["run_on"], "alpha");
    assert_eq!(item["actions"][0]["person_only"], false);
    assert_eq!(item["actions"][1]["person_only"], true);
    assert!(
        items(&bravo).is_empty(),
        "the member that placed the run lists it"
    );

    // Nothing of the output is kept: only the tool's name, in the run's record.
    let records = std::fs::read_dir(alpha.data())
        .unwrap()
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("records"))
        .map(|entry| std::fs::read(entry.path()).unwrap_or_default())
        .collect::<Vec<_>>()
        .concat();
    assert!(
        !records
            .windows(b"command not found".len())
            .any(|w| w == b"command not found"),
        "no output line is stored"
    );

    // The command itself missing on the member names the tool too.
    let output = run(&alpha, &setup, &["--member", "bravo", "--", TOOL]);
    assert_eq!(output.status.code(), Some(125));
    let found = items(&alpha);
    assert_eq!(found.len(), 1, "one item per member and tool: {found:?}");
    assert_ne!(
        found[0]["message"], item["message"],
        "the latest run is named"
    );

    // Pinning the tool in the workspace clears it.
    std::fs::write(pin_in, format!("[tools]\n{TOOL} = \"1.0.0\"\n")).unwrap();
    assert!(items(&alpha).is_empty(), "{:?}", alpha.fleet()["attention"]);
}
