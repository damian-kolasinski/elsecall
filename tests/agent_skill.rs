#![cfg(target_os = "macos")]
//! `elsecall skill`: the guide this build carries, and the skill folders `setup`
//! writes into each coding agent's skills folder.
mod processes;
use serde_json::Value;
use std::{fs, path::Path, process::Output};

fn home() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("ec-")
        .tempdir_in("/tmp")
        .unwrap()
}

fn skill(home: &Path, args: &[&str]) -> Output {
    crate::processes::command(env!("CARGO_BIN_EXE_elsecall"))
        .env("HOME", home)
        .current_dir(home)
        .arg("skill")
        .args(args)
        .output()
        .unwrap()
}

fn json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|_| panic!("not JSON: {output:?}"))
}

/// Each agent's skill state, in the order Elsecall lists agents.
fn states(document: &Value) -> Vec<(String, String)> {
    document["data"]["agents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| {
            (
                a["agent"].as_str().unwrap().to_owned(),
                a["skill"]["state"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

fn pair(agent: &str, state: &str) -> (String, String) {
    (agent.to_owned(), state.to_owned())
}

#[test]
fn the_guide_and_its_topics_print_as_text_or_one_json_document() {
    let home = home();
    let guide = skill(home.path(), &["show"]);
    assert!(guide.status.success(), "{guide:?}");
    let text = String::from_utf8(guide.stdout).unwrap();
    assert!(text.starts_with("# Elsecall\n"), "{text}");
    assert!(text.contains("elsecall skill show NAME"));

    let topic = skill(home.path(), &["show", "integrate"]);
    assert!(topic.status.success());
    let topic = String::from_utf8(topic.stdout).unwrap();
    assert!(topic.starts_with("# Integrating a project\n"));
    assert!(topic.contains("## Outputs of placed work"));

    let document = json(&skill(home.path(), &["show", "integrate", "--json"]));
    assert_eq!(document["schema_version"], 1);
    assert_eq!(document["data"]["name"], "integrate");
    assert_eq!(document["data"]["text"], topic);

    let unknown = skill(home.path(), &["show", "nothing"]);
    assert_eq!(unknown.status.code(), Some(2));
    let error = json(&unknown);
    assert_eq!(error["error"]["code"], "invalid_request");
    assert_eq!(error["error"]["hint"], "Run elsecall skill show integrate.");
}

#[test]
fn setup_installs_the_skill_for_the_agents_found_and_status_reports_it() {
    let home = home();
    let root = home.path();
    fs::create_dir(root.join(".claude")).unwrap();
    fs::create_dir(root.join(".codex")).unwrap();

    let before = json(&skill(root, &["status"]));
    assert_eq!(
        states(&before),
        [
            pair("claude", "missing"),
            pair("codex", "missing"),
            pair("opencode", "missing")
        ]
    );
    assert_eq!(before["data"]["agents"][2]["found"], false);

    let dry = skill(root, &["setup", "--dry-run"]);
    assert!(dry.status.success(), "{dry:?}");
    assert_eq!(json(&dry)["data"]["agents"][0]["changed"], true);
    assert!(
        !root.join(".claude/skills").exists(),
        "a dry run writes nothing"
    );

    let setup = skill(root, &["setup"]);
    assert!(setup.status.success(), "{setup:?}");
    let written = fs::read_to_string(root.join(".claude/skills/elsecall/SKILL.md")).unwrap();
    assert!(written.starts_with("---\nname: elsecall\n"));
    assert!(written.contains("Run `elsecall skill show`"));
    assert_eq!(
        fs::read_to_string(root.join(".agents/skills/elsecall/SKILL.md")).unwrap(),
        written
    );
    assert_eq!(
        states(&json(&skill(root, &["status"]))),
        [
            pair("claude", "current"),
            pair("codex", "current"),
            pair("opencode", "current")
        ],
        "OpenCode reads the folder Codex does"
    );

    // An older build's skill is refreshed by setup.
    fs::write(
        root.join(".claude/skills/elsecall/SKILL.md"),
        written.replace("each topic", "every reference"),
    )
    .unwrap();
    assert_eq!(
        states(&json(&skill(root, &["status"])))[0],
        pair("claude", "outdated")
    );
    let again = json(&skill(root, &["setup"]));
    assert_eq!(again["data"]["agents"][0]["changed"], true);
    assert_eq!(again["data"]["agents"][1]["changed"], false);
    assert_eq!(
        fs::read_to_string(root.join(".claude/skills/elsecall/SKILL.md")).unwrap(),
        written
    );
}

#[test]
fn a_skill_elsecall_did_not_write_is_kept_unless_forced_and_never_removed() {
    let home = home();
    let root = home.path();
    let theirs = root.join(".agents/skills/elsecall");
    fs::create_dir_all(&theirs).unwrap();
    fs::write(theirs.join("SKILL.md"), "---\nname: elsecall\n---\nmine\n").unwrap();

    let dry = skill(root, &["setup", "--agent", "codex", "--dry-run"]);
    assert_eq!(dry.status.code(), Some(1), "{dry:?}");
    assert_eq!(json(&dry)["data"]["agents"][0]["skipped"], "foreign");

    let refused = skill(root, &["setup", "--agent", "codex", "--agent", "claude"]);
    assert_eq!(refused.status.code(), Some(1), "{refused:?}");
    let document = json(&refused);
    assert_eq!(document["data"]["agents"][0]["skill"]["state"], "foreign");
    assert_eq!(document["data"]["agents"][0]["changed"], false);
    assert_eq!(document["data"]["agents"][0]["skipped"], "foreign");
    assert!(
        document["data"]["hint"]
            .as_str()
            .unwrap()
            .contains("--force")
    );
    assert_eq!(
        document["data"]["agents"][1]["changed"], true,
        "claude still set up"
    );
    assert!(
        fs::read_to_string(theirs.join("SKILL.md"))
            .unwrap()
            .contains("mine")
    );

    let removed = json(&skill(root, &["remove"]));
    assert_eq!(
        states(&removed),
        [
            pair("claude", "current"),
            pair("codex", "foreign"),
            pair("opencode", "foreign")
        ]
    );
    assert!(!root.join(".claude/skills/elsecall").exists());
    assert!(
        theirs.join("SKILL.md").exists(),
        "only what setup wrote goes"
    );

    let forced = skill(root, &["setup", "--agent", "codex", "--force"]);
    assert!(forced.status.success(), "{forced:?}");
    assert!(
        fs::read_to_string(theirs.join("SKILL.md"))
            .unwrap()
            .contains("elsecall skill setup")
    );
}

#[test]
fn setup_with_no_agent_found_says_how_to_name_one_and_install_writes_a_folder() {
    let home = home();
    let none = skill(home.path(), &["setup"]);
    assert_eq!(none.status.code(), Some(1));
    assert_eq!(json(&none)["error"]["code"], "no_agents_found");

    let elsewhere = home.path().join("agent-skills");
    let installed = json(&skill(
        home.path(),
        &["install", "--dir", elsewhere.to_str().unwrap()],
    ));
    assert_eq!(
        installed["data"]["path"],
        elsewhere.join("elsecall").to_str().unwrap()
    );
    assert!(elsewhere.join("elsecall/SKILL.md").exists());
}

#[test]
fn a_linked_skills_folder_is_written_and_removed_once() {
    let home = home();
    let root = home.path();
    fs::create_dir_all(root.join(".claude/skills")).unwrap();
    fs::create_dir(root.join(".codex")).unwrap();
    fs::create_dir(root.join(".agents")).unwrap();
    std::os::unix::fs::symlink(root.join(".claude/skills"), root.join(".agents/skills")).unwrap();

    let setup = skill(root, &["setup"]);
    assert!(setup.status.success(), "{setup:?}");
    assert_eq!(
        states(&json(&skill(root, &["status"]))),
        [
            pair("claude", "current"),
            pair("codex", "current"),
            pair("opencode", "current")
        ]
    );

    let removed = skill(root, &["remove"]);
    assert!(removed.status.success(), "{removed:?}");
    assert!(!root.join(".claude/skills/elsecall").exists());
}

#[test]
fn a_linked_skill_folder_is_never_written_through_or_removed() {
    let home = home();
    let root = home.path();
    let kept = root.join("project/skills/elsecall");
    fs::create_dir_all(&kept).unwrap();
    fs::write(
        kept.join("SKILL.md"),
        "---\nname: elsecall\n---\nkept here\n",
    )
    .unwrap();
    fs::create_dir_all(root.join(".claude/skills")).unwrap();
    std::os::unix::fs::symlink(&kept, root.join(".claude/skills/elsecall")).unwrap();

    let forced = skill(root, &["setup", "--agent", "claude", "--force"]);
    assert_eq!(forced.status.code(), Some(1), "{forced:?}");
    let document = json(&forced);
    assert_eq!(document["data"]["agents"][0]["skill"]["state"], "linked");
    assert_eq!(document["data"]["agents"][0]["skipped"], "linked");
    assert!(
        document["data"]["hint"]
            .as_str()
            .unwrap()
            .contains("Remove the link")
    );

    assert!(skill(root, &["remove"]).status.success());
    assert_eq!(
        fs::read_to_string(kept.join("SKILL.md")).unwrap(),
        "---\nname: elsecall\n---\nkept here\n"
    );
    assert!(root.join(".claude/skills/elsecall").is_symlink());
}

#[test]
fn install_refuses_a_foreign_folder_and_prints_an_absolute_path() {
    let home = home();
    let theirs = home.path().join("taken/elsecall");
    fs::create_dir_all(&theirs).unwrap();
    fs::write(theirs.join("SKILL.md"), "mine\n").unwrap();
    let refused = skill(home.path(), &["install", "--dir", "taken"]);
    assert_eq!(refused.status.code(), Some(1));
    assert_eq!(json(&refused)["error"]["code"], "skill_foreign");
    assert_eq!(
        fs::read_to_string(theirs.join("SKILL.md")).unwrap(),
        "mine\n"
    );

    let installed = json(&skill(home.path(), &["install", "--dir", "fresh"]));
    assert_eq!(
        installed["data"]["path"],
        home.path()
            .canonicalize()
            .unwrap()
            .join("fresh/elsecall")
            .to_str()
            .unwrap()
    );
}
