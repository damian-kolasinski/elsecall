//! The row action opens a modal with both sources and restores keyboard focus on dismissal.
use serde_json::{Value, json};
use std::{fs, process::Command};

#[test]
fn load_details_shows_both_sources_and_escape_returns_to_the_trigger() {
    if std::env::var_os("ELSECALL_UI_SKIP_GOLDENS").is_some() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let input = temp.path().join("input.json");
    let tree = temp.path().join("tree.json");
    fs::write(
        &input,
        json!([
            {"t":0.1,"key":"load-details:mac-mini"},
            {"t":1.4,"type":"key","name":"Escape"},
            {"t":2.3,"type":"key","name":" "}
        ])
        .to_string(),
    )
    .unwrap();
    for (at, open) in [("1.1", true), ("2.0", false), ("3.0", true)] {
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
        assert_eq!(visible("Load details · mac-mini"), open, "at {at}");
        assert_eq!(visible("Spotlight indexing"), open);
        assert_eq!(visible("Build acme-ios"), open);
        assert_eq!(visible("Device simulator"), open);
    }
}
