//! `elsecall uninstall` against a real installed service in a temporary home,
//! so the files it removes are the ones the service actually creates.
#![cfg(target_os = "macos")]
mod diagnostics;
mod members;
mod processes;

use members::{TestMember, json};
use serde_json::Value;
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

const TOOL: &str = "packslip:github.com/damian-kolasinski/elsecall";

fn names(directory: &Path) -> Vec<String> {
    let mut names: Vec<_> = fs::read_dir(directory)
        .map(|entries| {
            entries
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// Installs a member, asks its service for the things that open its stores, and
/// stops it with `uninstall`. No mise holds a request for it.
fn installed() -> TestMember {
    installed_with_mise(Path::new("/nonexistent/mise"))
}

fn installed_with_mise(mise: &Path) -> TestMember {
    let member = TestMember::install_with(
        "mac-mini",
        Path::new(env!("CARGO_BIN_EXE_elsecall")),
        "",
        &[("ELSECALL_MISE", mise.to_str().unwrap())],
    );
    member.ok(&["status"]);
    member.ok(&["runs"]);
    member.ok(&["copies"]);
    member
}

#[test]
fn uninstall_removes_everything_a_running_service_created() {
    let member = installed();
    let data = member.data();
    let created = names(&data);
    for expected in ["load.sqlite", "peer-io.sqlite", "copies.sqlite"] {
        assert!(
            created.iter().any(|name| name == expected),
            "{expected} missing from {created:?}"
        );
    }
    // The master's key that Add Mac authorized goes; the account's own stay.
    let ssh = member.home.path().join(".ssh");
    fs::create_dir_all(&ssh).unwrap();
    let ours = elsecall_add_mac::authorized_keys::line(
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIExampleMasterKey me@example.com",
        "mac-mini",
    )
    .unwrap();
    fs::write(
        ssh.join("authorized_keys"),
        format!("ssh-rsa AAAAOwnKey devbox@example.com\n{ours}\n"),
    )
    .unwrap();
    // Earlier builds kept onboarding operations here; nothing writes them now.
    fs::create_dir_all(data.join("onboarding")).unwrap();
    fs::write(data.join("onboarding/0123456789abcdef.json"), "{}").unwrap();
    let uninstalled = member.ok(&["uninstall", "--force"]);
    assert!(!data.exists(), "left behind: {:?}", names(&data));
    assert_eq!(uninstalled["status"], "uninstalled");
    assert!(uninstalled.get("kept").is_none(), "{uninstalled}");
    assert_eq!(uninstalled["mise"]["state"], "no_mise");
    assert_eq!(
        fs::read_to_string(ssh.join("authorized_keys")).unwrap(),
        "ssh-rsa AAAAOwnKey devbox@example.com\n"
    );
}

/// Uninstalls, which succeeds and names what it kept.
fn preserved(member: &TestMember) {
    let uninstalled = member.ok(&["uninstall", "--force"]);
    let data = member.data();
    assert_eq!(uninstalled["kept"]["directory"], data.to_str().unwrap());
    assert_eq!(
        uninstalled["kept"]["entries"],
        serde_json::to_value(names(&data)).unwrap()
    );
    // The installation itself is gone either way.
    assert!(!data.join("elsecall").exists());
    assert!(!data.join("member.key").exists());
}

#[test]
fn configuration_sync_backups_stay_with_the_state_that_indexes_them() {
    let member = installed();
    let data = member.data();
    let backups = data.join("config-sync/backups");
    elsecall_host::private_files::directory(&data.join("config-sync")).unwrap();
    elsecall_host::private_files::directory(&backups).unwrap();
    fs::write(backups.join("original"), b"this member's own version").unwrap();
    fs::write(data.join("config-sync/state.json"), b"{\"backups\":[]}").unwrap();
    preserved(&member);
    assert_eq!(names(&data), vec!["config-sync"]);
    let kept = names(&data.join("config-sync"));
    assert!(
        kept.contains(&"backups".into()) && kept.contains(&"state.json".into()),
        "{kept:?}"
    );
    assert_eq!(
        fs::read(backups.join("original")).unwrap(),
        b"this member's own version"
    );
}

#[test]
fn the_catalog_of_recorded_workspace_copies_stays_for_a_later_cleanup() {
    let member = installed();
    let data = member.data();
    elsecall_host::private_files::directory(&data.join("copies")).unwrap();
    fs::write(data.join("copies/copy.state"), b"{}").unwrap();
    preserved(&member);
    let kept = names(&data);
    assert!(
        kept.iter()
            .all(|name| name == "copies" || name.starts_with("copies.sqlite")),
        "{kept:?}"
    );
    assert!(kept.contains(&"copies.sqlite".into()), "{kept:?}");
}

#[test]
fn an_interrupted_rehearsal_stays_recoverable() {
    let member = installed();
    let data = member.data();
    fs::write(data.join("rehearsal-session.json"), b"{}").unwrap();
    preserved(&member);
    assert_eq!(names(&data), vec!["rehearsal-session.json"]);
}

/// How the fake mise behaves.
#[derive(Clone, Copy, PartialEq)]
enum Behaviour {
    Works,
    /// Has no packslip backend, so it refuses to list Elsecall's request.
    WithoutPackslip,
    ListFails,
    UnuseFails,
}

/// A mise that answers for Elsecall's request in `$HOME`'s global configuration,
/// removes it when asked, and logs its arguments, one call per line.
struct Mise {
    executable: PathBuf,
    calls: PathBuf,
    _directory: tempfile::TempDir,
}

impl Mise {
    fn new(behaviour: Behaviour) -> Self {
        let directory = tempfile::Builder::new()
            .prefix("ec-mise-")
            .tempdir_in("/tmp")
            .unwrap();
        let calls = directory.path().join("calls");
        let script = directory.path().join("mise.py");
        let python = |condition: bool| if condition { "True" } else { "False" };
        fs::write(
            &script,
            format!(
                r#"
import json, os, sys
from pathlib import Path
args = sys.argv[1:]
with open({calls:?}, "a") as log: log.write(" ".join(args) + "\n")
config = Path(os.environ["HOME"]) / ".config/mise/config.toml"
held = config.exists() and {tool:?} in config.read_text()
if args == ["backends", "ls"]:
    print("core\ngithub" + ("" if {without_packslip} else "\npackslip"))
elif args[:4] == ["ls", "--global", "--current", "--json"] and not {without_packslip} and not {list_fails}:
    # As mise lists a request whose release it does not have installed.
    print(json.dumps([{{"active": False, "source": {{"path": str(config)}}}}] if held else []))
elif args[0] == "unuse" and not {unuse_fails}:
    config.write_text("".join(l for l in config.read_text().splitlines(True) if {tool:?} not in l))
elif args[0] == "prune":
    pass
else:
    sys.exit(1)
"#,
                calls = calls.to_str().unwrap(),
                tool = TOOL,
                without_packslip = python(behaviour == Behaviour::WithoutPackslip),
                list_fails = python(behaviour == Behaviour::ListFails),
                unuse_fails = python(behaviour == Behaviour::UnuseFails),
            ),
        )
        .unwrap();
        let executable = directory.path().join("mise");
        fs::write(
            &executable,
            format!(
                "#!/bin/sh\nexec /usr/bin/python3 '{}' \"$@\"\n",
                script.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        Self {
            executable,
            calls,
            _directory: directory,
        }
    }

    fn calls(&self) -> Vec<String> {
        fs::read_to_string(&self.calls)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

/// A member whose global mise configuration requests Elsecall beside another
/// tool, as the one-command installer leaves it, and its configuration's path.
fn installed_by_mise(mise: &Mise) -> (TestMember, PathBuf) {
    let member = installed_with_mise(&mise.executable);
    let config = member.home.path().join(".config/mise/config.toml");
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    fs::write(
        &config,
        format!("[tools]\nnode = \"22\"\n\"{TOOL}\" = \"0.2.2\"\n"),
    )
    .unwrap();
    fs::set_permissions(&config, fs::Permissions::from_mode(0o644)).unwrap();
    (member, config)
}

#[test]
fn uninstall_removes_the_request_that_keeps_the_cli_on_the_path() {
    let mise = Mise::new(Behaviour::Works);
    let (member, config) = installed_by_mise(&mise);
    let uninstalled = member.ok(&["uninstall", "--force"]);
    assert_eq!(
        uninstalled["mise"],
        serde_json::json!({"state": "removed", "config": config, "version": "0.2.2", "pruned": true})
    );
    let calls = mise.calls();
    let unuse = format!("unuse --no-prune --path {} {TOOL}", config.display());
    let prune = format!("prune --yes --tools {TOOL}");
    let at = |call: &str| calls.iter().position(|made| made == call);
    assert!(at(&unuse) < at(&prune) && at(&unuse).is_some(), "{calls:?}");
    // Only Elsecall's request goes.
    assert_eq!(
        fs::read_to_string(&config).unwrap(),
        "[tools]\nnode = \"22\"\n"
    );
    assert!(!member.data().exists());
    // Nothing is left for a repeated uninstall to remove.
    assert_eq!(
        member.ok(&["uninstall"])["mise"],
        serde_json::json!({"state": "absent"})
    );
}

#[test]
fn keep_mise_leaves_the_request_alone() {
    let mise = Mise::new(Behaviour::Works);
    let (member, config) = installed_by_mise(&mise);
    let uninstalled = member.ok(&["uninstall", "--force", "--keep-mise"]);
    assert_eq!(uninstalled["mise"], serde_json::json!({"state": "kept"}));
    assert!(fs::read_to_string(&config).unwrap().contains(TOOL));
    assert_eq!(mise.calls(), Vec::<String>::new());
    assert!(!member.data().exists());
}

#[test]
fn a_refused_uninstall_leaves_mise_alone() {
    let mise = Mise::new(Behaviour::Works);
    let (member, config) = installed_by_mise(&mise);
    // Enrolled in a fleet, which needs --force.
    let membership = member.data().join("fleet.json");
    if !membership.exists() {
        fs::write(&membership, b"{}").unwrap();
    }
    let output = member.run(&["uninstall"]);
    assert_eq!(json(&output)["error"]["code"], "fleet_member");
    assert_eq!(mise.calls(), Vec::<String>::new());
    assert!(fs::read_to_string(&config).unwrap().contains(TOOL));
}

#[test]
fn a_mise_without_packslip_holds_no_request_that_resolves() {
    let mise = Mise::new(Behaviour::WithoutPackslip);
    let (member, _) = installed_by_mise(&mise);
    let uninstalled = member.ok(&["uninstall", "--force"]);
    assert_eq!(uninstalled["mise"], serde_json::json!({"state": "absent"}));
    assert!(!member.data().exists());
}

/// Uninstalls with a mise that fails, which exits 1 once the installation is gone.
fn failed_mise(mise: &Mise) -> (Value, PathBuf) {
    let (member, config) = installed_by_mise(mise);
    let output = member.run(&["uninstall", "--force"]);
    assert_eq!(output.status.code(), Some(1));
    let data: Value = json(&output)["data"].clone();
    assert_eq!(data["status"], "uninstalled");
    assert_eq!(data["mise"]["state"], "failed");
    assert_eq!(data["mise"]["error"]["code"], "mise_request_kept");
    assert!(!member.data().exists());
    (data["mise"]["error"].clone(), config)
}

#[test]
fn a_request_mise_cannot_remove_is_reported_after_the_installation_is_gone() {
    let mise = Mise::new(Behaviour::UnuseFails);
    let (error, config) = failed_mise(&mise);
    assert_eq!(
        error["hint"],
        format!("Run mise unuse --path {} {TOOL}.", config.display())
    );
}

#[test]
fn a_request_mise_cannot_list_is_reported_with_the_global_command() {
    let mise = Mise::new(Behaviour::ListFails);
    let (error, _) = failed_mise(&mise);
    assert_eq!(error["hint"], format!("Run mise unuse --global {TOOL}."));
    assert!(
        !error["message"].as_str().unwrap().contains("update"),
        "{error}"
    );
}
