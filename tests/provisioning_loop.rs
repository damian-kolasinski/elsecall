//! The master's provisioning loop through real member services: a new member,
//! a changed pin and a tool removed by hand are each installed without a
//! command, the member becomes ready by itself, and a step only a person can
//! unblock is held, not retried. Fake tool managers sit at the system boundary.
#![cfg(target_os = "macos")]
use crate::{
    members::{TestMember, enrol_awaiting},
    tools_plan::{bare, equipped, executable, utf8, workspace, write},
};
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

/// How often the master's loop looks in this test.
const POLL: &str = "300";

/// A mise that installs into a state file the way mise fills its installs
/// folder, lists what it installed, and logs every install.
struct Mise {
    _root: tempfile::TempDir,
    executable: PathBuf,
    state: PathBuf,
    log: PathBuf,
}

fn installing_mise(home: &Path) -> Mise {
    let root = tempfile::Builder::new()
        .prefix("ec-loop-")
        .tempdir_in("/tmp")
        .unwrap();
    let state = root.path().join("installed");
    let log = root.path().join("installs.log");
    let installs = home.join(".local/share/mise/installs");
    let executable_path = root.path().join("mise");
    executable(
        &executable_path,
        &format!(
            r#"#!/bin/sh
case "${{0##*/}}" in mise) ;; *) exit 1 ;; esac
case "$1" in
  --version) echo 2026.10.11 ;;
  ls)
    if [ "$2" = --global ] || [ ! -s '{state}' ]; then echo '{{}}'; exit 0; fi
    awk 'BEGIN {{ printf "{{" }} {{ if (n++) printf ","; printf "\"%s\":[{{\"version\":\"%s\",\"installed\":true,\"active\":false}}]", $1, $2 }} END {{ print "}}" }}' '{state}' ;;
  install)
    echo "$2" >> '{log}'
    tool=${{2%%@*}}; tool=${{tool%%[*}}; version=${{2##*@}}
    mkdir -p '{installs}'/"$tool"/"$version"
    echo "$tool $version" >> '{state}' ;;
  *) exit 2 ;;
esac
"#,
            state = utf8(&state),
            log = utf8(&log),
            installs = utf8(&installs),
        ),
    );
    Mise {
        _root: root,
        executable: executable_path,
        state,
        log,
    }
}

impl Mise {
    fn installs(&self) -> Vec<String> {
        fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

fn wait_for(what: &str, limit: Duration, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + limit;
    while !done() {
        assert!(Instant::now() < deadline, "{what} did not happen in time");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn provisioning(master: &TestMember, member: &str) -> Value {
    master.ok(&["tools", "provisioning", "--member", member])["members"][0].clone()
}

fn latest_apply(master: &TestMember, member: &str) -> Value {
    master.ok(&["tools", "applied", "--member", member])["members"][0]["apply"].clone()
}

fn step<'a>(apply: &'a Value, tool: &str) -> &'a Value {
    apply["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|step| step["tool"] == tool)
        .unwrap_or_else(|| panic!("no {tool} step: {apply}"))
}

#[test]
fn the_master_keeps_a_new_member_in_step_by_itself() {
    let master_tools = equipped();
    let mut master_env = master_tools.pairs();
    master_env.push(("ELSECALL_PROVISIONING_POLL_MS", POLL));
    let alpha = TestMember::start_with("alpha", "", &master_env);
    let mut bare_tools = bare();
    let home = tempfile::Builder::new()
        .prefix("ec-loop-home-")
        .tempdir_in("/tmp")
        .unwrap();
    let mise = installing_mise(home.path());
    for (key, value) in &mut bare_tools.env {
        if key == "ELSECALL_MISE" {
            *value = utf8(&mise.executable);
        }
    }
    let mut bravo_env = bare_tools.pairs();
    let installs_home = utf8(&home.path().join(".local/share"));
    bravo_env.push(("XDG_DATA_HOME", &installs_home));
    let bravo = TestMember::start_with("bravo", "", &bravo_env);
    let pins = workspace(
        &alpha,
        "acme-ios",
        &[("mise.toml", "[tools]\nbun = \"1.3.5\"\n")],
    );

    // A new member: planned, installed and marked ready with no command.
    enrol_awaiting(&alpha, &bravo);
    assert_eq!(alpha.view_of("bravo")["readiness"], "awaiting_provisioning");
    wait_for("bravo becoming ready", Duration::from_secs(60), || {
        alpha.view_of("bravo")["readiness"] == "ready"
    });
    assert_eq!(mise.installs(), ["bun@1.3.5"]);
    assert_eq!(bravo.view_of("bravo")["readiness"], "ready");
    let entry = provisioning(&alpha, "bravo");
    assert_eq!(entry["state"], "in_step", "{entry}");
    assert_eq!(entry["plan"]["steps"], 0, "{entry}");
    assert_eq!(bravo.fails(&["tools", "provisioning"]), "not_master");

    // A pin changes in a workspace runs started in.
    write(
        &pins.join("mise.toml"),
        "[tools]\nbun = \"1.3.5\"\nnode = \"22.19.0\"\n",
    );
    wait_for("node being installed", Duration::from_secs(60), || {
        mise.installs().contains(&"node@22.19.0".to_owned())
    });

    // A tool removed by hand comes back.
    wait_for("the loop settling", Duration::from_secs(30), || {
        provisioning(&alpha, "bravo")["state"] == "in_step"
    });
    let kept: String = fs::read_to_string(&mise.state)
        .unwrap()
        .lines()
        .filter(|line| !line.starts_with("bun "))
        .map(|line| format!("{line}\n"))
        .collect();
    fs::write(&mise.state, kept).unwrap();
    fs::remove_dir_all(home.path().join(".local/share/mise/installs/bun")).unwrap();
    wait_for("bun being installed again", Duration::from_secs(60), || {
        mise.installs()
            .iter()
            .filter(|line| *line == "bun@1.3.5")
            .count()
            == 2
    });

    // Xcode needs privileges only a person installs: the step is held, its
    // item stands, and the loop does not retry it on its own.
    write(&pins.join(".xcode-version"), "26.2\n");
    wait_for("the Xcode step failing", Duration::from_secs(60), || {
        provisioning(&alpha, "bravo")["state"] == "needs_person"
    });
    let failed = latest_apply(&alpha, "bravo");
    let xcode = step(&failed, "xcode").clone();
    assert_eq!(xcode["code"], "xcode_privileges_missing", "{failed}");
    let runtime = step(&failed, "iOS").clone();
    assert_eq!(runtime["code"], "xcode_missing", "{failed}");
    let entry = provisioning(&alpha, "bravo");
    assert_eq!(
        entry["held"][0]["code"], "xcode_privileges_missing",
        "{entry}"
    );
    std::thread::sleep(Duration::from_secs(3));
    let later = latest_apply(&alpha, "bravo");
    assert_eq!(
        later["started_at"], failed["started_at"],
        "nothing changed, so nothing was sent again"
    );
    assert!(
        alpha.fleet()["attention"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["id"] == "provisioning_step_failed:bravo:xcodes:xcode@26.2"),
        "the item stands"
    );

    // Asking retries what retrying can fix and still holds what needs a person.
    let asked = alpha.ok(&["tools", "provision", "bravo"]);
    assert_eq!(asked["members"][0]["member"], "bravo", "{asked}");
    wait_for("the asked apply", Duration::from_secs(30), || {
        let apply = latest_apply(&alpha, "bravo");
        apply["started_at"] != failed["started_at"] && !apply["finished_at"].is_null()
    });
    let asked = latest_apply(&alpha, "bravo");
    assert_eq!(step(&asked, "xcode")["at"], xcode["at"], "held: {asked}");
    assert_ne!(step(&asked, "iOS")["at"], runtime["at"], "retried: {asked}");
    assert_eq!(alpha.view_of("bravo")["readiness"], "ready", "ready stays");
    let master = alpha.run(&["tools", "provision", "alpha"]);
    assert_eq!(
        master.status.code(),
        Some(2),
        "the master is what members are kept in step with"
    );
    assert_eq!(provisioning(&alpha, "alpha")["state"], "master");
}

#[test]
fn a_pin_changed_on_any_member_reaches_every_member() {
    let master_tools = equipped();
    let mut master_env = master_tools.pairs();
    master_env.push(("ELSECALL_PROVISIONING_POLL_MS", POLL));
    let alpha = TestMember::start_with("alpha", "", &master_env);
    let start_installing = |name: &'static str| {
        let home = tempfile::Builder::new()
            .prefix("ec-loop-home-")
            .tempdir_in("/tmp")
            .unwrap();
        let mise = installing_mise(home.path());
        let mut tools = bare();
        for (key, value) in &mut tools.env {
            if key == "ELSECALL_MISE" {
                *value = utf8(&mise.executable);
            }
        }
        tools.env.push((
            "XDG_DATA_HOME".into(),
            utf8(&home.path().join(".local/share")),
        ));
        let member = TestMember::start_with(name, "", &tools.pairs());
        (member, mise, tools, home)
    };
    let (bravo, _bravo_mise, _bravo_tools, _bravo_home) = start_installing("bravo");
    let (charlie, charlie_mise, _charlie_tools, _charlie_home) = start_installing("charlie");
    enrol_awaiting(&alpha, &bravo);
    enrol_awaiting(&alpha, &charlie);
    wait_for("both ready", Duration::from_secs(60), || {
        alpha.view_of("bravo")["readiness"] == "ready"
            && alpha.view_of("charlie")["readiness"] == "ready"
    });
    assert!(charlie_mise.installs().is_empty(), "nothing was pinned");

    // A run starts on bravo in a workspace that pins bun: charlie gets it too.
    workspace(
        &bravo,
        "acme-ios",
        &[("mise.toml", "[tools]\nbun = \"1.3.14\"\n")],
    );
    wait_for("charlie installing bun", Duration::from_secs(60), || {
        charlie_mise.installs().contains(&"bun@1.3.14".to_owned())
    });
}
