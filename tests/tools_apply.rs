//! Installing each member's plan through real member services, with a fake mise
//! at the system boundary that records what it was asked to install.
#![cfg(target_os = "macos")]
use crate::{
    members::{TestMember, enrol},
    tools_plan::{bare, equipped, executable, plan_with, plist, utf8, workspace, write},
};
use serde_json::{Value, json};
use std::{fs, path::PathBuf};

/// A mise that has nothing installed, logs each install with where it ran and
/// which `MISE*` variables it saw, and fails to install node.
struct Mise {
    _root: tempfile::TempDir,
    executable: PathBuf,
    log: PathBuf,
}

fn fake_mise() -> Mise {
    let root = tempfile::Builder::new()
        .prefix("ec-apply-")
        .tempdir_in("/tmp")
        .unwrap();
    let log = root.path().join("installs.log");
    let executable_path = root.path().join("mise");
    executable(
        &executable_path,
        &format!(
            "#!/bin/sh\ncase \"$1\" in\n  --version) echo 2026.10.11 ;;\n  ls) echo '{{}}' ;;\n  install)\n    echo \"$*|$PWD|$(env | grep '^MISE' | sort | tr '\\n' ' ')\" >> '{log}'\n    case \"$2\" in node@*) exit 1 ;; esac ;;\n  *) exit 2 ;;\nesac\n",
            log = utf8(&log)
        ),
    );
    Mise {
        _root: root,
        executable: executable_path,
        log,
    }
}

fn steps(apply: &Value) -> Vec<(String, String, String)> {
    apply["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|step| {
            (
                step["tool"].as_str().unwrap().to_owned(),
                step["version"].as_str().unwrap().to_owned(),
                step["state"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

#[test]
fn each_member_installs_its_plan_as_admitted_work_and_failures_need_attention() {
    let master_tools = equipped();
    let mut bare_tools = bare();
    let mise = fake_mise();
    for (key, value) in &mut bare_tools.env {
        if key == "ELSECALL_MISE" {
            *value = utf8(&mise.executable);
        }
    }
    bare_tools
        .env
        .push(("MISE_DATA_DIR".into(), "/tmp/never-used".into()));
    let alpha = TestMember::start_with("alpha", "", &master_tools.pairs());
    let bravo = TestMember::start_with("bravo", "", &bare_tools.pairs());
    enrol(&alpha, &bravo);
    write(
        &alpha.home().join(".config/mise/config.toml"),
        "[tools]\nnode = \"22\"\n",
    );
    workspace(
        &alpha,
        "acme-ios",
        &[
            (
                "mise.toml",
                "[tools]\nbun = { version = \"1.3.5\", version_prefix = \"bun-v\", postinstall = \"touch /tmp/never\" }\n",
            ),
            (".xcode-version", "26.2\n"),
        ],
    );
    let plan = plan_with(&alpha, 1, &["tools", "plan"]);
    let dry = alpha.ok(&["tools", "apply", "--dry-run"]);
    assert_eq!(dry, plan, "a dry run is the plan");
    assert!(!mise.log.exists(), "a plan and a dry run install nothing");
    assert_eq!(bravo.fails(&["tools", "apply"]), "not_master");

    let started = alpha.ok(&["tools", "apply"]);
    assert_eq!(started["master"], "alpha");
    assert_eq!(
        started["members"][0],
        json!({"member": "alpha", "state": "nothing_to_install", "unresolved": 0})
    );
    let bravo_started = &started["members"][1];
    assert_eq!(bravo_started["state"], "started", "{started}");
    assert!(
        steps(&bravo_started["apply"])
            .iter()
            .all(|(tool, _, state)| {
                if tool == "xcode" {
                    state == "skipped"
                } else {
                    state == "queued" || state == "running"
                }
            }),
        "managers that do not install yet are skipped before anything runs: {started}"
    );

    let finished = alpha.ok(&["tools", "applied", "--member", "bravo", "--wait"]);
    let apply = &finished["members"][0]["apply"];
    assert_eq!(finished["members"][0]["state"], "finished", "{finished}");
    assert_eq!(
        steps(apply),
        [
            ("bun".into(), "1.3.5".into(), "installed".into()),
            ("node".into(), "22.19.0".into(), "failed".into()),
            ("xcode".into(), "26.2".into(), "skipped".into()),
            ("iOS".into(), "26.2".into(), "failed".into()),
        ]
    );
    assert_eq!(
        apply["steps"][0]["options"],
        json!({"version_prefix": "bun-v"})
    );
    assert_eq!(apply["steps"][1]["stage"], "install");
    assert_eq!(apply["steps"][1]["code"], "tool_install_failed");
    assert_eq!(apply["steps"][2]["reason"], "not_supported");
    assert_eq!(
        (&apply["steps"][3]["stage"], &apply["steps"][3]["code"]),
        (&json!("download"), &json!("xcode_missing")),
        "a runtime needs an Xcode to download it with"
    );

    let log = fs::read_to_string(&mise.log).unwrap();
    let home = utf8(&fs::canonicalize(bravo.home()).unwrap());
    assert_eq!(
        log.lines().collect::<Vec<_>>(),
        [
            format!("install bun[version_prefix=bun-v]@1.3.5|{home}|MISE_YES=1 "),
            format!("install node@22.19.0|{home}|MISE_YES=1 "),
        ],
        "versions named explicitly, in the home folder, with mise's variables removed"
    );

    let runs = bravo.ok(&["runs"]);
    let installs: Vec<&Value> = runs["runs"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|run| run["label"] == "elsecall tools install")
        .collect();
    assert_eq!(
        installs.len(),
        3,
        "one admitted run per step that ran, skipped ones not: {runs}"
    );

    let view = alpha.fleet();
    let items: Vec<&Value> = view["attention"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["category"] == "provisioning_step_failed")
        .collect();
    assert_eq!(items.len(), 2, "{view}");
    assert_eq!(items[1]["subject"], "simulators:iOS@26.2");
    assert_eq!(
        items[0]["id"],
        "provisioning_step_failed:bravo:mise:node@22.19.0"
    );
    assert_eq!(items[0]["code"], "tool_install_failed");
    assert_eq!(
        items[0]["actions"][0]["command"],
        json!(["elsecall", "tools", "apply", "--member", "bravo"])
    );
    assert_eq!(items[0]["actions"][0]["run_on"], "alpha");
    assert_eq!(
        view_status(&alpha, "bravo")["provisioning"]["failed"][0]["code"],
        "tool_install_failed"
    );

    let again = alpha.ok(&["tools", "apply", "--member", "bravo", "--wait"]);
    assert_eq!(again["members"][0]["state"], "finished", "{again}");
    let view = alpha.fleet();
    assert_eq!(
        view["attention"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|item| item["category"] == "provisioning_step_failed")
            .count(),
        2,
        "the retried steps failed again and still need attention: {view}"
    );
    let everyone = alpha.ok(&["tools", "apply", "--wait"]);
    assert_eq!(
        everyone["members"][0]["state"], "nothing_to_install",
        "--wait keeps the members it did not start: {everyone}"
    );
    assert_eq!(everyone["members"][1]["state"], "finished", "{everyone}");

    for tool in ["mise:node", "mise:bun", "xcode"] {
        alpha.ok(&["tools", "leave-out", tool]);
    }
    let emptied = alpha.ok(&["tools", "apply", "--member", "bravo"]);
    assert_eq!(
        emptied["members"][0]["state"], "nothing_to_install",
        "{emptied}"
    );
    assert_eq!(
        failed_items(&alpha),
        0,
        "an apply with nothing to install ends the last one's failures"
    );
}

fn failed_items(viewer: &TestMember) -> usize {
    viewer.fleet()["attention"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["category"] == "provisioning_step_failed")
        .count()
}

#[test]
fn a_restart_cuts_a_running_step_short_without_blocking_the_next_apply() {
    let master_tools = equipped();
    let mut bare_tools = bare();
    let root = tempfile::Builder::new()
        .prefix("ec-apply-")
        .tempdir_in("/tmp")
        .unwrap();
    let slow = root.path().join("mise");
    executable(
        &slow,
        "#!/bin/sh
case \"$1\" in\n  --version) echo 2026.10.11 ;;\n  ls) echo '{}' ;;\n  install) exec sleep 60 ;;\nesac\n",
    );
    for (key, value) in &mut bare_tools.env {
        if key == "ELSECALL_MISE" {
            *value = utf8(&slow);
        }
    }
    let alpha = TestMember::start_with("alpha", "", &master_tools.pairs());
    let mut bravo = TestMember::start_with("bravo", "", &bare_tools.pairs());
    enrol(&alpha, &bravo);
    workspace(
        &alpha,
        "acme-ios",
        &[("mise.toml", "[tools]\nbun = \"1.3.5\"\n")],
    );
    plan_with(&alpha, 1, &["tools", "plan"]);
    alpha.ok(&["tools", "apply", "--member", "bravo"]);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while alpha.ok(&["tools", "applied", "--member", "bravo"])["members"][0]["apply"]["steps"][0]["state"]
        != "running"
    {
        assert!(std::time::Instant::now() < deadline, "the step never ran");
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let busy = alpha.ok(&["tools", "apply", "--member", "bravo"]);
    assert_eq!(busy["members"][0]["state"], "refused", "{busy}");
    assert_eq!(busy["members"][0]["error"]["code"], "apply_in_progress");
    bravo.restart();
    let read = alpha.ok(&["tools", "applied", "--member", "bravo"]);
    assert_eq!(read["members"][0]["state"], "finished", "{read}");
    assert_eq!(
        read["members"][0]["apply"]["steps"][0]["code"],
        "apply_interrupted"
    );
    let again = alpha.ok(&["tools", "apply", "--member", "bravo"]);
    assert_eq!(again["members"][0]["state"], "started", "{again}");
}

fn view_status(viewer: &TestMember, member: &str) -> Value {
    viewer.view_of(member)["status"].clone()
}

#[test]
fn a_member_without_mise_copies_the_masters_before_installing() {
    let master_tools = equipped();
    let mut bare_tools = bare();
    let mut target = PathBuf::new();
    for (key, value) in &mut bare_tools.env {
        if key == "ELSECALL_MISE" {
            target = PathBuf::from(value.as_str())
                .with_file_name("bin")
                .join("mise");
            *value = utf8(&target);
        }
    }
    let master_mise = PathBuf::from(
        &master_tools
            .env
            .iter()
            .find(|(key, _)| key == "ELSECALL_MISE")
            .unwrap()
            .1,
    );
    let alpha = TestMember::start_with("alpha", "", &master_tools.pairs());
    let bravo = TestMember::start_with("bravo", "", &bare_tools.pairs());
    enrol(&alpha, &bravo);
    workspace(
        &alpha,
        "acme-ios",
        &[("mise.toml", "[tools]\nbun = \"1.3.5\"\n")],
    );
    plan_with(&alpha, 1, &["tools", "plan"]);
    assert!(!target.exists(), "bravo starts without mise");

    let done = alpha.ok(&["tools", "apply", "--member", "bravo", "--wait"]);
    let step = &done["members"][0]["apply"]["steps"][0];
    assert_eq!(step["tool"], "bun", "{done}");
    assert_eq!(
        (step["state"].as_str(), step["stage"].as_str()),
        (Some("failed"), Some("install")),
        "mise was copied; the master's fake mise then refuses to install: {done}"
    );
    assert_eq!(fs::read(&target).unwrap(), fs::read(&master_mise).unwrap());
    let mode =
        std::os::unix::fs::PermissionsExt::mode(&fs::metadata(&target).unwrap().permissions());
    assert_eq!(mode & 0o777, 0o755);
    let left: Vec<_> = fs::read_dir(target.parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(left, ["mise"], "nothing staged is left behind");
    let inventory = bravo.ok(&["tools", "inventory"]);
    assert_eq!(
        inventory["managers"][0]["executable"]["state"], "found",
        "{inventory}"
    );
}

#[test]
fn a_master_without_mise_has_none_to_copy_and_later_steps_fail_with_it() {
    let master_tools = bare();
    let bare_tools = bare();
    let alpha = TestMember::start_with("alpha", "", &master_tools.pairs());
    let bravo = TestMember::start_with("bravo", "", &bare_tools.pairs());
    enrol(&alpha, &bravo);
    workspace(
        &alpha,
        "acme-ios",
        &[(
            "mise.toml",
            "[tools]\nbun = \"1.3.5\"\nnode = \"22.19.0\"\n",
        )],
    );
    plan_with(&alpha, 1, &["tools", "plan"]);
    alpha.ok(&["tools", "set", "mise:bun", "1.3.5", "--member", "bravo"]);
    alpha.ok(&["tools", "set", "mise:node", "22.19.0", "--member", "bravo"]);

    let done = alpha.ok(&["tools", "apply", "--member", "bravo", "--wait"]);
    let steps = done["members"][0]["apply"]["steps"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(steps.len(), 2, "{done}");
    for step in &steps {
        assert_eq!(
            (&step["stage"], &step["code"]),
            (&json!("bootstrap"), &json!("artifact_unavailable")),
            "{done}"
        );
    }
    let runs = bravo.ok(&["runs"]);
    let installs = runs["runs"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|run| run["label"] == "elsecall tools install")
        .count();
    assert_eq!(
        installs, 1,
        "the second step fails without asking again: {runs}"
    );
}

#[test]
fn a_runtime_downloads_through_the_members_own_xcode() {
    let master_tools = equipped();
    let bare_tools = bare();
    let applications = PathBuf::from(
        &bare_tools
            .env
            .iter()
            .find(|(key, _)| key == "ELSECALL_XCODE_APPLICATIONS")
            .unwrap()
            .1,
    );
    let contents = applications.join("Xcode-26.2.app/Contents");
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
    let log = applications.join("xcodebuild.log");
    executable(
        &contents.join("Developer/usr/bin/xcodebuild"),
        &format!(
            "#!/bin/sh\necho \"$*|$DEVELOPER_DIR|$PWD\" >> '{}'\n",
            utf8(&log)
        ),
    );
    let alpha = TestMember::start_with("alpha", "", &master_tools.pairs());
    let bravo = TestMember::start_with("bravo", "", &bare_tools.pairs());
    enrol(&alpha, &bravo);
    workspace(&alpha, "acme-ios", &[(".xcode-version", "26.2\n")]);
    plan_with(&alpha, 1, &["tools", "plan"]);

    let done = alpha.ok(&["tools", "apply", "--member", "bravo", "--wait"]);
    assert_eq!(
        steps(&done["members"][0]["apply"]),
        [("iOS".into(), "26.2".into(), "installed".into())],
        "bravo has Xcode 26.2, so only its default runtime is a step: {done}"
    );
    assert_eq!(
        fs::read_to_string(&log).unwrap(),
        format!(
            "-downloadPlatform iOS -buildVersion 26.2 -architectureVariant arm64|{}|/\n",
            utf8(&contents.join("Developer"))
        )
    );
    drop(bravo);
}

#[test]
fn privileges_report_what_this_account_may_do_without_asking_for_anything() {
    let solo = TestMember::start("solo");
    let status = solo.ok(&["tools", "privileges"]);
    assert!(status["ready"].is_boolean(), "{status}");
    assert!(
        matches!(
            status["helper"].as_str(),
            Some("missing" | "stale" | "current")
        ),
        "{status}"
    );
    assert!(
        status["account"]
            .as_str()
            .is_some_and(|name| !name.is_empty())
    );
}
