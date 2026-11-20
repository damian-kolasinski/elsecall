//! Exceptions to the pins on a real local service: solo master, hand edits, the
//! refused desired-state shape, and a fleet.
#![cfg(target_os = "macos")]
use crate::members;

use members::{TestMember, enrol};
use serde_json::{Value, json};
use std::os::unix::fs::{MetadataExt, PermissionsExt};

fn file(member: &TestMember) -> std::path::PathBuf {
    member.data().join("fleet-tools.toml")
}

#[test]
fn a_solo_master_writes_exceptions_and_keeps_hand_edits() {
    let alpha = TestMember::start("alpha");
    let empty = alpha.ok(&["tools", "exceptions"]);
    assert_eq!(
        empty,
        json!({
            "members": [{"member": "alpha", "exceptions": []}],
            "unknown_members": []
        })
    );
    assert!(!file(&alpha).exists());

    alpha.ok(&["tools", "leave-out", "mise:sentry-cli"]);
    alpha.ok(&["tools", "set", "xcode", "26.3", "--member", "alpha"]);
    alpha.ok(&[
        "tools",
        "set",
        "mise:tuist",
        "4.205.0",
        "--member",
        "alpha",
        "--install-only",
    ]);
    alpha.ok(&["tools", "leave-out", "simulators:iOS", "--member", "alpha"]);
    let exceptions = alpha.ok(&["tools", "exceptions"]);
    assert_eq!(
        exceptions,
        json!({
            "members": [{
                "member": "alpha",
                "exceptions": [
                    {"manager": "mise", "tool": "sentry-cli", "key": "defaults.mise.tools.sentry-cli",
                     "kind": "leave_out"},
                    {"manager": "mise", "tool": "tuist", "key": "members.alpha.mise.tools.tuist",
                     "kind": "versions", "default": null, "install": ["4.205.0"]},
                    {"manager": "xcodes", "tool": "xcode", "key": "members.alpha.xcodes.tools.xcode",
                     "kind": "versions", "default": "26.3", "install": []},
                    {"manager": "simulators", "tool": "iOS", "key": "members.alpha.simulators.tools.iOS",
                     "kind": "leave_out"}
                ]
            }],
            "unknown_members": []
        })
    );
    let path = file(&alpha);
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);

    let mut text = std::fs::read_to_string(&path).unwrap();
    text.push_str("\n# operator note\n[defaults.mise.tools.bun]\n");
    text.push_str("[members.other-mac.mise.tools.node]\ndefault = \"20.0.0\"\n");
    std::fs::write(&path, &text).unwrap();
    alpha.ok(&["tools", "set", "xcode", "26.4", "--member", "alpha"]);
    let kept = std::fs::read_to_string(&path).unwrap();
    for expected in ["# operator note", "bun", "26.4", "other-mac"] {
        assert!(kept.contains(expected), "{expected}: {kept}");
    }
    let after = alpha.ok(&["tools", "exceptions", "--member", "alpha"]);
    assert_eq!(after["unknown_members"], json!(["other-mac"]));
    assert_eq!(after["members"][0]["exceptions"][0]["tool"], "bun");

    alpha.ok(&["tools", "unset", "mise:sentry-cli"]);
    alpha.ok(&[
        "tools",
        "unset",
        "xcode",
        "--member",
        "alpha",
        "--version",
        "26.4",
    ]);
    let left: Vec<String> = alpha.ok(&["tools", "exceptions"])["members"][0]["exceptions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|exception| exception["tool"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(left, ["bun", "tuist", "iOS"]);

    let before = std::fs::read(&path).unwrap();
    let failed = alpha.run(&["tools", "set", "mise:node", "latest", "--member", "alpha"]);
    assert_eq!(failed.status.code(), Some(1), "{failed:?}");
    let error: Value = members::json(&failed);
    assert_eq!(error["error"]["code"], "invalid_fleet_tools");
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("versions must be exact, e.g. 22.19.0"),
        "{error}"
    );
    assert!(!error.to_string().contains("latest"));
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[test]
fn the_desired_state_shape_is_refused_by_reads_and_writes() {
    let alpha = TestMember::start("alpha");
    let path = file(&alpha);
    std::fs::write(
        &path,
        "[defaults.mise]\nversion = \"2026.11.6\"\n\n[defaults.mise.tools.node]\ndefault = \"22.19.0\"\n",
    )
    .unwrap();
    let before = std::fs::read(&path).unwrap();
    for args in [
        &["tools", "exceptions"][..],
        &["tools", "leave-out", "mise:bun"],
    ] {
        let failed = alpha.run(args);
        assert_eq!(failed.status.code(), Some(1), "{args:?}: {failed:?}");
        let error = members::json(&failed);
        assert_eq!(error["error"]["code"], "invalid_fleet_tools");
        let message = error["error"]["message"].as_str().unwrap();
        assert!(message.starts_with("defaults.mise."), "{message}");
        let hint = error["error"]["hint"].as_str().unwrap();
        assert!(hint.contains("only exceptions to the pins"), "{hint}");
        assert!(!error.to_string().contains("2026.11.6"), "{error}");
    }
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[test]
fn versions_need_a_member_and_executables_are_not_exceptions() {
    let alpha = TestMember::start("alpha");
    let without_member = alpha.run(&["tools", "set", "mise:node", "22.19.0"]);
    assert_eq!(without_member.status.code(), Some(2), "{without_member:?}");
    for args in [
        &["tools", "set", "mise", "2026.11.6", "--member", "alpha"][..],
        &["tools", "unset", "xcodes"],
        &["tools", "leave-out", "mise:foo bar"],
        &["tools", "leave-out", "node"],
    ] {
        let failed = alpha.run(args);
        assert_eq!(failed.status.code(), Some(2), "{args:?}: {failed:?}");
        assert_eq!(members::json(&failed)["error"]["code"], "invalid_request");
    }
    let failed = alpha.run(&["tools", "leave-out", "xcodes:node"]);
    assert_eq!(failed.status.code(), Some(1), "{failed:?}");
    let error = members::json(&failed);
    assert_eq!(error["error"]["code"], "invalid_fleet_tools");
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("xcodes manages only the tool xcode"),
        "{error}"
    );
    assert!(!file(&alpha).exists());
}

#[test]
fn only_the_master_edits_and_members_must_be_enrolled() {
    let alpha = TestMember::start("alpha");
    let beta = TestMember::start("beta");
    enrol(&alpha, &beta);

    let denied = beta.run(&["tools", "leave-out", "mise:sentry-cli"]);
    assert_eq!(denied.status.code(), Some(1), "{denied:?}");
    let error = members::json(&denied);
    assert_eq!(error["error"]["code"], "not_master");
    assert!(
        error["error"]["hint"].as_str().unwrap().contains("alpha"),
        "{error}"
    );
    assert!(!file(&beta).exists());

    assert_eq!(
        alpha.fails(&["tools", "set", "xcode", "26.2", "--member", "nobody"]),
        "unknown_member"
    );
    alpha.ok(&["tools", "set", "xcode", "26.3", "--member", "beta"]);
    let view = alpha.ok(&["tools", "exceptions"]);
    assert_eq!(view["members"][0]["exceptions"], json!([]));
    assert_eq!(view["members"][1]["member"], "beta");
    assert_eq!(view["members"][1]["exceptions"][0]["default"], "26.3");
}

#[test]
fn a_noop_edit_does_not_rewrite_the_file() {
    let alpha = TestMember::start("alpha");
    alpha.ok(&["tools", "leave-out", "mise:bun"]);
    let path = file(&alpha);
    let before = std::fs::read(&path).unwrap();
    let inode = std::fs::metadata(&path).unwrap().ino();
    alpha.ok(&["tools", "leave-out", "mise:bun"]);
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert_eq!(std::fs::metadata(&path).unwrap().ino(), inode);
    alpha.ok(&["tools", "set", "mise:bun", "1.3.14", "--member", "alpha"]);
    assert_ne!(std::fs::read(&path).unwrap(), before);
}
