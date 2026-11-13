#![cfg(target_os = "macos")]
//! Each member's tool inventory, read through real member services.
mod members;
mod processes;

use members::{TestMember, enrol, json};
use serde_json::Value;
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::Path,
    time::{Duration, Instant},
};

struct Fakes {
    _root: tempfile::TempDir,
    env: Vec<(String, String)>,
}

fn fakes(mise_version: &str, node: &str, xcode_version: &str, build: &str) -> Fakes {
    fakes_for(mise_version, node, xcode_version, build, 0)
}

fn fakes_for(
    mise_version: &str,
    node: &str,
    xcode_version: &str,
    build: &str,
    ls_sleep_secs: u64,
) -> Fakes {
    let root = tempfile::Builder::new()
        .prefix("ec-tools-")
        .tempdir_in("/tmp")
        .unwrap();
    let dir = root.path();
    let listing = dir.join("ls.json");
    let global = dir.join("ls-global.json");
    fs::write(
        &listing,
        format!(r#"{{"node":[{{"version":"{node}","installed":true,"active":false}}]}}"#),
    )
    .unwrap();
    fs::write(
        &global,
        format!(r#"{{"node":[{{"version":"{node}","installed":true,"active":true}}]}}"#),
    )
    .unwrap();
    let pause = if ls_sleep_secs == 0 {
        String::new()
    } else {
        format!("sleep {ls_sleep_secs}\n")
    };
    let mise = dir.join("mise");
    executable(
        &mise,
        &format!(
            r#"#!/bin/sh
set -eu
if [ "${{1:-}}" = "--version" ] && [ "$#" -eq 1 ]; then
  printf '%s\n' '{mise_version}'
  exit 0
fi
if [ "${{1:-}}" = "ls" ] && [ "${{2:-}}" = "--json" ] && [ "$#" -eq 2 ]; then
  {pause}cat '{listing}'
  exit 0
fi
if [ "${{1:-}}" = "ls" ] && [ "${{2:-}}" = "--global" ] && [ "${{3:-}}" = "--json" ] && [ "$#" -eq 3 ]; then
  cat '{global}'
  exit 0
fi
exit 2
"#,
            mise_version = mise_version,
            pause = pause,
            listing = utf8(&listing),
            global = utf8(&global),
        ),
    );
    let apps = dir.join("Applications");
    let app = apps.join(format!("Xcode-{xcode_version}.app"));
    let contents = app.join("Contents");
    fs::create_dir_all(contents.join("Developer")).unwrap();
    fs::write(
        contents.join("Info.plist"),
        xml_plist(&[
            ("CFBundleIdentifier", "com.apple.dt.Xcode"),
            ("CFBundleShortVersionString", xcode_version),
        ]),
    )
    .unwrap();
    fs::write(
        contents.join("version.plist"),
        xml_plist(&[("ProductBuildVersion", build)]),
    )
    .unwrap();
    let select = dir.join("xcode-select");
    executable(
        &select,
        &format!(
            "#!/bin/sh\nprintf '%s\\n' '{}/Contents/Developer'\n",
            utf8(&app)
        ),
    );
    Fakes {
        env: vec![
            ("ELSECALL_MISE".into(), utf8(&mise)),
            ("ELSECALL_XCODE_APPLICATIONS".into(), utf8(&apps)),
            ("ELSECALL_XCODE_SELECT".into(), utf8(&select)),
            ("ELSECALL_XCODES".into(), utf8(&dir.join("no-xcodes"))),
        ],
        _root: root,
    }
}

fn pairs(fakes: &Fakes) -> Vec<(&str, &str)> {
    fakes
        .env
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect()
}

fn utf8(path: &Path) -> String {
    path.to_str().expect("utf-8 path").to_owned()
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

fn names(data: &Value) -> Vec<&str> {
    data["members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|member| member["member"].as_str().unwrap())
        .collect()
}

fn assert_tools(entry: &Value, mise: &str, node: &str, xcode: &str, build: &str) {
    assert_eq!(entry["state"], "read", "{entry}");
    assert_eq!(entry["managers"][0]["manager"], "mise");
    assert_eq!(entry["managers"][0]["state"], "read");
    assert_eq!(entry["managers"][0]["executable"]["state"], "found");
    assert_eq!(entry["managers"][0]["executable"]["version"], mise);
    assert_eq!(
        entry["managers"][0]["tools"],
        serde_json::json!([{ "name": "node", "version": node, "default": true }])
    );
    assert_eq!(entry["managers"][1]["manager"], "xcodes");
    assert_eq!(entry["managers"][1]["state"], "read");
    assert_eq!(entry["managers"][1]["executable"]["state"], "missing");
    assert_eq!(entry["managers"][1]["tools"][0]["name"], "xcode");
    assert_eq!(entry["managers"][1]["tools"][0]["version"], xcode);
    assert_eq!(entry["managers"][1]["tools"][0]["build"], build);
    assert_eq!(entry["managers"][1]["tools"][0]["default"], true);
}

#[test]
fn fleet_inventory_shows_each_members_tools_and_an_unreachable_member() {
    let alpha_tools = fakes("2026.1.0", "22.11.0", "16.4", "16F6");
    let bravo_tools = fakes("2026.2.0", "20.18.0", "26.2", "17C52");
    let alpha_env = pairs(&alpha_tools);
    let bravo_env = pairs(&bravo_tools);
    let alpha = TestMember::start_with("alpha", "", &alpha_env);
    let mut bravo = TestMember::start_with("bravo", "", &bravo_env);
    enrol(&alpha, &bravo);

    let local = alpha.ok(&["tools", "inventory"]);
    assert!(local.get("members").is_none(), "{local}");
    assert_eq!(local["managers"][0]["executable"]["version"], "2026.1.0");

    let from_alpha = alpha.ok(&["tools", "inventory", "--fleet"]);
    assert_eq!(names(&from_alpha), ["alpha", "bravo"]);
    assert_tools(
        &from_alpha["members"][0],
        "2026.1.0",
        "22.11.0",
        "16.4",
        "16F6",
    );
    assert_tools(
        &from_alpha["members"][1],
        "2026.2.0",
        "20.18.0",
        "26.2",
        "17C52",
    );

    let from_bravo = bravo.ok(&["tools", "inventory", "--fleet"]);
    assert_eq!(names(&from_bravo), ["bravo", "alpha"]);
    assert_tools(
        &from_bravo["members"][0],
        "2026.2.0",
        "20.18.0",
        "26.2",
        "17C52",
    );

    let one = alpha.ok(&["tools", "inventory", "--member", "bravo"]);
    assert!(one.get("members").is_none(), "{one}");
    assert_eq!(one["member"], "bravo");
    assert_tools(&one, "2026.2.0", "20.18.0", "26.2", "17C52");
    let with_both = alpha.ok(&["tools", "inventory", "--fleet", "--member", "alpha"]);
    assert_eq!(with_both["member"], "alpha");
    assert_tools(&with_both, "2026.1.0", "22.11.0", "16.4", "16F6");

    let unknown = alpha.run(&["tools", "inventory", "--member", "nobody"]);
    assert_eq!(unknown.status.code(), Some(2), "{unknown:?}");
    let unknown = json(&unknown);
    assert_eq!(unknown["error"]["code"], "invalid_request");
    let message = unknown["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("alpha") && message.contains("bravo"),
        "{message}"
    );

    bravo.stop();
    let down = alpha.ok(&["tools", "inventory", "--fleet"]);
    assert_eq!(names(&down), ["alpha", "bravo"]);
    assert_eq!(down["members"][0]["state"], "read");
    assert_eq!(
        down["members"][0]["managers"][0]["executable"]["version"],
        "2026.1.0"
    );
    assert_eq!(down["members"][1]["state"], "unavailable");
    assert_eq!(down["members"][1]["error"]["code"], "member_unreachable");
    assert!(
        down["members"][1]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("bravo")
    );
    assert!(
        down["members"][1]["error"]["hint"]
            .as_str()
            .unwrap()
            .contains("elsecall doctor")
    );
    assert!(down["members"][1].get("managers").is_none());
}

#[test]
fn a_solo_machine_is_a_fleet_of_one() {
    let tools = fakes("2026.3.0", "18.20.0", "15.4", "15F31d");
    let env = pairs(&tools);
    let solo = TestMember::start_with("solo", "", &env);
    assert_eq!(solo.fleet()["enrolled"], false);
    let data = solo.ok(&["tools", "inventory", "--fleet"]);
    assert_eq!(names(&data), ["solo"]);
    assert_tools(&data["members"][0], "2026.3.0", "18.20.0", "15.4", "15F31d");
}

#[test]
fn a_slow_member_is_read_past_the_connection_bound() {
    let alpha_tools = fakes("2026.1.0", "22.11.0", "16.4", "16F6");
    let bravo_tools = fakes_for("2026.2.0", "20.18.0", "26.2", "17C52", 35);
    let alpha = TestMember::start_with("alpha", "", &pairs(&alpha_tools));
    let bravo = TestMember::start_with("bravo", "", &pairs(&bravo_tools));
    enrol(&alpha, &bravo);

    let started = Instant::now();
    let fleet = alpha.ok(&["tools", "inventory", "--fleet"]);
    assert!(
        started.elapsed() < Duration::from_secs(80),
        "took {:?}",
        started.elapsed()
    );
    assert_eq!(names(&fleet), ["alpha", "bravo"]);
    assert_eq!(fleet["members"][1]["state"], "read", "{fleet}");
    assert_eq!(fleet["members"][0]["state"], "read");
}

#[test]
fn a_member_past_the_budget_is_a_timeout() {
    let alpha_tools = fakes("2026.1.0", "22.11.0", "16.4", "16F6");
    let bravo_tools = fakes_for("2026.2.0", "20.18.0", "26.2", "17C52", 30);
    let mut alpha_env = pairs(&alpha_tools);
    alpha_env.push(("ELSECALL_TOOLS_MEMBER_TIMEOUT_SECS", "3"));
    let alpha = TestMember::start_with("alpha", "", &alpha_env);
    let bravo = TestMember::start_with("bravo", "", &pairs(&bravo_tools));
    enrol(&alpha, &bravo);

    let started = Instant::now();
    let fleet = alpha.ok(&["tools", "inventory", "--fleet"]);
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(12),
        "took {elapsed:?}, past the 3 second budget"
    );
    assert_eq!(names(&fleet), ["alpha", "bravo"]);
    assert_eq!(fleet["members"][0]["state"], "read", "{fleet}");
    assert_eq!(fleet["members"][1]["state"], "unavailable");
    assert_eq!(fleet["members"][1]["error"]["code"], "member_timeout");
    assert!(
        fleet["members"][1]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("3")
    );
}

#[test]
fn a_solo_machine_without_tailnet_presence_is_one_read_member() {
    let tools = fakes("2026.3.0", "18.20.0", "15.4", "15F31d");
    let solo = TestMember::start_without_tailnet("solo", &pairs(&tools));
    let output = solo.run(&["tools", "inventory", "--fleet"]);
    assert!(output.status.success(), "{output:?}");
    let data = json(&output)["data"].clone();
    let members = data["members"].as_array().unwrap();
    assert_eq!(members.len(), 1, "{data}");
    assert_eq!(members[0]["state"], "read");
    assert_tools(&members[0], "2026.3.0", "18.20.0", "15.4", "15F31d");
}

#[test]
fn an_oversized_peer_inventory_is_too_large_and_the_other_member_is_read() {
    let alpha_tools = fakes("2026.1.0", "22.11.0", "16.4", "16F6");
    let version = "1".repeat(600_000);
    let bravo_tools = fakes("2026.2.0", &version, "26.2", "17C52");
    let alpha = TestMember::start_with("alpha", "", &pairs(&alpha_tools));
    let bravo = TestMember::start_with("bravo", "", &pairs(&bravo_tools));
    enrol(&alpha, &bravo);

    let fleet = alpha.ok(&["tools", "inventory", "--fleet"]);
    assert_eq!(names(&fleet), ["alpha", "bravo"]);
    assert_tools(&fleet["members"][0], "2026.1.0", "22.11.0", "16.4", "16F6");
    assert_eq!(fleet["members"][1]["state"], "unavailable", "{fleet}");
    assert_eq!(fleet["members"][1]["error"]["code"], "inventory_too_large");
    assert!(fleet["members"][1].get("managers").is_none());
}
