//! Drive the native UI through menu and input events against an isolated personal file.

use serde_json::{Value, json};
use std::{fs, path::Path, process::Command};

#[test]
fn inspect_rules_check_a_label_and_read_the_source_without_the_service() {
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
            {"t": 0.1, "menu": "workload-configuration"},
            {"t": 0.7, "key": "segment1"},
            {"t": 0.9, "key": "configuration-label"},
            {"t": 1.1, "type": "type", "text": "elsecall build", "every": 0.015},
            {"t": 1.7, "key": "segment2"},
            {"t": 2.0, "key": "configuration-back"}
        ])
        .to_string(),
    )
    .unwrap();

    let rules = snapshot(home.path(), &script, "0.6");
    assert!(visible(&rules, "elsecall check"));
    assert!(visible(&rules, "Run unmanaged"));

    let check = snapshot(home.path(), &script, "1.6");
    for text in ["Matched elsecall build", "4.5", "15m", "Pinned"] {
        assert!(visible(&check, text), "expected {text:?} in label preview");
    }

    let toml = snapshot(home.path(), &script, "1.95");
    assert!(visible(&toml, source));
    let fleet = snapshot(home.path(), &script, "2.25");
    assert!(visible(&fleet, "Fleet"));
    assert!(!visible(&fleet, "workloads.toml"));
    assert_eq!(
        fs::read_to_string(data.join("workloads.toml")).unwrap(),
        source
    );
}

fn snapshot(home: &Path, script: &Path, at: &str) -> Value {
    let tree = home.join("tree.json");
    let output = Command::new(env!("CARGO_BIN_EXE_elsecall-ui"))
        .args([
            "--scene",
            "loading",
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
