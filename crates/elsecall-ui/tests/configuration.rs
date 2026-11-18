//! Drive the native UI through toolbar and input events against an isolated personal file.

use serde_json::{Value, json};
use std::{fs, path::Path, process::Command};

#[test]
fn navigate_search_rules_and_read_the_source_without_the_service() {
    if std::env::var_os("ELSECALL_UI_SKIP_GOLDENS").is_some() {
        return;
    }
    let home = tempfile::Builder::new()
        .prefix("ec-ui-")
        .tempdir_in("/tmp")
        .unwrap();
    let data = home.path().join("Library/Application Support/Elsecall");
    let source = include_str!("../../../tests/fixtures/development-workloads.toml");
    fs::create_dir_all(&data).unwrap();
    fs::write(data.join("workloads.toml"), source).unwrap();
    let script = home.path().join("input.json");
    fs::write(
        &script,
        json!([
            {"t": 0.1, "key": "main-navigation/segment1"},
            {"t": 0.8, "key": "configuration-search"},
            {"t": 1.0, "type": "type", "text": "BUILD", "every": 0.015},
            {"t": 1.75, "key": "configuration-tabs/segment1"},
            {"t": 2.1, "key": "main-navigation/segment0"}
        ])
        .to_string(),
    )
    .unwrap();

    let rules = snapshot(home.path(), &script, "loading", "0.6");
    assert!(visible(&rules, "elsecall check"));
    assert!(visible(&rules, "Run unmanaged"));

    let filtered = snapshot(home.path(), &script, "loading", "1.6");
    assert!(visible(&filtered, "elsecall build"));
    assert!(!visible(&filtered, "elsecall check"));
    assert!(visible(&filtered, "4.5"));

    let toml = snapshot(home.path(), &script, "loading", "1.95");
    assert!(visible(&toml, source));
    let fleet = snapshot(home.path(), &script, "loading", "2.25");
    assert!(visible(&fleet, "Fleet"));
    assert!(!visible(&fleet, "workloads.toml"));
    assert_eq!(
        fs::read_to_string(data.join("workloads.toml")).unwrap(),
        source
    );
}

#[test]
fn add_mac_keeps_configuration_closed_until_the_sheet_is_dismissed() {
    if std::env::var_os("ELSECALL_UI_SKIP_GOLDENS").is_some() {
        return;
    }
    let home = tempfile::Builder::new()
        .prefix("ec-ui-")
        .tempdir_in("/tmp")
        .unwrap();
    let script = home.path().join("input.json");
    fs::write(
        &script,
        json!([
            {"t": 0.2, "key": "main-navigation/segment1"},
            {"t": 0.4, "menu": "workload-configuration"},
            {"t": 0.8, "key": "add-mac-cancel"},
            {"t": 1.25, "key": "main-navigation/segment1"}
        ])
        .to_string(),
    )
    .unwrap();
    let active = snapshot(home.path(), &script, "add-mac-prepare", "0.65");
    assert!(visible(&active, "Remote Login is on"));
    assert!(!visible(&active, "workloads.toml"));
    let dismissed = snapshot(home.path(), &script, "add-mac-prepare", "1.7");
    assert!(visible(&dismissed, "workloads.toml"));
    assert!(!visible(&dismissed, "Remote Login is on"));
}

fn snapshot(home: &Path, script: &Path, scene: &str, at: &str) -> Value {
    let tree = home.join("tree.json");
    let output = Command::new(env!("CARGO_BIN_EXE_elsecall-ui"))
        .args([
            "--scene",
            scene,
            "--realtime",
            "--size",
            "1040x480",
            "--scale",
            "1",
            "--script",
        ])
        .arg(script)
        .args(["--at", at, "--tree"])
        .arg(&tree)
        .env("HOME", home)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&fs::read(tree).unwrap()).unwrap()
}

fn visible(tree: &Value, text: &str) -> bool {
    tree["elements"]
        .as_array()
        .unwrap()
        .iter()
        .any(|node| node["text"] == text && node["exiting"] != true)
}
