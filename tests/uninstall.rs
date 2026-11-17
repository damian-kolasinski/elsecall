//! `elsecall uninstall` against a real installed service in a temporary home,
//! so the files it removes are the ones the service actually creates.
#![cfg(target_os = "macos")]
mod members;
mod processes;

use members::{TestMember, json};
use std::{fs, path::Path};

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
/// stops it with `uninstall`.
fn installed() -> TestMember {
    let member = TestMember::install("mac-mini", Path::new(env!("CARGO_BIN_EXE_elsecall")));
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
    let output = member.run(&["uninstall", "--force"]);
    assert!(output.status.success(), "{}", json(&output));
    assert!(!data.exists(), "left behind: {:?}", names(&data));
}

fn preserved(member: &TestMember) {
    let output = member.run(&["uninstall", "--force"]);
    assert!(!output.status.success());
    assert_eq!(json(&output)["error"]["code"], "files_preserved");
    // The installation itself is gone either way.
    assert!(!member.data().join("elsecall").exists());
    assert!(!member.data().join("member.key").exists());
}

#[test]
fn configuration_sync_backups_stay_with_the_state_that_indexes_them() {
    let member = installed();
    let data = member.data();
    let backups = data.join("config-sync/backups");
    elsecall::private_files::directory(&data.join("config-sync")).unwrap();
    elsecall::private_files::directory(&backups).unwrap();
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
    elsecall::private_files::directory(&data.join("copies")).unwrap();
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
