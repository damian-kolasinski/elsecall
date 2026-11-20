//! A background-session installation and its boot starter, as install, doctor
//! and `service start-at-boot` report them. These never reach sudo: the starter
//! itself is installed only in a VM (docs/e2e-vm.md, tests/openssh_mac.rs).
#![cfg(target_os = "macos")]
use crate::members::TestMember;
use std::path::Path;

const NEXT_STEP: &str = "elsecall service start-at-boot";

#[test]
fn a_background_installation_without_its_starter_says_it_will_not_survive_a_restart() {
    let member = TestMember::install("alpha", Path::new(env!("CARGO_BIN_EXE_elsecall")));
    // This test's home is not the account's, so the account has no starter
    // for it: none at all, or one for its real installation, which is foreign
    // here and never this installation's to replace.
    // A loaded test machine can start a debug build slower than install
    // waits; other tests retry the same way.
    let mut install = member.run(&["install"]);
    for _ in 0..3 {
        if crate::members::json(&install)["error"]["code"] != "startup_failed" {
            break;
        }
        install = member.run(&["install"]);
    }
    let document = crate::members::json(&install);
    assert!(install.status.success(), "{document}");
    let updated = document["data"].clone();
    assert_eq!(updated["session"], "background", "{updated}");
    let foreign = updated["boot_start"] == "foreign";
    if !foreign {
        assert_eq!(updated["boot_start"], "missing", "{updated}");
    }
    let hinted =
        |next_step: &serde_json::Value| next_step.as_str().is_some_and(|s| s.contains(NEXT_STEP));
    assert_eq!(hinted(&updated["next_step"]), !foreign, "{updated}");

    let doctor = crate::members::json(&member.run(&["doctor"]))["data"].clone();
    assert_eq!(doctor["session"], "background", "{doctor}");
    assert_eq!(doctor["boot_start"], updated["boot_start"], "{doctor}");
    assert_eq!(hinted(&doctor["next_step"]), !foreign, "{doctor}");

    // Without a password on standard input it refuses before running sudo,
    // and it never replaces another installation's starter.
    assert_eq!(
        member.fails(&["service", "start-at-boot"]),
        if foreign {
            "boot_start_foreign"
        } else {
            "password_required"
        }
    );
    // The uninstall keeps only a starter of its own.
    let uninstalled = member.ok(&["uninstall", "--force"]);
    assert!(uninstalled["boot_start_kept"].is_null(), "{uninstalled}");
}

#[test]
fn start_at_boot_needs_an_installation() {
    let home = tempfile::Builder::new()
        .prefix("ec-")
        .tempdir_in("/tmp")
        .unwrap();
    let output = crate::processes::command(env!("CARGO_BIN_EXE_elsecall"))
        .env("HOME", home.path())
        .current_dir(home.path())
        .args(["service", "start-at-boot"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        crate::members::json(&output)["error"]["code"],
        "not_installed"
    );
}
