//! The plan per member, through real member services: workspaces where runs
//! started on each member, the master's global config, exceptions, and fake
//! tool managers at the system boundary.
#![cfg(target_os = "macos")]
use crate::members;

use members::{TestMember, enrol};
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

pub(crate) fn utf8(path: &Path) -> String {
    path.to_str().expect("utf-8 path").to_owned()
}

pub(crate) fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

pub(crate) fn executable(path: &Path, body: &str) {
    write(path, body);
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// Fake managers for a member: what mise, Xcode and simctl report.
pub(crate) struct Managers {
    _root: tempfile::TempDir,
    pub(crate) env: Vec<(String, String)>,
}

impl Managers {
    pub(crate) fn pairs(&self) -> Vec<(&str, &str)> {
        self.env
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
            .collect()
    }
}

/// The master: node 22.19.0 (the global default), two buns, Xcode 26.2 and
/// the iOS 26.2 runtime.
pub(crate) fn equipped() -> Managers {
    let root = tempfile::Builder::new()
        .prefix("ec-plan-")
        .tempdir_in("/tmp")
        .unwrap();
    let dir = root.path();
    write(
        &dir.join("ls.json"),
        r#"{"node":[{"version":"22.19.0","installed":true,"active":false}],
            "bun":[{"version":"1.3.5","installed":true,"active":false},
                   {"version":"1.3.14","installed":true,"active":false}]}"#,
    );
    write(
        &dir.join("ls-global.json"),
        r#"{"node":[{"version":"22.19.0","installed":true,"active":true}]}"#,
    );
    executable(
        &dir.join("mise"),
        &format!(
            "#!/bin/sh\ncase \"$*\" in\n  --version) echo 2026.10.11 ;;\n  'ls --json') cat '{0}/ls.json' ;;\n  'ls --global --json') cat '{0}/ls-global.json' ;;\n  *) exit 2 ;;\nesac\n",
            utf8(dir)
        ),
    );
    let contents = dir.join("Applications/Xcode-26.2.0.app/Contents");
    fs::create_dir_all(contents.join("Developer")).unwrap();
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
    write(
        &dir.join("runtimes.json"),
        r#"{"A":{"platformIdentifier":"com.apple.platform.iphonesimulator","version":"26.2","build":"23C54","state":"Ready"}}"#,
    );
    executable(
        &dir.join("simctl"),
        &format!(
            "#!/bin/sh\ncase \"$*\" in\n  --version) echo 'PROJECT:CoreSimulator-1051.17.7' ;;\n  'runtime list -j') cat '{}/runtimes.json' ;;\n  *) exit 2 ;;\nesac\n",
            utf8(dir)
        ),
    );
    executable(&dir.join("xcode-select"), "#!/bin/sh\nexit 1\n");
    Managers {
        env: vec![
            ("ELSECALL_MISE".into(), utf8(&dir.join("mise"))),
            (
                "ELSECALL_XCODE_APPLICATIONS".into(),
                utf8(&dir.join("Applications")),
            ),
            (
                "ELSECALL_XCODE_SELECT".into(),
                utf8(&dir.join("xcode-select")),
            ),
            ("ELSECALL_XCODES".into(), utf8(&dir.join("no-xcodes"))),
            ("ELSECALL_SIMCTL".into(), utf8(&dir.join("simctl"))),
        ],
        _root: root,
    }
}

/// A bare Mac: Command Line Tools only, so no mise, no Xcode, no simctl.
pub(crate) fn bare() -> Managers {
    let root = tempfile::Builder::new()
        .prefix("ec-plan-")
        .tempdir_in("/tmp")
        .unwrap();
    let dir = root.path();
    fs::create_dir_all(dir.join("Applications")).unwrap();
    executable(&dir.join("xcode-select"), "#!/bin/sh\nexit 1\n");
    Managers {
        env: vec![
            ("ELSECALL_MISE".into(), utf8(&dir.join("no-mise"))),
            (
                "ELSECALL_XCODE_APPLICATIONS".into(),
                utf8(&dir.join("Applications")),
            ),
            (
                "ELSECALL_XCODE_SELECT".into(),
                utf8(&dir.join("xcode-select")),
            ),
            ("ELSECALL_XCODES".into(), utf8(&dir.join("no-xcodes"))),
            ("ELSECALL_SIMCTL".into(), utf8(&dir.join("no-simctl"))),
        ],
        _root: root,
    }
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

/// A git repository under the member's home with `files`, where a run then starts.
pub(crate) fn workspace(member: &TestMember, name: &str, files: &[(&str, &str)]) -> PathBuf {
    let root = member.home().join("Developer").join(name);
    for (file, text) in files {
        write(&root.join(file), text);
    }
    let git = std::process::Command::new("/usr/bin/git")
        .args(["init", "-q"])
        .current_dir(&root)
        .output()
        .unwrap();
    assert!(git.status.success(), "{git:?}");
    let run = crate::processes::command(env!("CARGO_BIN_EXE_elsecall"))
        .env("HOME", member.home())
        .current_dir(&root)
        .args(["run", "--pinned", "--", "/usr/bin/true"])
        .output()
        .unwrap();
    assert!(run.status.success(), "{run:?}");
    root
}

/// The plan once both workspaces are known: admission records them aside.
pub(crate) fn plan_with(master: &TestMember, workspaces: usize, args: &[&str]) -> Value {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let plan = master.ok(args);
        if plan["workspaces"].as_array().unwrap().len() == workspaces {
            return plan;
        }
        assert!(Instant::now() < deadline, "workspaces missing: {plan}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn tools(entries: &Value) -> Vec<(String, String, Value)> {
    entries
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| {
            (
                entry["tool"].as_str().unwrap().to_owned(),
                entry["version"].as_str().unwrap_or("-").to_owned(),
                entry["exact"].clone(),
            )
        })
        .collect()
}

fn named(tool: &str, version: &str, exact: Option<bool>) -> (String, String, Value) {
    (tool.into(), version.into(), json!(exact))
}

#[test]
fn each_member_is_planned_from_every_members_workspaces() {
    let master_tools = equipped();
    let bare_tools = bare();
    let alpha = TestMember::start_with("alpha", "", &master_tools.pairs());
    let bravo = TestMember::start_with("bravo", "", &bare_tools.pairs());
    enrol(&alpha, &bravo);
    write(
        &alpha.home().join(".config/mise/config.toml"),
        "[tools]\nnode = \"22\"\njava = \"lts\"\n",
    );
    write(
        &bravo.home().join(".config/mise/config.toml"),
        "[tools]\nruby = \"3.3.0\"\n",
    );
    workspace(
        &alpha,
        "acme-ios",
        &[
            (
                "mise.toml",
                "[tools]\nnode = \"22.19.0\"\nbun = \"latest\"\npython = \"3.10.4\"\nsentry-cli = \"latest\"\n",
            ),
            (".xcode-version", "26.2\n"),
        ],
    );
    workspace(
        &bravo,
        "acme-tools",
        &[(
            "mise.toml",
            "[tools]\nbun = { version = \"1.3.5\", version_prefix = \"bun-v\", postinstall = \"touch /tmp/never\" }\n",
        )],
    );
    alpha.ok(&["tools", "leave-out", "mise:sentry-cli"]);
    alpha.ok(&["tools", "set", "xcode", "26.3", "--member", "bravo"]);

    let plan = plan_with(&alpha, 2, &["tools", "plan"]);
    assert_eq!(plan["master"], "alpha");
    assert_eq!(
        plan["global"],
        json!({"file": "~/.config/mise/config.toml", "state": "read"})
    );
    let workspaces: Vec<(&str, &str, &str)> = plan["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| {
            (
                entry["member"].as_str().unwrap(),
                entry["workspace"].as_str().unwrap(),
                entry["state"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        workspaces,
        [
            ("alpha", "~/Developer/acme-ios", "read"),
            ("bravo", "~/Developer/acme-tools", "read"),
        ]
    );
    assert_eq!(plan["unread_members"], json!([]));
    assert_eq!(plan["warnings"], json!([]));
    let members = plan["members"].as_array().unwrap();
    assert_eq!(members.len(), 2);

    let master = &members[0];
    assert_eq!(master["member"], "alpha");
    assert_eq!(master["state"], "planned");
    assert_eq!(master["steps"], json!([]), "{master}");
    assert_eq!(
        tools(&master["installed"]),
        [
            named("bun", "1.3.5", None),
            named("bun", "1.3.14", None),
            named("node", "22.19.0", None),
            named("xcode", "26.2", None),
            named("iOS", "26.2", None),
        ]
    );
    assert_eq!(
        tools(&master["unresolved"]),
        [named("java", "lts", None), named("python", "3.10.4", None)]
    );
    assert!(
        master["unresolved"][1]["hint"]
            .as_str()
            .unwrap()
            .contains("alpha"),
        "{master}"
    );

    let member = &members[1];
    assert_eq!(member["member"], "bravo");
    assert_eq!(
        tools(&member["steps"]),
        [
            named("bun", "1.3.5", Some(true)),
            named("bun", "1.3.14", Some(false)),
            named("node", "22.19.0", Some(false)),
            named("xcode", "26.3", Some(true)),
            named("iOS", "26.3", Some(true)),
        ],
        "{member}"
    );
    let options: Vec<&Value> = member["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|step| &step["options"])
        .collect();
    assert_eq!(
        options,
        [
            &json!({"version_prefix": "bun-v"}),
            &json!({}),
            &json!({}),
            &json!({}),
            &json!({})
        ],
        "a member's workspace carries its install options to the plan, and nothing else"
    );
    let bun = &member["steps"][1];
    assert_eq!(
        bun["reasons"],
        json!([{"kind": "pin", "member": "alpha", "workspace": "~/Developer/acme-ios",
                "source": "mise_project", "file": "mise.toml", "version": "latest",
                "environment": null}])
    );
    assert!(bun["hint"].as_str().unwrap().contains("alpha"), "{bun}");
    let node_reasons: Vec<(&str, Value)> = member["steps"][2]["reasons"]
        .as_array()
        .unwrap()
        .iter()
        .map(|reason| {
            (
                reason["version"].as_str().unwrap(),
                reason["workspace"].clone(),
            )
        })
        .collect();
    assert_eq!(
        node_reasons,
        [
            ("22", json!(null)),
            ("22.19.0", json!("~/Developer/acme-ios"))
        ]
    );
    assert_eq!(
        member["steps"][3]["reasons"],
        json!([{"kind": "exception", "file": "fleet-tools.toml",
                "key": "members.bravo.xcodes.tools.xcode"}])
    );
    assert_eq!(
        member["steps"][4]["reasons"],
        json!([{"kind": "exception_runtime", "file": "fleet-tools.toml",
                "key": "members.bravo.xcodes.tools.xcode", "xcode": "26.3"}])
    );
    let exceptions: Vec<(&str, &str, Value)> = member["exceptions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|exception| {
            (
                exception["tool"].as_str().unwrap(),
                exception["kind"].as_str().unwrap(),
                exception["pinned"].clone(),
            )
        })
        .collect();
    assert_eq!(
        exceptions,
        [
            ("sentry-cli", "leave_out", json!(["latest"])),
            ("xcode", "versions", json!(["26.2"])),
        ]
    );
    assert!(
        !plan.to_string().contains("ruby"),
        "only the master's global config counts"
    );

    let one = alpha.ok(&["tools", "plan", "--member", "bravo"]);
    assert_eq!(one["members"].as_array().unwrap().len(), 1);
    assert_eq!(one["members"][0]["member"], "bravo");
    assert_eq!(one["workspaces"].as_array().unwrap().len(), 2);
    let unknown = alpha.run(&["tools", "plan", "--member", "nobody"]);
    assert_eq!(unknown.status.code(), Some(2), "{unknown:?}");
    assert_eq!(bravo.fails(&["tools", "plan"]), "not_master");
}

#[test]
fn a_broken_workspace_and_an_unreachable_member_are_reported_not_planned() {
    let master_tools = equipped();
    let bare_tools = bare();
    let alpha = TestMember::start_with("alpha", "", &master_tools.pairs());
    let mut bravo = TestMember::start_with("bravo", "", &bare_tools.pairs());
    enrol(&alpha, &bravo);
    let root = workspace(
        &alpha,
        "acme-ios",
        &[
            ("mise.toml", "[tools]\nnode = \"22.19.0\"\n"),
            (".tool-versions", "node 20.0.0\n"),
        ],
    );
    let plan = plan_with(&alpha, 1, &["tools", "plan"]);
    assert_eq!(plan["workspaces"][0]["state"], "failed");
    assert_eq!(plan["workspaces"][0]["error"]["code"], "pins_conflict");
    assert_eq!(plan["members"][1]["steps"], json!([]));

    fs::remove_file(root.join(".tool-versions")).unwrap();
    bravo.stop();
    let plan = alpha.ok(&["tools", "plan"]);
    assert_eq!(plan["workspaces"][0]["state"], "read");
    assert_eq!(plan["unread_members"][0]["member"], "bravo");
    assert_eq!(
        plan["unread_members"][0]["error"]["code"],
        "member_unreachable"
    );
    assert_eq!(plan["members"][1]["state"], "unavailable");
    assert_eq!(plan["members"][1]["error"]["code"], "member_unreachable");
}

#[test]
fn a_solo_machine_plans_itself_and_runs_outside_a_tree_add_no_workspace() {
    let master_tools = equipped();
    let solo = TestMember::start_with("solo", "", &master_tools.pairs());
    let outside = crate::processes::command(env!("CARGO_BIN_EXE_elsecall"))
        .env("HOME", solo.home())
        .current_dir(solo.home())
        .args(["run", "--pinned", "--", "/usr/bin/true"])
        .output()
        .unwrap();
    assert!(outside.status.success(), "{outside:?}");
    workspace(&solo, "acme-ios", &[(".xcode-version", "26.2\n")]);
    let plan = plan_with(&solo, 1, &["tools", "plan"]);
    assert_eq!(plan["master"], "solo");
    assert_eq!(plan["global"]["state"], "missing");
    assert_eq!(plan["members"][0]["steps"], json!([]));
    assert_eq!(
        tools(&plan["members"][0]["installed"]),
        [named("xcode", "26.2", None), named("iOS", "26.2", None)]
    );
}

/// The plan on this machine's real tools and workspaces, as a master, with a
/// bare Mac beside it. Not part of `mise run check`. The services run under
/// scratch homes; the master's sees this machine's mise installs and global
/// config through links. `ELSECALL_LIVE_WORKSPACES` lists the workspaces to
/// start a run in, separated by `:`.
#[test]
#[ignore]
fn live_plan_of_this_machine() {
    let real_home = PathBuf::from(std::env::var_os("HOME").unwrap());
    let bare_tools = bare();
    let mise = real_home.join(".local/bin/mise");
    let alpha = TestMember::start_with("mac-mini", "", &[("ELSECALL_MISE", &utf8(&mise))]);
    let bravo = TestMember::start_with("new-mac", "", &bare_tools.pairs());
    enrol(&alpha, &bravo);
    let share = alpha.home().join(".local/share");
    fs::create_dir_all(&share).unwrap();
    std::os::unix::fs::symlink(real_home.join(".local/share/mise"), share.join("mise")).unwrap();
    let global = real_home.join(".config/mise/config.toml");
    if global.exists() {
        write(
            &alpha.home().join(".config/mise/config.toml"),
            &fs::read_to_string(&global).unwrap(),
        );
    }
    let roots = std::env::var("ELSECALL_LIVE_WORKSPACES").unwrap_or_default();
    let roots: Vec<&str> = roots.split(':').filter(|root| !root.is_empty()).collect();
    for root in &roots {
        let run = crate::processes::command(env!("CARGO_BIN_EXE_elsecall"))
            .env("HOME", alpha.home())
            .current_dir(root)
            .args(["run", "--pinned", "--", "/usr/bin/true"])
            .output()
            .unwrap();
        assert!(run.status.success(), "{run:?}");
    }
    let started = Instant::now();
    let plan = plan_with(&alpha, roots.len(), &["tools", "plan"]);
    println!("planned in {:?}", started.elapsed());
    println!("{}", serde_json::to_string_pretty(&plan).unwrap());
}
