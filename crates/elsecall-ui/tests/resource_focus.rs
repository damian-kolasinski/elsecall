//! The same contributor details are reachable with a pointer or the keyboard.
use serde_json::{Value, json};
use std::{fs, process::Command};

#[test]
fn tab_switches_contributor_and_escape_dismisses_while_focused() {
    if std::env::var_os("ELSECALL_UI_SKIP_GOLDENS").is_some() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let input = temp.path().join("input.json");
    let tree = temp.path().join("tree.json");
    fs::write(
        &input,
        json!([
            {"t":0.1,"key":"usage-mac-mini-CPU-Managed"},
            {"t":0.35,"type":"move","x":1090,"y":540},
            {"t":0.7,"type":"key","name":"Tab"},
            {"t":1.4,"type":"key","name":"Escape"}
        ])
        .to_string(),
    )
    .unwrap();
    for (at, open) in [("1.1", true), ("2.0", false)] {
        let result = Command::new(env!("CARGO_BIN_EXE_elsecall-ui"))
            .args(["--scene", "resource-load", "--size", "1120x560", "--script"])
            .arg(&input)
            .args(["--at", at, "--tree"])
            .arg(&tree)
            .env("HOME", temp.path())
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let document: Value = serde_json::from_slice(&fs::read(&tree).unwrap()).unwrap();
        let visible = |text: &str| {
            document["elements"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["text"] == text && e["exiting"] != true)
        };
        assert_eq!(visible("Outside CPU · mac-mini"), open);
        assert_eq!(visible("Spotlight indexing"), open);
        assert!(!visible("Build acme-ios"));
    }
}
