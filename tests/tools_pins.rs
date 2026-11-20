//! `elsecall tools pins` on a workspace: every source, inexact pins resolved
//! against this machine, and nothing the workspace controls ever run.
#![cfg(target_os = "macos")]

use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Output,
};

struct Layout {
    home: tempfile::TempDir,
    workspace: PathBuf,
    mise_log: PathBuf,
    marker: PathBuf,
    environment: Vec<(&'static str, String)>,
}

fn utf8(path: &Path) -> String {
    path.to_str().expect("utf-8 path").to_owned()
}

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn executable(path: &Path, body: &str) {
    write(path, body);
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn plist(pairs: &[(&str, &str)]) -> String {
    let mut body =
        String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\"><dict>");
    for (key, value) in pairs {
        body.push_str(&format!("<key>{key}</key><string>{value}</string>"));
    }
    body.push_str("</dict></plist>\n");
    body
}

/// A home with the master's global config, a workspace using every source, a
/// fake mise that logs where it runs, and one installed Xcode.
fn layout() -> Layout {
    let home = tempfile::Builder::new()
        .prefix("ec-")
        .tempdir_in("/tmp")
        .unwrap();
    let root = fs::canonicalize(home.path()).unwrap();
    let marker = root.join("ran");
    let workspace = root.join("acme-ios");
    write(
        &root.join(".config/mise/config.toml"),
        "[tools]\nnode = \"22\"\nruby = \"3.2.0\"\n",
    );
    write(
        &workspace.join("mise.toml"),
        &format!(
            "[tools]\npython = \"3.10.4\"\nbun = \"latest\"\nsentry-cli = \"latest\"\n\
             [env]\nAPI_TOKEN = \"{{{{ exec(command='touch {marker}') }}}}\"\n\
             [hooks]\nenter = \"touch {marker}\"\n\
             [tasks.build]\nrun = \"touch {marker}\"\n",
            marker = utf8(&marker),
        ),
    );
    write(
        &workspace.join("mise.signing.toml"),
        "[tools]\n\"packslip:example/crem\" = \"0.2.0\"\n",
    );
    write(
        &workspace.join("mise.local.toml"),
        "[tools]\nlocal-only = \"1.0.0\"\n",
    );
    write(&workspace.join(".xcode-version"), "26\n");
    write(
        &workspace.join(".elsecall-tools.toml"),
        "[simulators]\nruntimes = [\"iOS 26.2\"]\n",
    );
    write(
        &workspace.join(".elsecall.toml"),
        "legacy workload file, not TOML = =\n",
    );

    let listing = root.join("ls.json");
    write(
        &listing,
        r#"{"bun":[
            {"version":"1.3.5","installed":true,"active":false},
            {"version":"1.3.14","installed":true,"active":false}
        ],"node":[
            {"version":"22.19.0","installed":true,"active":true},
            {"version":"24.0.1","installed":true,"active":false}
        ],"sentry-cli":[
            {"version":"3.8.0","installed":true,"active":false}
        ]}"#,
    );
    let mise_log = root.join("mise.log");
    let mise = root.join("bin/mise");
    executable(
        &mise,
        &format!(
            "#!/bin/sh\nset -eu\npwd >> '{log}'\n\
             if [ \"$*\" = \"--version\" ]; then echo '2026.10.11 macos-arm64'; exit 0; fi\n\
             if [ \"$*\" = \"ls --json\" ]; then cat '{listing}'; exit 0; fi\n\
             if [ \"$*\" = \"ls --global --json\" ]; then echo '{{}}'; exit 0; fi\n\
             echo \"unexpected arguments: $*\" >&2\nexit 2\n",
            log = utf8(&mise_log),
            listing = utf8(&listing),
        ),
    );
    let apps = root.join("Applications");
    let contents = apps.join("Xcode-26.2.0.app/Contents");
    write(
        &contents.join("Info.plist"),
        &plist(&[
            ("CFBundleIdentifier", "com.apple.dt.Xcode"),
            ("CFBundleShortVersionString", "26.2"),
        ]),
    );
    write(
        &contents.join("version.plist"),
        &plist(&[("ProductBuildVersion", "17C52")]),
    );
    let select = root.join("bin/xcode-select");
    executable(&select, "#!/bin/sh\nexit 1\n");
    Layout {
        environment: vec![
            ("ELSECALL_MISE", utf8(&mise)),
            ("ELSECALL_XCODE_APPLICATIONS", utf8(&apps)),
            ("ELSECALL_XCODE_SELECT", utf8(&select)),
            ("ELSECALL_XCODES", utf8(&root.join("no-xcodes"))),
        ],
        home,
        workspace,
        mise_log,
        marker,
    }
}

fn pins(layout: &Layout, directory: &Path, args: &[&str]) -> Output {
    crate::processes::command(env!("CARGO_BIN_EXE_elsecall"))
        .env("HOME", layout.home.path())
        .envs(layout.environment.iter().map(|(name, value)| (name, value)))
        .current_dir(directory)
        .args(["tools", "pins"])
        .args(args)
        .output()
        .unwrap()
}

fn document(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|_| panic!("not JSON: {output:?}"))
}

#[test]
fn a_workspace_reports_every_pin_and_runs_none_of_its_code() {
    let layout = layout();
    let output = pins(&layout, &layout.workspace, &[]);
    assert!(output.status.success(), "{output:?}");
    let source = |kind: &str, file: &str| json!({"kind": kind, "file": file});
    let project = source("mise_project", "mise.toml");
    let global = source("mise_global", "~/.config/mise/config.toml");
    let resolved_hint =
        "Pin the resolved version, or another exact one, so every member installs the same build.";
    assert_eq!(
        document(&output),
        json!({
            "schema_version": 1,
            "data": {
                "workspace": utf8(&layout.workspace),
                "sources": [
                    project,
                    source("mise_project", "mise.signing.toml"),
                    global,
                    source("xcode_version", ".xcode-version"),
                    source("elsecall_tools", ".elsecall-tools.toml"),
                ],
                "pins": [
                    {"manager": "mise", "tool": "bun", "version": "latest", "exact": false,
                     "resolved": "1.3.14", "hint": resolved_hint,
                     "environment": null, "source": project},
                    {"manager": "mise", "tool": "node", "version": "22", "exact": false,
                     "resolved": "22.19.0", "hint": resolved_hint,
                     "environment": null, "source": global},
                    {"manager": "mise", "tool": "packslip:example/crem", "version": "0.2.0",
                     "exact": true, "environment": "signing",
                     "source": source("mise_project", "mise.signing.toml")},
                    {"manager": "mise", "tool": "python", "version": "3.10.4", "exact": true,
                     "environment": null, "source": project},
                    {"manager": "mise", "tool": "ruby", "version": "3.2.0", "exact": true,
                     "environment": null, "source": global},
                    {"manager": "mise", "tool": "sentry-cli", "version": "latest", "exact": false,
                     "resolved": "3.8.0", "hint": resolved_hint,
                     "environment": null, "source": project},
                    {"manager": "xcodes", "tool": "xcode", "version": "26", "exact": false,
                     "resolved": "26.2", "hint": resolved_hint,
                     "environment": null, "source": source("xcode_version", ".xcode-version")},
                    {"manager": "simulators", "tool": "iOS", "version": "26.2", "exact": true,
                     "environment": null, "source": source("elsecall_tools", ".elsecall-tools.toml")},
                ],
                "warnings": [],
            }
        })
    );
    assert!(
        !layout.marker.exists(),
        "the workspace's hooks, env or tasks ran"
    );
    let ran_in = fs::read_to_string(&layout.mise_log).unwrap();
    let home = utf8(&fs::canonicalize(layout.home.path()).unwrap());
    assert!(
        ran_in.lines().all(|line| line == home),
        "mise ran outside the home folder: {ran_in}"
    );

    let explicit = pins(&layout, layout.home.path(), &[&utf8(&layout.workspace)]);
    assert_eq!(document(&explicit), document(&output));
}

#[test]
fn broken_and_conflicting_workspaces_fail_without_echoing_values() {
    let layout = layout();
    let mise = layout.workspace.join("mise.toml");

    fs::write(&mise, "[tools]\nnode = \"{{ env.SECRET_TOKEN }}\"\n").unwrap();
    let output = pins(&layout, &layout.workspace, &[]);
    assert_eq!(output.status.code(), Some(1));
    let error = &document(&output)["error"];
    assert_eq!(error["code"], "pin_source_invalid");
    assert_eq!(
        error["message"],
        "mise.toml: tools.node: versions must be literal; Elsecall does not render templates."
    );

    fs::write(&mise, "[tools]\nnode = \"22.19.0\"\n").unwrap();
    write(&layout.workspace.join(".tool-versions"), "node 20.19.5\n");
    let output = pins(&layout, &layout.workspace, &[]);
    assert_eq!(output.status.code(), Some(1));
    let error = &document(&output)["error"];
    assert_eq!(error["code"], "pins_conflict");
    assert_eq!(
        error["message"],
        "mise node is pinned in both .tool-versions and mise.toml."
    );

    fs::remove_file(layout.workspace.join(".tool-versions")).unwrap();
    let secret = layout.home.path().join("secret.txt");
    write(&secret, "SECRET_TOKEN=abc\n");
    fs::remove_file(&mise).unwrap();
    std::os::unix::fs::symlink(&secret, &mise).unwrap();
    let output = pins(&layout, &layout.workspace, &[]);
    assert_eq!(output.status.code(), Some(1));
    let error = &document(&output)["error"];
    assert_eq!(error["code"], "pin_source_unreadable");
    assert_eq!(
        error["message"],
        "mise.toml: it resolves outside the workspace."
    );

    for output in [
        pins(&layout, &layout.workspace, &["missing"]),
        pins(&layout, &layout.workspace, &[".xcode-version"]),
    ] {
        assert_eq!(output.status.code(), Some(2), "{output:?}");
        assert_eq!(document(&output)["error"]["code"], "invalid_request");
    }

    let printed = String::from_utf8_lossy(&output_of_all(&layout)).into_owned();
    assert!(!printed.contains("SECRET_TOKEN"), "{printed}");
}

/// Every failure above again, concatenated, to check none prints a value.
fn output_of_all(layout: &Layout) -> Vec<u8> {
    let mise = layout.workspace.join("mise.toml");
    let mut printed = Vec::new();
    for text in [
        "[tools]\nnode = \"{{ env.SECRET_TOKEN }}\"\n",
        "SECRET_TOKEN = = 1\n",
        "[tools]\nnode = { version = 1, SECRET_TOKEN = 2 }\n",
    ] {
        let _ = fs::remove_file(&mise);
        fs::write(&mise, text).unwrap();
        let output = pins(layout, &layout.workspace, &[]);
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        printed.extend(output.stdout);
        printed.extend(output.stderr);
    }
    printed
}
