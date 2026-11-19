#![cfg(target_os = "macos")]
//! `elsecall skill`: the guide this build carries, the stubs `setup` writes into
//! each coding agent's skills folder, the refresh an upgrade does by itself, and
//! what `doctor` reports. Every check runs in a scratch HOME.
mod processes;
use serde_json::Value;
use std::{fs, path::Path, process::Output};

fn home() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("ec-")
        .tempdir_in("/tmp")
        .unwrap()
}

fn elsecall(home: &Path, args: &[&str]) -> Output {
    crate::processes::command(env!("CARGO_BIN_EXE_elsecall"))
        .env("HOME", home)
        .current_dir(home)
        .args(args)
        .output()
        .unwrap()
}

fn skill(home: &Path, args: &[&str]) -> Output {
    let mut all = vec!["skill"];
    all.extend_from_slice(args);
    elsecall(home, &all)
}

fn json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|_| panic!("not JSON: {output:?}"))
}

/// Each agent's (name, value at `field` under its entry), as Elsecall lists them.
fn each(document: &Value, field: &[&str]) -> Vec<(String, String)> {
    document["data"]["agents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| {
            let value = field.iter().fold(a, |v, key| &v[*key]);
            (
                a["agent"].as_str().unwrap().to_owned(),
                value.as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect()
}

fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
    items
        .iter()
        .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
        .collect()
}

fn states(home: &Path) -> Vec<(String, String)> {
    each(&json(&skill(home, &["status"])), &["skill", "state"])
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap()
}

#[test]
fn the_guide_and_its_topics_print_as_text_or_one_json_document() {
    let home = home();
    let guide = skill(home.path(), &["show"]);
    assert!(guide.status.success(), "{guide:?}");
    let text = String::from_utf8(guide.stdout).unwrap();
    assert!(text.starts_with("# Elsecall\n"), "no frontmatter: {text}");
    assert!(text.contains("elsecall skill show NAME"));
    assert!(!text.contains("](references/"));

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
fn setup_installs_a_stub_for_the_agents_found_and_a_second_run_changes_nothing() {
    let home = home();
    let root = home.path();
    fs::create_dir(root.join(".claude")).unwrap();
    fs::create_dir(root.join(".codex")).unwrap();
    assert_eq!(
        states(root),
        pairs(&[
            ("claude", "missing"),
            ("codex", "missing"),
            ("opencode", "missing")
        ])
    );

    let dry = json(&skill(root, &["setup", "--dry-run"]));
    assert_eq!(
        each(&dry, &["result"]),
        pairs(&[("claude", "installed"), ("codex", "installed")])
    );
    assert!(
        !root.join(".claude/skills").exists(),
        "a dry run writes nothing"
    );

    let setup = skill(root, &["setup"]);
    assert!(setup.status.success(), "{setup:?}");
    assert!(
        json(&setup)["data"]["note"]
            .as_str()
            .unwrap()
            .contains("New agent sessions")
    );
    let stub = read(&root.join(".claude/skills/elsecall/SKILL.md"));
    assert!(stub.starts_with("---\nname: elsecall\ndescription: Run builds,"));
    assert!(stub.contains("  elsecall-stub: \"1\"\n"));
    assert!(stub.contains("Run `elsecall skill show`"));
    assert_eq!(read(&root.join(".agents/skills/elsecall/SKILL.md")), stub);
    assert_eq!(
        states(root),
        pairs(&[
            ("claude", "current"),
            ("codex", "current"),
            ("opencode", "current")
        ]),
        "OpenCode reads the folder Codex does"
    );

    let again = json(&skill(root, &["setup"]));
    assert_eq!(
        each(&again, &["result"]),
        pairs(&[("claude", "unchanged"), ("codex", "unchanged")])
    );

    // A whole copy `install` exported, put where an agent reads it, is updated.
    let exported = root.join("exported");
    assert!(
        skill(root, &["install", "--dir", exported.to_str().unwrap()])
            .status
            .success()
    );
    fs::remove_dir_all(root.join(".claude/skills/elsecall")).unwrap();
    fs::rename(
        exported.join("elsecall"),
        root.join(".claude/skills/elsecall"),
    )
    .unwrap();
    assert_eq!(states(root)[0], pairs(&[("claude", "exported")])[0]);
    let replaced = json(&skill(root, &["setup", "--agent", "claude"]));
    assert_eq!(replaced["data"]["agents"][0]["result"], "updated");
    assert!(!root.join(".claude/skills/elsecall/references").exists());
    assert_eq!(read(&root.join(".claude/skills/elsecall/SKILL.md")), stub);
}

#[test]
fn a_skill_elsecall_did_not_write_is_a_conflict_unless_forced_and_never_removed() {
    let home = home();
    let root = home.path();
    let theirs = root.join(".agents/skills/elsecall");
    fs::create_dir_all(&theirs).unwrap();
    fs::write(theirs.join("SKILL.md"), "---\nname: elsecall\n---\nmine\n").unwrap();

    let dry = skill(root, &["setup", "--agent", "codex", "--dry-run"]);
    assert_eq!(dry.status.code(), Some(1), "{dry:?}");
    assert_eq!(json(&dry)["data"]["agents"][0]["result"], "conflict");

    let refused = skill(root, &["setup", "--agent", "codex", "--agent", "claude"]);
    assert_eq!(refused.status.code(), Some(1), "{refused:?}");
    let document = json(&refused);
    assert_eq!(
        each(&document, &["result"]),
        pairs(&[("codex", "conflict"), ("claude", "installed")])
    );
    assert!(
        document["data"]["hint"]
            .as_str()
            .unwrap()
            .contains("--force")
    );
    assert!(read(&theirs.join("SKILL.md")).contains("mine"));

    let dry_remove = json(&skill(root, &["remove", "--dry-run"]));
    assert_eq!(dry_remove["data"]["agents"][0]["result"], "removed");
    assert!(root.join(".claude/skills/elsecall/SKILL.md").exists());
    let removed = json(&skill(root, &["remove"]));
    assert_eq!(
        each(&removed, &["result"]),
        pairs(&[
            ("claude", "removed"),
            ("codex", "unchanged"),
            ("opencode", "unchanged")
        ])
    );
    assert!(!root.join(".claude/skills/elsecall").exists());
    assert!(
        theirs.join("SKILL.md").exists(),
        "only what Elsecall wrote goes"
    );

    let forced = skill(root, &["setup", "--agent", "codex", "--force"]);
    assert!(forced.status.success(), "{forced:?}");
    assert!(read(&theirs.join("SKILL.md")).contains("elsecall-stub"));
}

#[test]
fn links_are_never_written_through_and_a_linked_skills_folder_is_written_once() {
    let home = home();
    let root = home.path();
    let kept = root.join("project/skills/elsecall");
    fs::create_dir_all(&kept).unwrap();
    fs::write(kept.join("SKILL.md"), "kept here\n").unwrap();
    fs::create_dir_all(root.join(".claude/skills")).unwrap();
    std::os::unix::fs::symlink(&kept, root.join(".claude/skills/elsecall")).unwrap();
    let forced = skill(root, &["setup", "--agent", "claude", "--force"]);
    assert_eq!(forced.status.code(), Some(1), "{forced:?}");
    let document = json(&forced);
    assert_eq!(document["data"]["agents"][0]["skill"]["state"], "linked");
    assert_eq!(document["data"]["agents"][0]["result"], "conflict");
    assert!(skill(root, &["remove"]).status.success());
    assert_eq!(read(&kept.join("SKILL.md")), "kept here\n");

    fs::remove_file(root.join(".claude/skills/elsecall")).unwrap();
    fs::create_dir(root.join(".codex")).unwrap();
    fs::create_dir(root.join(".agents")).unwrap();
    std::os::unix::fs::symlink(root.join(".claude/skills"), root.join(".agents/skills")).unwrap();
    assert!(skill(root, &["setup"]).status.success());
    assert_eq!(
        states(root),
        pairs(&[
            ("claude", "current"),
            ("codex", "current"),
            ("opencode", "current")
        ])
    );
    let removed = skill(root, &["remove"]);
    assert!(removed.status.success(), "{removed:?}");
    assert!(!root.join(".claude/skills/elsecall").exists());
}

#[test]
fn an_upgrade_refreshes_older_stubs_and_installs_nothing_where_nothing_was() {
    let home = home();
    let root = home.path();
    fs::create_dir(root.join(".claude")).unwrap();
    fs::create_dir(root.join(".codex")).unwrap();
    assert!(skill(root, &["setup"]).status.success());
    let current = read(&root.join(".claude/skills/elsecall/SKILL.md"));
    let older = current
        .replace("elsecall-stub: \"1\"", "elsecall-stub: \"0\"")
        .replace("never retry", "do not retry");
    fs::write(root.join(".claude/skills/elsecall/SKILL.md"), &older).unwrap();
    let newer = older.replace("elsecall-stub: \"0\"", "elsecall-stub: \"9\"");
    fs::write(root.join(".agents/skills/elsecall/SKILL.md"), &newer).unwrap();
    assert_eq!(states(root)[0], pairs(&[("claude", "outdated")])[0]);

    assert_eq!(elsecall::agent_skill::refresh(root), 1);
    assert_eq!(
        read(&root.join(".claude/skills/elsecall/SKILL.md")),
        current
    );
    assert_eq!(
        read(&root.join(".agents/skills/elsecall/SKILL.md")),
        newer,
        "a newer build's stub stays"
    );

    let empty = self::home();
    fs::create_dir(empty.path().join(".claude")).unwrap();
    assert_eq!(elsecall::agent_skill::refresh(empty.path()), 0);
    assert!(!empty.path().join(".claude/skills").exists());

    // A build that is not the installed one never refreshes.
    fs::write(root.join(".claude/skills/elsecall/SKILL.md"), &older).unwrap();
    let paths = elsecall::paths::Paths::for_home(root).unwrap();
    assert_eq!(elsecall::agent_skill::refresh_if_installed(&paths), 0);
    assert_eq!(read(&root.join(".claude/skills/elsecall/SKILL.md")), older);
}

/// Every file under `dir`, with its contents, to compare before and after.
fn snapshot(dir: &Path) -> Vec<(String, Vec<u8>)> {
    let mut files = Vec::new();
    let mut pending = vec![dir.to_owned()];
    while let Some(next) = pending.pop() {
        for entry in fs::read_dir(&next).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path);
            } else {
                files.push((path.display().to_string(), fs::read(&path).unwrap()));
            }
        }
    }
    files.sort();
    files
}

#[test]
fn doctor_warns_of_an_agent_without_the_skill_and_nothing_but_setup_writes_there() {
    let home = home();
    let root = home.path();
    fs::create_dir_all(root.join(".claude")).unwrap();
    fs::create_dir_all(root.join(".codex")).unwrap();
    fs::write(root.join(".claude/settings.json"), "{}").unwrap();
    let before = (
        snapshot(&root.join(".claude")),
        snapshot(&root.join(".codex")),
    );

    let doctor = json(&elsecall(root, &["doctor"]));
    let warning = doctor["data"]["agent_skill"]["warning"].as_str().unwrap();
    assert!(warning.contains("Claude Code, Codex"), "{warning}");
    assert!(warning.contains("elsecall skill setup"));
    for args in [&["skill", "status"][..], &["skill", "show"], &["machine"]] {
        elsecall(root, args);
    }
    assert_eq!(
        (
            snapshot(&root.join(".claude")),
            snapshot(&root.join(".codex"))
        ),
        before,
        "only setup, remove, install and refresh write agent files"
    );

    assert!(skill(root, &["setup"]).status.success());
    let doctor = json(&elsecall(root, &["doctor"]));
    assert!(
        doctor["data"]["agent_skill"]["warning"].is_null(),
        "{doctor}"
    );
}

#[test]
fn install_writes_the_whole_guide_and_refuses_a_foreign_folder() {
    let home = home();
    let none = skill(home.path(), &["setup"]);
    assert_eq!(none.status.code(), Some(1));
    assert_eq!(json(&none)["error"]["code"], "no_agents_found");

    let installed = json(&skill(home.path(), &["install", "--dir", "fresh"]));
    let folder = home.path().canonicalize().unwrap().join("fresh/elsecall");
    assert_eq!(installed["data"]["path"], folder.to_str().unwrap());
    assert!(
        installed["data"]["note"]
            .as_str()
            .unwrap()
            .contains("again after upgrading")
    );
    assert!(read(&folder.join("SKILL.md")).starts_with("---\nname: elsecall\n"));
    assert!(read(&folder.join("SKILL.md")).contains("# Elsecall\n"));
    assert!(read(&folder.join("references/integrate.md")).starts_with("# Integrating"));

    let theirs = home.path().join("taken/elsecall");
    fs::create_dir_all(&theirs).unwrap();
    fs::write(theirs.join("SKILL.md"), "mine\n").unwrap();
    let refused = skill(home.path(), &["install", "--dir", "taken"]);
    assert_eq!(refused.status.code(), Some(1));
    assert_eq!(json(&refused)["error"]["code"], "skill_foreign");
    assert_eq!(read(&theirs.join("SKILL.md")), "mine\n");
}

fn older_stub(current: &str) -> String {
    current
        .replace("elsecall-stub: \"1\"", "elsecall-stub: \"0\"")
        .replace("never retry", "do not retry")
}

#[test]
fn refresh_leaves_exported_copies_and_other_skills_alone() {
    let home = home();
    let root = home.path();
    fs::create_dir(root.join(".claude")).unwrap();
    assert!(skill(root, &["setup"]).status.success());
    let current = read(&root.join(".claude/skills/elsecall/SKILL.md"));

    let exported = root.join("copy");
    let dir = exported.to_str().unwrap();
    assert!(skill(root, &["install", "--dir", dir]).status.success());
    fs::create_dir_all(root.join(".agents/skills")).unwrap();
    fs::rename(
        exported.join("elsecall"),
        root.join(".agents/skills/elsecall"),
    )
    .unwrap();
    let copy = read(&root.join(".agents/skills/elsecall/SKILL.md"));
    fs::write(
        root.join(".claude/skills/elsecall/SKILL.md"),
        older_stub(&current),
    )
    .unwrap();

    assert_eq!(elsecall::agent_skill::refresh(root), 1);
    assert_eq!(
        read(&root.join(".claude/skills/elsecall/SKILL.md")),
        current
    );
    assert_eq!(read(&root.join(".agents/skills/elsecall/SKILL.md")), copy);

    fs::write(
        root.join(".claude/skills/elsecall/SKILL.md"),
        "---\nname: elsecall\n---\nmine\n",
    )
    .unwrap();
    assert_eq!(elsecall::agent_skill::refresh(root), 0);
    assert!(read(&root.join(".claude/skills/elsecall/SKILL.md")).contains("mine"));
}

#[test]
fn the_installed_service_refreshes_an_older_stub_when_it_starts() {
    let home = home();
    let root = home.path();
    fs::create_dir(root.join(".claude")).unwrap();
    assert!(skill(root, &["setup"]).status.success());
    let stub = root.join(".claude/skills/elsecall/SKILL.md");
    let current = read(&stub);
    fs::write(&stub, older_stub(&current)).unwrap();

    // The binary where `elsecall install` puts it, as the LaunchAgent runs it.
    let paths = elsecall::paths::Paths::for_home(root).unwrap();
    elsecall::private_files::directory(&paths.data).unwrap();
    fs::copy(env!("CARGO_BIN_EXE_elsecall"), paths.executable()).unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut service = crate::processes::command(paths.executable())
        .env("HOME", root)
        .env("ELSECALL_FLEET_PORT", port.to_string())
        .env("ELSECALL_TAILNET_NAME", "skill-refresh")
        .env("ELSECALL_TAILNET_ADDRESSES", "127.0.0.1")
        .env("ELSECALL_MACHINE_LOAD", "idle")
        .env("ELSECALL_MACHINE_SPEED", "1")
        .arg("serve")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + crate::processes::WAIT;
    while read(&stub) != current && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let _ = service.kill();
    let _ = service.wait();
    assert_eq!(
        read(&stub),
        current,
        "the service brought the stub up to date"
    );
}

#[test]
fn the_default_copy_keeps_the_data_folder_private_and_uninstall_removes_everything() {
    use std::os::unix::fs::PermissionsExt;
    let home = home();
    let root = home.path();
    fs::create_dir(root.join(".claude")).unwrap();
    assert!(skill(root, &["setup"]).status.success());
    let installed = json(&skill(root, &["install"]));
    let paths = elsecall::paths::Paths::for_home(root).unwrap();
    let copy = paths.data.join("skill/elsecall");
    assert_eq!(
        installed["data"]["path"],
        copy.to_str().unwrap(),
        "{installed}"
    );
    assert_eq!(
        fs::metadata(&paths.data).unwrap().permissions().mode() & 0o777,
        0o700,
        "elsecall install refuses a data folder others can read"
    );

    let (removed, complete) = elsecall::agent_skill::remove_all(&paths);
    assert!(complete, "{removed}");
    assert!(!root.join(".claude/skills/elsecall").exists());
    assert!(!paths.data.join("skill").exists());
}
