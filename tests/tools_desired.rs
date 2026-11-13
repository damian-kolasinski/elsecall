//! Desired tool versions on a real local service: solo master, hand edits, and a fleet.
#![cfg(target_os = "macos")]
mod members;
mod processes;

use members::{TestMember, enrol};
use serde_json::{Value, json};
use std::os::unix::fs::{MetadataExt, PermissionsExt};

fn file(member: &TestMember) -> std::path::PathBuf {
    member.data().join("fleet-tools.toml")
}

#[test]
fn a_solo_master_sets_desired_versions_and_keeps_hand_edits() {
    let alpha = TestMember::start("alpha");
    let empty = alpha.ok(&["tools", "desired"]);
    assert_eq!(
        empty,
        json!({
            "members": [{
                "member": "alpha",
                "wants": {
                    "mise": {"version": null, "tools": {}},
                    "xcodes": {"version": null, "tools": {}}
                }
            }],
            "unknown_members": []
        })
    );
    assert!(!file(&alpha).exists());

    alpha.ok(&["tools", "set", "mise:node", "22.19.0"]);
    alpha.ok(&["tools", "set", "mise:tuist", "4.205.0", "--install-only"]);
    alpha.ok(&["tools", "set", "xcode", "26.2"]);
    let desired = alpha.ok(&["tools", "desired"]);
    assert_eq!(
        desired,
        json!({
            "members": [{
                "member": "alpha",
                "wants": {
                    "mise": {
                        "version": null,
                        "tools": {
                            "node": {"default": "22.19.0", "install": []},
                            "tuist": {"default": null, "install": ["4.205.0"]}
                        }
                    },
                    "xcodes": {
                        "version": null,
                        "tools": {
                            "xcode": {"default": "26.2", "install": []}
                        }
                    }
                }
            }],
            "unknown_members": []
        })
    );
    let path = file(&alpha);
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);

    let mut text = std::fs::read_to_string(&path).unwrap();
    text.push_str("\n# operator note\n[defaults.mise.tools.bun]\ndefault = \"1.2.13\"\n");
    text.push_str("[members.other-mac.mise.tools.node]\ndefault = \"20.0.0\"\n");
    std::fs::write(&path, &text).unwrap();
    alpha.ok(&["tools", "set", "mise:node", "22.16.0"]);
    let kept = std::fs::read_to_string(&path).unwrap();
    assert!(kept.contains("# operator note"), "{kept}");
    assert!(kept.contains("bun"), "{kept}");
    assert!(kept.contains("1.2.13"), "{kept}");
    assert!(kept.contains("22.16.0"), "{kept}");
    assert!(kept.contains("other-mac"), "{kept}");
    let after = alpha.ok(&["tools", "desired"]);
    assert_eq!(after["unknown_members"], json!(["other-mac"]));
    assert_eq!(
        after["members"][0]["wants"]["mise"]["tools"]["node"]["default"],
        "22.16.0"
    );
    assert_eq!(
        after["members"][0]["wants"]["mise"]["tools"]["bun"]["default"],
        "1.2.13"
    );

    let before = std::fs::read(&path).unwrap();
    let failed = alpha.run(&["tools", "set", "mise:node", "latest"]);
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
fn only_the_master_edits_and_members_must_be_enrolled() {
    let alpha = TestMember::start("alpha");
    let beta = TestMember::start("beta");
    enrol(&alpha, &beta);

    let denied = beta.run(&["tools", "set", "mise:node", "22.19.0"]);
    assert_eq!(denied.status.code(), Some(1), "{denied:?}");
    let error = members::json(&denied);
    assert_eq!(error["error"]["code"], "not_master");
    let text = error.to_string();
    assert!(text.contains("alpha"), "{error}");
    assert!(
        error["error"]["hint"].as_str().unwrap().contains("alpha"),
        "{error}"
    );
    assert!(!file(&beta).exists());

    assert_eq!(
        alpha.fails(&["tools", "set", "xcode", "26.2", "--member", "nobody"]),
        "unknown_member"
    );
    assert!(!file(&alpha).exists());
}

#[test]
fn a_noop_set_does_not_rewrite_the_file() {
    let alpha = TestMember::start("alpha");
    alpha.ok(&["tools", "set", "mise:node", "22.19.0"]);
    let path = file(&alpha);
    let before = std::fs::read(&path).unwrap();
    let inode = std::fs::metadata(&path).unwrap().ino();
    alpha.ok(&["tools", "set", "mise:node", "22.19.0"]);
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert_eq!(std::fs::metadata(&path).unwrap().ino(), inode);
    alpha.ok(&["tools", "set", "mise:node", "22.16.0"]);
    assert_ne!(std::fs::read(&path).unwrap(), before);
}

#[test]
fn a_malformed_tool_name_exits_2_and_a_non_xcode_name_is_a_document_error() {
    let alpha = TestMember::start("alpha");
    for args in [
        &["tools", "set", "mise:foo bar", "22.19.0"][..],
        &["tools", "unset", "mise:foo bar"],
    ] {
        let failed = alpha.run(args);
        assert_eq!(failed.status.code(), Some(2), "{args:?}: {failed:?}");
        assert_eq!(members::json(&failed)["error"]["code"], "invalid_request");
    }
    assert!(!file(&alpha).exists());

    for args in [
        &["tools", "set", "xcodes:node", "16.4"][..],
        &["tools", "unset", "xcodes:node"],
    ] {
        let failed = alpha.run(args);
        assert_eq!(failed.status.code(), Some(1), "{args:?}: {failed:?}");
        let error = members::json(&failed);
        assert_eq!(error["error"]["code"], "invalid_fleet_tools");
        assert!(
            error["error"]["message"]
                .as_str()
                .unwrap()
                .contains("xcodes manages only the tool xcode"),
            "{error}"
        );
    }
    assert!(!file(&alpha).exists());
}
