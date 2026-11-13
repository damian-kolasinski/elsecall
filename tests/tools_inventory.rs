#![cfg(target_os = "macos")]
mod processes;

use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Output, Stdio},
    thread::sleep,
    time::{Duration, Instant},
};

fn home() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("ec-")
        .tempdir_in("/tmp")
        .unwrap()
}

fn cli(home: &Path, args: &[&str]) -> Output {
    crate::processes::command(env!("CARGO_BIN_EXE_elsecall"))
        .env("HOME", home)
        .current_dir(home)
        .args(args)
        .output()
        .unwrap()
}

fn json_output(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|_| panic!("not JSON: {output:?}"))
}

struct Service(Child);
impl Drop for Service {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn executable(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn xml_plist(pairs: &[(&str, &str)]) -> String {
    let mut body = String::from(concat!(
        r#"<?xml version="1.0" encoding="UTF-8"?>"#,
        "\n",
        r#"<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">"#,
        "\n",
        r#"<plist version="1.0"><dict>"#,
    ));
    for (key, value) in pairs {
        body.push_str("<key>");
        body.push_str(key);
        body.push_str("</key><string>");
        body.push_str(value);
        body.push_str("</string>");
    }
    body.push_str("</dict></plist>\n");
    body
}

fn write_bundle(apps: &Path, name: &str, info: &[(&str, &str)], version: Option<&[(&str, &str)]>) {
    let contents = apps.join(name).join("Contents");
    fs::create_dir_all(&contents).unwrap();
    fs::write(contents.join("Info.plist"), xml_plist(info)).unwrap();
    if let Some(version) = version {
        fs::write(contents.join("version.plist"), xml_plist(version)).unwrap();
    }
}

struct Layout {
    home: tempfile::TempDir,
    xcode_16: PathBuf,
    xcode_26: PathBuf,
    environment: Vec<(String, String)>,
}

fn utf8(path: &Path) -> String {
    path.to_str().expect("utf-8 path").to_owned()
}

fn layout(mise_body: &str) -> Layout {
    let home = home();
    let root = home.path();
    let apps = root.join("Applications");
    fs::create_dir_all(&apps).unwrap();
    write_bundle(
        &apps,
        "Xcode-16.4.0.app",
        &[
            ("CFBundleIdentifier", "com.apple.dt.Xcode"),
            ("CFBundleShortVersionString", "16.4"),
        ],
        Some(&[("ProductBuildVersion", "16F6")]),
    );
    write_bundle(
        &apps,
        "Xcode-26.2.0.app",
        &[
            ("CFBundleIdentifier", "com.apple.dt.Xcode"),
            ("CFBundleShortVersionString", "26.2"),
        ],
        Some(&[("ProductBuildVersion", "17C52")]),
    );
    write_bundle(
        &apps,
        "TextEdit.app",
        &[
            ("CFBundleIdentifier", "com.apple.TextEdit"),
            ("CFBundleShortVersionString", "1.0"),
        ],
        Some(&[("ProductBuildVersion", "1")]),
    );
    write_bundle(
        &apps,
        "BadVersion.app",
        &[
            ("CFBundleIdentifier", "com.apple.dt.Xcode"),
            ("CFBundleShortVersionString", "26 .2"),
        ],
        Some(&[("ProductBuildVersion", "17C52")]),
    );
    let broken = apps.join("Broken.app/Contents");
    fs::create_dir_all(&broken).unwrap();
    fs::write(broken.join("Info.plist"), b"not a plist").unwrap();
    fs::create_dir_all(apps.join("NotAnApp")).unwrap();
    fs::write(apps.join("notes.txt"), b"leave me").unwrap();

    let listing = root.join("ls.json");
    let global = root.join("ls-global.json");
    fs::write(
        &listing,
        r#"{"bun":[
            {"version":"1.3.5","install_path":"/Users/me/.local/share/mise/installs/bun/1.3.5","installed":true,"active":false},
            {"version":"1.2.13","install_path":"/Users/me/.local/share/mise/installs/bun/1.2.13","installed":true,"active":false}
        ],"node":[
            {"version":"22.19.0","requested_version":"22","install_path":"/Users/me/.local/share/mise/installs/node/22.19.0","source":{"type":"mise.toml","path":"/Users/me/.config/mise/config.toml"},"installed":true,"active":true},
            {"version":"99.0.0","installed":false,"active":false},
            {"version":"22.16.0","install_path":"/Users/me/.local/share/mise/installs/node/22.16.0","installed":true,"active":false}
        ],"rust":[
            {"version":"1.99.0","install_path":"/Users/me/.local/share/mise/installs/rust/1.99.0","symlinked_to":"/Users/me/.cargo/bin","installed":true,"active":false}
        ],"tuist":[
            {"version":"4.206.0","installed":false,"active":true,"requested_version":"4.206.0"}
        ]}"#,
    )
    .unwrap();
    fs::write(
        &global,
        r#"{"node":[
            {"version":"22.19.0","requested_version":"22","install_path":"/Users/me/.local/share/mise/installs/node/22.19.0","source":{"type":"mise.toml","path":"/Users/me/.config/mise/config.toml"},"installed":true,"active":true}
        ],"tuist":[
            {"version":"4.206.0","requested_version":"4.206.0","installed":false,"active":true}
        ]}"#,
    )
    .unwrap();
    let mise = root.join("mise");
    let env_file = root.join("mise.env");
    executable(
        &mise,
        &format!(
            "#!/bin/sh\nset -eu\n/usr/bin/env > '{env}'\nif [ \"${{1:-}}\" = \"--version\" ] && [ \"$#\" -eq 1 ]; then\n  printf '%s\\n' '2026.10.11 macos-arm64 (2026-10-15)'\n  exit 0\nfi\nif [ \"${{1:-}}\" = \"ls\" ] && [ \"${{2:-}}\" = \"--json\" ] && [ \"$#\" -eq 2 ]; then\n  cat '{listing}'\n  exit 0\nfi\nif [ \"${{1:-}}\" = \"ls\" ] && [ \"${{2:-}}\" = \"--global\" ] && [ \"${{3:-}}\" = \"--json\" ] && [ \"$#\" -eq 3 ]; then\n  cat '{global}'\n  exit 0\nfi\nprintf '%s\\n' \"unexpected arguments: $*\" >&2\nexit 2\n",
            env = utf8(&env_file),
            listing = utf8(&listing),
            global = utf8(&global),
        ),
    );
    if !mise_body.is_empty() {
        executable(&mise, mise_body);
    }
    let xcode_16 = apps.join("Xcode-16.4.0.app");
    let xcode_26 = apps.join("Xcode-26.2.0.app");
    let select = root.join("xcode-select");
    executable(
        &select,
        &format!(
            "#!/bin/sh\nprintf '%s\\n' '{}/Contents/Developer'\n",
            utf8(&xcode_26)
        ),
    );
    let missing = root.join("no-xcodes");
    Layout {
        environment: vec![
            ("ELSECALL_MISE".into(), utf8(&mise)),
            ("ELSECALL_XCODE_APPLICATIONS".into(), utf8(&apps)),
            ("ELSECALL_XCODE_SELECT".into(), utf8(&select)),
            ("ELSECALL_XCODES".into(), utf8(&missing)),
        ],
        home,
        xcode_16,
        xcode_26,
    }
}

fn start(layout: &Layout) -> Service {
    let log_path = layout.home.path().join("serve.log");
    let log = fs::File::create(&log_path).unwrap();
    let mut command = crate::processes::command(env!("CARGO_BIN_EXE_elsecall"));
    command
        .env("HOME", layout.home.path())
        .current_dir(layout.home.path())
        .env("ELSECALL_TAILNET_NAME", "tools-inventory")
        .env("ELSECALL_TAILNET_ADDRESSES", "127.0.0.1")
        .env("ELSECALL_FLEET_PORT", free_port().to_string())
        .env("ELSECALL_MACHINE_SPEED", "1");
    for (key, value) in &layout.environment {
        command.env(key, value);
    }
    let service = Service(
        command
            .arg("serve")
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log))
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + crate::processes::WAIT;
    while !cli(layout.home.path(), &["status"]).status.success() {
        assert!(
            Instant::now() < deadline,
            "service did not start: {}",
            fs::read_to_string(&log_path).unwrap_or_default()
        );
        sleep(Duration::from_millis(20));
    }
    service
}

fn mise_path(layout: &Layout) -> String {
    layout
        .environment
        .iter()
        .find(|(name, _)| name == "ELSECALL_MISE")
        .unwrap()
        .1
        .clone()
}

fn mise_read(path: &str) -> Value {
    json!({
        "manager": "mise",
        "state": "read",
        "executable": {"state": "found", "path": path, "version": "2026.10.11"},
        "tools": [
            {"name": "bun", "version": "1.2.13", "default": false},
            {"name": "bun", "version": "1.3.5", "default": false},
            {"name": "node", "version": "22.16.0", "default": false},
            {"name": "node", "version": "22.19.0", "default": true},
            {"name": "rust", "version": "1.99.0", "default": false}
        ]
    })
}

fn xcode_tools(layout: &Layout) -> Value {
    json!([
        {
            "name": "xcode",
            "version": "16.4",
            "default": false,
            "build": "16F6",
            "path": utf8(&layout.xcode_16)
        },
        {
            "name": "xcode",
            "version": "26.2",
            "default": true,
            "build": "17C52",
            "path": utf8(&layout.xcode_26)
        }
    ])
}

#[test]
fn inventory_requires_the_service() {
    let home = home();
    let result = cli(home.path(), &["tools", "inventory"]);
    assert_eq!(result.status.code(), Some(1));
    assert_eq!(json_output(&result)["error"]["code"], "service_unavailable");
}

#[test]
fn reports_the_fake_managers_exactly() {
    let layout = layout("");
    let _service = start(&layout);
    let result = cli(layout.home.path(), &["tools", "inventory"]);
    assert!(result.status.success(), "{result:?}");
    assert!(result.stderr.is_empty());
    assert_eq!(
        json_output(&result),
        json!({
            "schema_version": 1,
            "data": {
                "managers": [
                    mise_read(&mise_path(&layout)),
                    {
                        "manager": "xcodes",
                        "state": "read",
                        "executable": {"state": "missing"},
                        "tools": xcode_tools(&layout)
                    }
                ]
            }
        })
    );
}

#[test]
fn a_failing_xcodes_version_keeps_the_mise_inventory() {
    let mut layout = layout("");
    let xcodes = layout.home.path().join("xcodes");
    executable(&xcodes, "#!/bin/sh\nexit 1\n");
    for (name, value) in &mut layout.environment {
        if name == "ELSECALL_XCODES" {
            *value = utf8(&xcodes);
        }
    }
    let _service = start(&layout);
    let result = cli(layout.home.path(), &["tools", "inventory"]);
    assert!(result.status.success(), "{result:?}");
    assert_eq!(
        json_output(&result),
        json!({
            "schema_version": 1,
            "data": {
                "managers": [
                    mise_read(&mise_path(&layout)),
                    {
                        "manager": "xcodes",
                        "state": "failed",
                        "error": {
                            "code": "tool_manager_failed",
                            "message": "xcodes version exited with status 1.",
                            "hint": "Run the tool manager in a terminal to see why, then retry elsecall tools inventory."
                        }
                    }
                ]
            }
        })
    );
}

#[test]
fn mise_variables_are_removed_from_the_child() {
    let mut layout = layout("");
    layout.environment.push(("MISE".into(), "signing".into()));
    layout
        .environment
        .push(("MISE_ENV".into(), "signing".into()));
    let _service = start(&layout);
    let result = cli(layout.home.path(), &["tools", "inventory"]);
    assert!(result.status.success(), "{result:?}");
    let dumped = fs::read_to_string(layout.home.path().join("mise.env")).unwrap();
    let kept: Vec<_> = dumped
        .lines()
        .filter(|line| line.starts_with("MISE=") || line.starts_with("MISE_"))
        .map(|line| line.split_once('=').map(|(name, _)| name).unwrap_or(line))
        .collect();
    assert!(kept.is_empty(), "mise child kept {kept:?}");
    assert!(dumped.lines().any(|line| line.starts_with("HOME=")));
}

#[test]
fn a_failing_mise_keeps_the_xcode_inventory() {
    let layout = layout("#!/bin/sh\nexit 1\n");
    let _service = start(&layout);
    let result = cli(layout.home.path(), &["tools", "inventory"]);
    assert!(result.status.success(), "{result:?}");
    assert_eq!(
        json_output(&result),
        json!({
            "schema_version": 1,
            "data": {
                "managers": [
                    {
                        "manager": "mise",
                        "state": "failed",
                        "error": {
                            "code": "tool_manager_failed",
                            "message": "mise --version exited with status 1.",
                            "hint": "Run the tool manager in a terminal to see why, then retry elsecall tools inventory."
                        }
                    },
                    {
                        "manager": "xcodes",
                        "state": "read",
                        "executable": {"state": "missing"},
                        "tools": xcode_tools(&layout)
                    }
                ]
            }
        })
    );
}

/// The real managers on this machine. Not part of `mise run check`.
#[test]
#[ignore]
fn live_machine_tools_inventory() {
    let mise = elsecall::mise_cli::MiseCli;
    let xcodes = elsecall::macos::xcodes::XcodesCli;
    let managers: [&dyn elsecall::tool_inventory::ToolManager; 2] = [&mise, &xcodes];
    let inventory = elsecall::tool_inventory::gather(&managers);
    println!("{}", serde_json::to_string_pretty(&inventory).unwrap());
    assert_eq!(inventory.managers.len(), 2);
}
