//! Installing each member's plan through real member services, with a fake mise
//! at the system boundary that records what it was asked to install.
#![cfg(target_os = "macos")]
use crate::{
    members::{TestMember, enrol},
    tools_plan::{bare, equipped, executable, plan_with, utf8, workspace, write},
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
            .all(|(_, _, state)| state == "queued" || state == "running"),
        "{started}"
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
            ("iOS".into(), "26.2".into(), "skipped".into()),
        ]
    );
    assert_eq!(
        apply["steps"][0]["options"],
        json!({"version_prefix": "bun-v"})
    );
    assert_eq!(apply["steps"][1]["stage"], "install");
    assert_eq!(apply["steps"][1]["code"], "tool_install_failed");
    assert_eq!(apply["steps"][2]["reason"], "not_supported");

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
        2,
        "one admitted run per installed step: {runs}"
    );

    let view = alpha.fleet();
    let items: Vec<&Value> = view["attention"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["category"] == "provisioning_step_failed")
        .collect();
    assert_eq!(items.len(), 1, "{view}");
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
        1,
        "the retried step failed again and still needs attention: {view}"
    );
}

fn view_status(viewer: &TestMember, member: &str) -> Value {
    viewer.view_of(member)["status"].clone()
}
