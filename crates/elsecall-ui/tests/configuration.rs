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

#[test]
fn file_sync_approval_sends_the_plan_shown_and_reports_pending_delivery() {
    if std::env::var_os("ELSECALL_UI_SKIP_GOLDENS").is_some() {
        return;
    }
    use std::io::{BufRead, BufReader, Read, Write};
    use std::os::unix::net::UnixListener;
    use std::time::Duration;

    let home = tempfile::Builder::new()
        .prefix("ec-ui-")
        .tempdir_in("/tmp")
        .unwrap();
    let socket = home
        .path()
        .join("Library/Application Support/Elsecall/service.sock");
    fs::create_dir_all(socket.parent().unwrap()).unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(socket.parent().unwrap(), fs::Permissions::from_mode(0o700)).unwrap();
    let listener = UnixListener::bind(socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let requests = std::thread::spawn(move || {
        let fixture: Value = serde_json::from_str(include_str!("fixtures/file-sync.json")).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        for (route, expected, data) in [
            (
                "/v1/sync/enable",
                json!({"path": "~/.config/acme-ios", "confirm": "0123456789abcdef"}),
                json!({"round": {"complete": false}}),
            ),
            ("/v1/sync", json!({}), fixture["status"].clone()),
        ] {
            let mut connection = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "UI did not send {route}"
                        );
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("socket accept failed: {error}"),
                }
            };
            connection
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut reader = BufReader::new(&mut connection);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            assert!(line.starts_with(&format!("POST {route} ")), "{line}");
            let mut length = 0;
            loop {
                line.clear();
                assert!(reader.read_line(&mut line).unwrap() > 0);
                if line == "\r\n" {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse().unwrap();
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            assert_eq!(serde_json::from_slice::<Value>(&body).unwrap(), expected);
            let response = json!({"schema_version": 1, "data": data}).to_string();
            write!(connection, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).unwrap();
        }
    });
    let script = home.path().join("input.json");
    fs::write(&script, r#"[{"t":0.35,"key":"file-sync-enable"}]"#).unwrap();
    let tree = snapshot(home.path(), &script, "configuration-sync-review", "1.4");
    requests.join().unwrap();
    assert!(visible(
        &tree,
        "Configuration sync is enabled. Delivery is pending on some members."
    ));
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
