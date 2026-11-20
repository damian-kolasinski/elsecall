//! Placement by pins between real member services: placeable work goes to a
//! member that has what its workspace pins, and to one that lacks it only when
//! no other can start it in time, with a warning. What each member lacks comes
//! from the master's provisioning loop; fake tool managers sit at the boundary.
#![cfg(target_os = "macos")]
use crate::{
    members::enrol,
    placement::{Machine, explain, placed, repository},
    tools_plan::{equipped, executable, utf8},
};
use std::time::{Duration, Instant};

fn until(what: &str, mut done: impl FnMut() -> bool, show: impl Fn() -> String) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !done() {
        assert!(
            Instant::now() < deadline,
            "{what} did not happen in time: {}",
            show()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn work_goes_where_the_pinned_tools_are_and_elsewhere_only_with_a_warning() {
    let master_tools = equipped();
    let charlie_tools = equipped();
    // bravo has no tools, and installing fails, so it keeps lacking them.
    let failing = tempfile::Builder::new()
        .prefix("ec-pins-")
        .tempdir_in("/tmp")
        .unwrap();
    let mise = failing.path().join("mise");
    executable(
        &mise,
        "#!/bin/sh\ncase \"$1\" in\n  --version) echo 2026.10.11 ;;\n  ls) echo '{}' ;;\n  *) exit 1 ;;\nesac\n",
    );
    let mut bravo_tools = equipped().pairs_owned();
    for (key, value) in &mut bravo_tools {
        if key == "ELSECALL_MISE" {
            *value = utf8(&mise);
        }
    }
    let mut master_env = master_tools.pairs();
    master_env.push(("ELSECALL_PROVISIONING_POLL_MS", "300"));
    let alpha = Machine::with_environment("alpha", 10, 1.0, "", &master_env);
    let bravo_env: Vec<(&str, &str)> = bravo_tools
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    let bravo = Machine::with_environment("bravo", 10, 4.0, "", &bravo_env);
    let charlie = Machine::with_environment("charlie", 10, 1.0, "", &charlie_tools.pairs());
    enrol(&alpha.member, &bravo.member);
    enrol(&alpha.member, &charlie.member);
    let (_temp, repo) = repository();
    std::fs::write(repo.join("mise.toml"), "[tools]\nbun = \"1.3.5\"\n").unwrap();
    // A run there makes it a workspace the plan reads.
    placed(&alpha.member, &repo, &["--pinned"]);

    for member in ["bravo", "charlie"] {
        alpha.member.ok(&["tools", "provision", member]);
    }
    until(
        "both members planned",
        || {
            let view = alpha.member.ok(&["tools", "provisioning"]);
            let state = |name: &str| {
                view["members"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|entry| entry["member"] == name)
                    .map(|entry| (entry["state"].clone(), entry["plan"]["steps"].clone()))
                    .unwrap()
            };
            state("charlie") == ("in_step".into(), 0.into())
                && state("bravo").1 == 1
                && state("bravo").0 != "applying"
                && state("bravo").0 != "waiting"
        },
        || alpha.member.ok(&["tools", "provisioning"]).to_string(),
    );

    // The caller is saturated; bravo is four times faster than charlie, but
    // lacks what the workspace pins.
    alpha.set_utilization(0.97);
    let (id, member) = placed(&alpha.member, &repo, &["--placeable"]);
    assert_eq!(member, "charlie");
    let record = explain(&alpha.member, &id);
    let reason = record["decision"]["reason"].as_str().unwrap();
    assert!(
        reason.contains("bravo was left out: it lacks bun 1.3.5, which the workspace pins"),
        "{record}"
    );
    assert_eq!(
        record["decision"]["scoring"]["passed_over"][0]["member"], "bravo",
        "{record}"
    );
    assert!(
        record["decision"]["unreachable"].is_null(),
        "bravo answered: {record}"
    );

    // With charlie saturated too, bravo is the only member that can start it.
    charlie.set_utilization(0.97);
    let (id, member) = placed(&alpha.member, &repo, &["--placeable"]);
    assert_eq!(member, "bravo");
    let record = explain(&alpha.member, &id);
    assert!(
        record["decision"]["reason"]
            .as_str()
            .unwrap()
            .contains("bravo lacks bun 1.3.5, which this workspace pins"),
        "{record}"
    );
    let warned = record.to_string();
    assert!(
        warned.contains("\"warnings\":[\"bravo lacks bun 1.3.5"),
        "the run carries the warning: {record}"
    );

    // Named with --member, it runs there, with what to expect.
    let (id, member) = placed(&alpha.member, &repo, &["--member", "bravo"]);
    assert_eq!(member, "bravo");
    let record = explain(&alpha.member, &id).to_string();
    assert!(
        record.contains("It runs there because it was asked to."),
        "{record}"
    );
}
