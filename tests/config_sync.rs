//! Configuration sync between real member services in separate temporary homes,
//! with loopback standing in for the tailnet. Tests that need a misbehaving master
//! act as one with a member's own key, as only that member's account could.
mod members;
mod processes;

use elsecall::{
    config_sync::{
        manifest::{FileNode, Manifest, Node, Tree},
        protocol::{self, Message, Purpose},
        state::State,
    },
    paths::Paths,
    peer_client,
    workspace::path::WorkspacePath,
};
use members::{TestMember, enrol};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    thread::sleep,
    time::{Duration, Instant},
};

/// A value that must never appear outside the synced files and the replica store.
const SENTINEL: &str = "s3ntinel-VALUE-7f1c90";
const DECLARED: &str = "[sync]\npaths = [\"~/.config/demo\"]\n";

fn file(member: &TestMember, relative: &str) -> PathBuf {
    member.home.path().join(relative)
}

fn write(member: &TestMember, relative: &str, content: &str) {
    let path = file(member, relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn read(member: &TestMember, relative: &str) -> Option<String> {
    std::fs::read_to_string(file(member, relative)).ok()
}

fn mode(member: &TestMember, relative: &str) -> u32 {
    std::fs::symlink_metadata(file(member, relative))
        .unwrap()
        .permissions()
        .mode()
        & 0o777
}

fn chmod(member: &TestMember, relative: &str, mode: u32) {
    std::fs::set_permissions(
        file(member, relative),
        std::fs::Permissions::from_mode(mode),
    )
    .unwrap();
}

fn wait_until(what: &str, condition: impl Fn() -> bool) {
    let deadline = Instant::now() + crate::processes::WAIT;
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting until {what}");
        sleep(Duration::from_millis(100));
    }
}

/// Asks the master, from `member`, to push now; sync runs on such events only.
fn push(member: &TestMember) -> Value {
    let round = member.ok(&["sync", "push"]);
    assert_eq!(round["complete"], true, "{round}");
    round
}

/// Starts alpha (master, declaring ~/.config/demo) and beta, enrolled together.
fn pair() -> (TestMember, TestMember) {
    let alpha = TestMember::start_with("alpha", DECLARED, &[]);
    let beta = TestMember::start_with("beta", DECLARED, &[]);
    enrol(&alpha, &beta);
    (alpha, beta)
}

/// The master's files: a secret, a nested token and an executable script.
fn master_files(alpha: &TestMember) {
    write(
        alpha,
        ".config/demo/secret.json",
        &format!("{{\"token\":\"{SENTINEL}\"}}"),
    );
    write(alpha, ".config/demo/sub/token", "alpha-token");
    write(alpha, ".config/demo/run.sh", "#!/bin/sh\necho alpha\n");
    chmod(alpha, ".config/demo/run.sh", 0o755);
}

/// Classes by file for `member` in a plan.
fn classes(plan: &Value, member: &str) -> BTreeMap<String, (String, bool)> {
    let entry = plan["members"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["member"] == member)
        .unwrap_or_else(|| panic!("{member} not in plan: {plan}"));
    entry["paths"][0]["files"]
        .as_array()
        .unwrap_or_else(|| panic!("no files for {member}: {plan}"))
        .iter()
        .map(|f| {
            (
                f["file"].as_str().unwrap().to_owned(),
                (
                    f["change"].as_str().unwrap().to_owned(),
                    f["backup"].as_bool().unwrap(),
                ),
            )
        })
        .collect()
}

/// Enables ~/.config/demo on alpha through plan-then-confirm.
fn enable(alpha: &TestMember) -> Value {
    let planned = alpha.ok(&["sync", "enable", "~/.config/demo"]);
    let id = planned["plan"]["plan_id"].as_str().unwrap().to_owned();
    alpha.ok(&["sync", "enable", "~/.config/demo", "--confirm", &id])
}

fn status_of(member: &TestMember, of: &str) -> Value {
    member.ok(&["sync", "status"])["members"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["member"] == of)
        .cloned()
        .unwrap()
}

fn sync_dir(member: &TestMember) -> PathBuf {
    Paths::for_home(member.home.path())
        .unwrap()
        .data
        .join("config-sync")
}

fn manifest_of(member: &TestMember) -> Manifest {
    State::load(&sync_dir(member).join("state.json"))
        .unwrap()
        .manifest
        .unwrap()
}

#[test]
fn plan_enable_push_overrides_and_local_edits() {
    let (alpha, beta) = pair();
    master_files(&alpha);
    // Beta already has its own copy, with a value of its own and an extra file.
    write(
        &beta,
        ".config/demo/secret.json",
        "{\"token\":\"beta-own\"}",
    );
    write(&beta, ".config/demo/local-only.txt", "beta only");
    // ... and a link sync cannot carry.
    std::os::unix::fs::symlink("/etc/hosts", file(&beta, ".config/demo/hosts")).unwrap();

    // The plan, per member and per file, by fingerprints only.
    let plan = alpha.ok(&["sync", "plan"]);
    assert_eq!(plan["paths"][0]["path"], "~/.config/demo");
    assert_eq!(plan["paths"][0]["declared"], true);
    assert_eq!(plan["paths"][0]["enabled"], false);
    assert_eq!(plan["paths"][0]["files"], 3);
    let planned = classes(&plan, "beta");
    assert_eq!(
        planned,
        BTreeMap::from([
            (
                "~/.config/demo/local-only.txt".into(),
                ("removed".into(), true)
            ),
            ("~/.config/demo/run.sh".into(), ("added".into(), false)),
            (
                "~/.config/demo/secret.json".into(),
                ("changed".into(), true)
            ),
            ("~/.config/demo/sub/token".into(), ("added".into(), false)),
        ]),
        "{plan}"
    );
    assert!(plan["next_step"].as_str().unwrap().contains("--confirm"));
    assert_eq!(plan["members"][0]["paths"][0]["skipped"][0], "hosts");
    // Planning changed nothing.
    assert_eq!(
        read(&beta, ".config/demo/secret.json").unwrap(),
        "{\"token\":\"beta-own\"}"
    );
    // Only the master plans; paths outside the rules are refused before anything.
    assert_eq!(beta.fails(&["sync", "plan"]), "not_master");
    let refused = alpha.run(&["sync", "plan", "~/.ssh"]);
    assert_eq!(refused.status.code(), Some(2));
    assert_eq!(
        alpha.fails(&["sync", "plan", "~/.config/other"]),
        "not_declared"
    );

    // Enabling needs the plan's ID; a wrong one changes nothing.
    let shown = alpha.ok(&["sync", "enable", "~/.config/demo"]);
    assert_eq!(shown["status"], "planned");
    assert_eq!(
        alpha.fails(&[
            "sync",
            "enable",
            "~/.config/demo",
            "--confirm",
            "0000000000000000"
        ]),
        "plan_changed"
    );
    assert!(read(&beta, ".config/demo/run.sh").is_none());
    let enabled = enable(&alpha);
    assert_eq!(enabled["status"], "enabled", "{enabled}");
    assert_eq!(enabled["round"]["complete"], true, "{enabled}");

    // Beta now holds an exact, owner-only replica; its own files were backed up.
    for relative in [
        ".config/demo/secret.json",
        ".config/demo/sub/token",
        ".config/demo/run.sh",
    ] {
        assert_eq!(read(&beta, relative), read(&alpha, relative), "{relative}");
    }
    assert!(read(&beta, ".config/demo/local-only.txt").is_none());
    assert_eq!(mode(&beta, ".config/demo"), 0o700);
    assert_eq!(mode(&beta, ".config/demo/sub"), 0o700);
    assert_eq!(mode(&beta, ".config/demo/secret.json"), 0o600);
    assert_eq!(mode(&beta, ".config/demo/run.sh"), 0o700);
    let beta_status = status_of(&alpha, "beta");
    assert_eq!(beta_status["state"], "in_sync", "{beta_status}");
    let backups = beta_status["sync"]["backups"].as_array().unwrap();
    assert_eq!(backups.len(), 3, "{beta_status}");
    for backup in backups.iter().filter(|b| b["fingerprint"].is_string()) {
        let location = backup["location"].as_str().unwrap();
        assert!(location.contains("config-sync/backups"), "{location}");
        assert_eq!(
            std::fs::metadata(location).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    // The whole previous tree, link included, is kept since sync cannot carry it.
    let previous = backups
        .iter()
        .find(|b| b["file"] == "~/.config/demo")
        .unwrap_or_else(|| panic!("no kept tree: {beta_status}"));
    let kept = PathBuf::from(previous["location"].as_str().unwrap());
    assert!(
        std::fs::symlink_metadata(kept.join("hosts"))
            .unwrap()
            .is_symlink()
    );
    assert!(std::fs::symlink_metadata(file(&beta, ".config/demo/hosts")).is_err());
    let secret_backup = backups
        .iter()
        .find(|b| b["file"] == "~/.config/demo/secret.json")
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(secret_backup["location"].as_str().unwrap()).unwrap(),
        "{\"token\":\"beta-own\"}"
    );
    assert_eq!(
        alpha.view_of("beta")["config_sync"],
        "in_sync",
        "the fleet view shows each member's sync state"
    );
    assert_eq!(alpha.view_of("alpha")["config_sync"], "source");

    // A change on the master is pushed: content, deletions and permissions.
    write(
        &alpha,
        ".config/demo/secret.json",
        &format!("{{\"token\":\"{SENTINEL}-2\"}}"),
    );
    std::fs::remove_file(file(&alpha, ".config/demo/sub/token")).unwrap();
    chmod(&alpha, ".config/demo/run.sh", 0o644);
    // Nothing runs in the background: the change waits for a sync event.
    sleep(Duration::from_secs(2));
    assert_eq!(
        read(&beta, ".config/demo/sub/token").as_deref(),
        Some("alpha-token")
    );
    let round = push(&alpha);
    assert_eq!(round["deliveries"][0]["outcome"], "applied", "{round}");
    assert_eq!(
        read(&beta, ".config/demo/secret.json"),
        read(&alpha, ".config/demo/secret.json")
    );
    assert!(read(&beta, ".config/demo/sub/token").is_none());
    assert_eq!(mode(&beta, ".config/demo/run.sh"), 0o600);

    // Beta's override wins at the declared path and survives pushes.
    let overrides = file(&beta, ".elsecall/sync-overrides");
    assert_eq!(
        std::fs::metadata(&overrides).unwrap().permissions().mode() & 0o777,
        0o700,
        "members create their overrides directory owner-only"
    );
    write(
        &beta,
        ".elsecall/sync-overrides/.config/demo/secret.json",
        "{\"token\":\"beta-override\"}",
    );
    // Syncing one member, as a placed run will: the override not yet laid is a
    // difference, so the member is pushed to and the override applied.
    let one = alpha.ok(&["sync", "push", "--member", "beta"]);
    assert_eq!(one["deliveries"][0]["member"], "beta", "{one}");
    assert_eq!(
        read(&beta, ".config/demo/secret.json").as_deref(),
        Some("{\"token\":\"beta-override\"}")
    );
    // Already in sync: only its generation is asked, nothing is pushed.
    let again = alpha.ok(&["sync", "push", "--member", "beta"]);
    assert!(
        again["deliveries"].as_array().unwrap().is_empty(),
        "{again}"
    );
    write(
        &alpha,
        ".config/demo/secret.json",
        &format!("{{\"token\":\"{SENTINEL}-3\"}}"),
    );
    write(&alpha, ".config/demo/new.txt", "added later");
    push(&alpha);
    assert!(read(&beta, ".config/demo/new.txt").is_some());
    assert_eq!(
        read(&beta, ".config/demo/secret.json").as_deref(),
        Some("{\"token\":\"beta-override\"}"),
        "an override is never overwritten by sync"
    );
    let beta_status = status_of(&beta, "beta");
    assert_eq!(beta_status["state"], "in_sync", "{beta_status}");
    assert_eq!(
        beta_status["sync"]["paths"][0]["overridden"][0],
        "~/.config/demo/secret.json"
    );

    // A local edit is reported, then backed up once and replaced at the next sync.
    write(
        &beta,
        ".config/demo/run.sh",
        "#!/bin/sh\necho edited on beta\n",
    );
    let edited = status_of(&alpha, "beta");
    assert_eq!(edited["state"], "locally_edited", "{edited}");
    assert_eq!(
        edited["sync"]["paths"][0]["differences"][0]["change"],
        "locally_edited"
    );
    let pushed = beta.ok(&["sync", "push"]);
    assert_eq!(pushed["complete"], true, "{pushed}");
    assert_eq!(
        read(&beta, ".config/demo/run.sh"),
        read(&alpha, ".config/demo/run.sh")
    );
    let after = status_of(&alpha, "beta");
    assert_eq!(after["state"], "in_sync", "{after}");
    assert_eq!(after["sync"]["backups"].as_array().unwrap().len(), 4);
    alpha.ok(&["sync", "push"]);
    assert_eq!(
        status_of(&alpha, "beta")["sync"]["backups"]
            .as_array()
            .unwrap()
            .len(),
        4,
        "nothing more to back up"
    );

    // No value appears in logs, records, status, plans, the fleet view or runs.
    let local = alpha.ok(&["runs"]);
    let mut outputs = vec![
        alpha.fleet().to_string(),
        beta.fleet().to_string(),
        alpha.ok(&["sync", "status"]).to_string(),
        beta.ok(&["sync", "status"]).to_string(),
        alpha.ok(&["sync", "plan"]).to_string(),
        alpha.ok(&["sync", "history", "--limit", "100"]).to_string(),
        beta.ok(&["sync", "history", "--limit", "100"]).to_string(),
        local.to_string(),
        alpha.log(),
        beta.log(),
    ];
    for member in [&alpha, &beta] {
        for name in ["state.json", "history.jsonl"] {
            outputs.push(std::fs::read_to_string(sync_dir(member).join(name)).unwrap());
        }
    }
    let run = alpha.run(&["run", "--", "/usr/bin/true"]);
    assert!(run.status.success());
    let runs = alpha.ok(&["runs"]);
    if let Some(id) = runs["runs"][0]["run_id"].as_str() {
        outputs.push(alpha.ok(&["explain", id]).to_string());
    }
    for output in outputs {
        assert!(
            !output.contains(SENTINEL),
            "a synced value leaked: {output}"
        );
        assert!(
            !output.contains("beta-override"),
            "an override leaked: {output}"
        );
    }
}

/// Opens a sync channel to `to` as `from`, which only `from`'s account could.
async fn channel(
    from: &TestMember,
    to: &TestMember,
) -> elsecall::failure::Result<peer_client::RunChannel> {
    peer_client::channel(
        &from.key(),
        &to.as_member(),
        protocol::ROUTE,
        protocol::PURPOSE,
        protocol::PROTOCOL,
    )
    .await
}

/// The manifest with `content` at `secret.json`.
fn changed(manifest: &Manifest, content: &[u8]) -> Manifest {
    let mut next = manifest.clone();
    next.generation += 1;
    let tree = next.paths.values_mut().next().unwrap();
    let Tree::Directory(entries) = tree else {
        panic!("a directory")
    };
    entries.insert(
        WorkspacePath::parse("secret.json").unwrap(),
        Node::File(FileNode {
            mode: 0o600,
            size: content.len() as u64,
            fingerprint: manifest.key.fingerprint(content),
        }),
    );
    next
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupted_pushes_change_nothing_and_only_the_master_pushes() {
    let (alpha, beta) = pair();
    master_files(&alpha);
    enable(&alpha);
    let before = read(&beta, ".config/demo/secret.json").unwrap();
    let held = beta.ok(&["sync", "status"])["members"].clone();
    let manifest = manifest_of(&alpha);
    let content = b"{\"token\":\"never applied\"}";
    let next = changed(&manifest, content);

    // Cut off in the middle of the content.
    let mut sealed = channel(&alpha, &beta).await.unwrap();
    protocol::send(
        &mut sealed,
        &Message::Offer {
            purpose: Purpose::Push,
            manifest: next.clone(),
        },
    )
    .await
    .unwrap();
    let Message::Need { fingerprints } = protocol::receive(&mut sealed).await.unwrap() else {
        panic!("expected need")
    };
    assert_eq!(fingerprints.len(), 1, "only the content beta lacks");
    elsecall::wire::write(
        &mut sealed,
        &elsecall::wire::Frame::Content(content[..5].to_vec()),
    )
    .await
    .unwrap();
    drop(sealed);

    // All content, but no commit.
    let mut sealed = channel(&alpha, &beta).await.unwrap();
    protocol::send(
        &mut sealed,
        &Message::Offer {
            purpose: Purpose::Push,
            manifest: next.clone(),
        },
    )
    .await
    .unwrap();
    let _ = protocol::receive(&mut sealed).await.unwrap();
    protocol::send_blob(&mut sealed, content).await.unwrap();
    drop(sealed);
    sleep(Duration::from_millis(500));
    assert_eq!(read(&beta, ".config/demo/secret.json").unwrap(), before);
    let still = beta.ok(&["sync", "status"])["members"].clone();
    let id = |members: &Value| {
        members
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["member"] == "beta")
            .unwrap()["sync"]["manifest_id"]
            .clone()
    };
    assert_eq!(
        id(&still),
        id(&held),
        "beta keeps its previous complete state"
    );

    // Content that does not match its fingerprint is refused.
    let other = b"{\"token\":\"other content\"}";
    let mut sealed = channel(&alpha, &beta).await.unwrap();
    protocol::send(
        &mut sealed,
        &Message::Offer {
            purpose: Purpose::Push,
            manifest: changed(&manifest, other),
        },
    )
    .await
    .unwrap();
    let _ = protocol::receive(&mut sealed).await.unwrap();
    let mut tampered = other.to_vec();
    tampered[12] ^= 1;
    protocol::send_blob(&mut sealed, &tampered).await.unwrap();
    match protocol::receive(&mut sealed).await {
        Ok(Message::Failed { failure }) => assert_eq!(failure.code, "sync_content_mismatch"),
        other => panic!("expected a refusal: {other:?}"),
    }
    assert_eq!(read(&beta, ".config/demo/secret.json").unwrap(), before);

    // A member that is not master is refused before anything is offered.
    let refused = channel(&beta, &alpha).await.err().expect("refused");
    assert_eq!(refused.code, "not_master");

    // An offer naming a forbidden path is invalid input, whoever sends it.
    let mut sealed = channel(&alpha, &beta).await.unwrap();
    let mut raw = serde_json::to_value(Message::Offer {
        purpose: Purpose::Push,
        manifest: next.clone(),
    })
    .unwrap();
    let tree = raw["manifest"]["paths"]
        .as_object_mut()
        .unwrap()
        .remove("~/.config/demo")
        .unwrap();
    raw["manifest"]["paths"]["~/.ssh"] = tree;
    protocol::send_blob(&mut sealed, &serde_json::to_vec(&raw).unwrap())
        .await
        .unwrap();
    match protocol::receive(&mut sealed).await {
        Ok(Message::Failed { failure }) => assert_eq!(failure.code, "protocol_error"),
        other => panic!("expected a refusal: {other:?}"),
    }

    // The complete exchange applies.
    let mut sealed = channel(&alpha, &beta).await.unwrap();
    protocol::send(
        &mut sealed,
        &Message::Offer {
            purpose: Purpose::Push,
            manifest: next.clone(),
        },
    )
    .await
    .unwrap();
    let Message::Need { fingerprints } = protocol::receive(&mut sealed).await.unwrap() else {
        panic!("expected need")
    };
    assert!(
        fingerprints.is_empty(),
        "kept from the uncommitted exchange"
    );
    protocol::send(&mut sealed, &Message::Commit).await.unwrap();
    assert!(matches!(
        protocol::receive(&mut sealed).await.unwrap(),
        Message::Applied { .. }
    ));
    assert_eq!(
        read(&beta, ".config/demo/secret.json").unwrap().as_bytes(),
        content
    );
    // ... and the real master puts its own state back when it next syncs beta.
    let restored = alpha.ok(&["sync", "push", "--member", "beta"]);
    assert_eq!(
        restored["deliveries"][0]["outcome"], "applied",
        "{restored}"
    );
    assert_eq!(
        read(&beta, ".config/demo/secret.json"),
        read(&alpha, ".config/demo/secret.json")
    );
}

#[test]
fn moving_the_master_pushes_final_state_and_keeps_overrides_local() {
    let (alpha, beta) = pair();
    master_files(&alpha);
    enable(&alpha);
    write(
        &beta,
        ".elsecall/sync-overrides/.config/demo/secret.json",
        "{\"token\":\"beta-override\"}",
    );
    push(&alpha);
    assert_eq!(
        read(&beta, ".config/demo/secret.json").as_deref(),
        Some("{\"token\":\"beta-override\"}")
    );
    write(&beta, ".config/demo/run.sh", "beta drift before the move");
    // A change right before the move is pushed by the move itself.
    write(
        &alpha,
        ".config/demo/sub/token",
        "last change before the move",
    );
    let moved = alpha.ok(&["fleet", "move-master", "beta"]);
    assert_eq!(moved["config_sync"]["status"], "in_sync", "{moved}");
    assert_eq!(
        read(&beta, ".config/demo/sub/token").as_deref(),
        Some("last change before the move")
    );
    let id = moved["config_sync"]["manifest_id"].clone();
    // Beta's local edit from before the move never becomes the fleet's truth: the
    // final push replaced it, backed up.
    assert_eq!(
        read(&beta, ".config/demo/run.sh"),
        read(&alpha, ".config/demo/run.sh")
    );
    let backups = status_of(&beta, "beta")["sync"]["backups"].clone();
    let drift = backups
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["file"] == "~/.config/demo/run.sh")
        .unwrap_or_else(|| panic!("beta's drift was not backed up: {backups}"));
    assert_eq!(
        std::fs::read_to_string(drift["location"].as_str().unwrap()).unwrap(),
        "beta drift before the move"
    );

    // Beta took over the moment the role arrived; its replica, without its
    // override, is the truth, so nothing changed on alpha.
    wait_until("beta takes over as master", || {
        beta.log().contains("\"config_sync_role_changed\"")
    });
    let status = alpha.ok(&["sync", "status"]);
    assert_eq!(status["master"], "beta");
    assert_eq!(status["manifest_id"], id, "{status}");
    assert_eq!(status_of(&alpha, "alpha")["state"], "in_sync");
    assert!(
        read(&alpha, ".config/demo/secret.json")
            .unwrap()
            .contains(SENTINEL)
    );

    // An edit on the new master right after the move, before any sync command,
    // is its own: the next push sends it, never reverts it.
    write(&beta, ".config/demo/run.sh", "#!/bin/sh\necho from beta\n");
    push(&alpha); // asks the new master
    assert_eq!(
        read(&alpha, ".config/demo/run.sh").as_deref(),
        Some("#!/bin/sh\necho from beta\n")
    );
    assert_eq!(alpha.fails(&["sync", "plan"]), "not_master");
    assert!(
        read(&alpha, ".config/demo/secret.json")
            .unwrap()
            .contains(SENTINEL)
    );

    // Moving back, run on the member that is not master.
    write(&beta, ".config/demo/sub/token", "beta's final change");
    let back = alpha.ok(&["fleet", "move-master", "alpha"]);
    assert_eq!(back["config_sync"]["status"], "in_sync", "{back}");
    assert_eq!(
        read(&alpha, ".config/demo/sub/token").as_deref(),
        Some("beta's final change")
    );
    assert_eq!(
        read(&beta, ".config/demo/secret.json").as_deref(),
        Some("{\"token\":\"beta-override\"}")
    );
    assert!(
        read(&alpha, ".config/demo/secret.json")
            .unwrap()
            .contains(SENTINEL)
    );
    assert_eq!(status_of(&alpha, "beta")["state"], "in_sync");
}

#[test]
fn a_forced_move_makes_the_new_masters_replica_the_truth() {
    let (mut alpha, beta) = pair();
    master_files(&alpha);
    enable(&alpha);
    // Alpha changes a file and goes down before it can push.
    write(&alpha, ".config/demo/sub/token", "never pushed");
    alpha.stop();
    assert_eq!(
        beta.fails(&["fleet", "move-master", "beta"]),
        "member_unavailable"
    );
    // Alpha missed the change, so the forced move exits 1 like any incomplete one.
    let forced = beta.run(&["fleet", "move-master", "beta", "--force"]);
    assert_eq!(forced.status.code(), Some(1));
    let moved = members::json(&forced)["data"].clone();
    assert_eq!(moved["master"], "beta");
    assert_eq!(
        moved["config_sync"]["status"], "old_master_unreachable",
        "{moved}"
    );
    assert!(
        moved["config_sync"]["warning"]
            .as_str()
            .unwrap()
            .contains("lost to the fleet")
    );
    assert_eq!(
        read(&beta, ".config/demo/sub/token").as_deref(),
        Some("alpha-token")
    );

    // Alpha returns; once it records beta as master, its unpushed change is backed
    // up and replaced at the next sync, never silently.
    alpha.restart();
    let synced = beta.run(&["fleet", "sync"]);
    assert!(synced.status.success(), "{synced:?}");
    wait_until("alpha records beta as master", || {
        alpha.ok(&["sync", "status"])["master"] == "beta"
    });
    beta.ok(&["sync", "push"]);
    assert_eq!(
        read(&alpha, ".config/demo/sub/token").as_deref(),
        Some("alpha-token")
    );
    let backups = status_of(&beta, "alpha")["sync"]["backups"].clone();
    let kept = backups
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["file"] == "~/.config/demo/sub/token")
        .unwrap_or_else(|| panic!("no backup: {backups}"));
    assert_eq!(
        std::fs::read_to_string(kept["location"].as_str().unwrap()).unwrap(),
        "never pushed"
    );
}

#[test]
fn a_removed_member_keeps_its_files_and_gets_no_more_pushes() {
    let (alpha, beta) = pair();
    master_files(&alpha);
    enable(&alpha);
    alpha.ok(&["fleet", "remove", "beta"]);
    write(&alpha, ".config/demo/sub/token", "after removal");
    let round = push(&alpha);
    assert!(
        round["deliveries"].as_array().unwrap().is_empty(),
        "{round}"
    );
    assert_eq!(
        read(&beta, ".config/demo/sub/token").as_deref(),
        Some("alpha-token")
    );
    beta.ok(&["fleet", "leave"]);
    assert_eq!(
        read(&beta, ".config/demo/sub/token").as_deref(),
        Some("alpha-token")
    );
    assert!(
        read(&beta, ".config/demo/secret.json")
            .unwrap()
            .contains(SENTINEL)
    );
}

#[test]
fn disabling_stops_sync_and_leaves_files_in_place() {
    let (alpha, beta) = pair();
    master_files(&alpha);
    enable(&alpha);
    let shown = alpha.ok(&["sync", "disable", "~/.config/demo"]);
    assert_eq!(shown["status"], "planned");
    let id = shown["plan_id"].as_str().unwrap().to_owned();
    let disabled = alpha.ok(&["sync", "disable", "~/.config/demo", "--confirm", &id]);
    assert_eq!(disabled["status"], "disabled", "{disabled}");
    write(&alpha, ".config/demo/sub/token", "after disabling");
    push(&alpha);
    assert_eq!(
        read(&beta, ".config/demo/sub/token").as_deref(),
        Some("alpha-token")
    );
    assert!(
        read(&beta, ".config/demo/secret.json")
            .unwrap()
            .contains(SENTINEL)
    );
    let beta_sync = status_of(&alpha, "beta");
    assert!(
        beta_sync["sync"]["paths"].as_array().unwrap().is_empty(),
        "{beta_sync}"
    );
    // Replicas of the path are dropped from the store.
    let objects: Vec<_> = std::fs::read_dir(sync_dir(&beta).join("objects"))
        .unwrap()
        .collect();
    assert!(objects.is_empty());
}

#[test]
fn members_refuse_paths_that_resolve_to_forbidden_places() {
    let (alpha, beta) = pair();
    master_files(&alpha);
    // On beta, ~/.config is a link into ~/.ssh.
    std::fs::create_dir_all(file(&beta, ".ssh")).unwrap();
    std::os::unix::fs::symlink(file(&beta, ".ssh"), file(&beta, ".config")).unwrap();
    let plan = alpha.ok(&["sync", "plan"]);
    let beta_plan = plan["members"][0]["paths"][0].clone();
    assert_eq!(beta_plan["error"]["code"], "unsafe_sync_path", "{plan}");
    let id = plan["plan_id"].as_str().unwrap();
    let incomplete = alpha.run(&["sync", "enable", "~/.config/demo", "--confirm", id]);
    assert_eq!(
        incomplete.status.code(),
        Some(1),
        "a failed delivery exits 1"
    );
    let enabled = members::json(&incomplete)["data"].clone();
    assert_eq!(enabled["status"], "enabled", "{enabled}");
    assert_eq!(enabled["round"]["complete"], false, "{enabled}");
    assert_eq!(
        enabled["round"]["deliveries"][0]["error"]["code"],
        "unsafe_sync_path"
    );
    assert!(!file(&beta, ".ssh/demo").exists());
}

#[test]
fn a_masters_overrides_stay_local_and_never_rewrite_its_files() {
    let (alpha, beta) = pair();
    master_files(&alpha);
    // The master's own tree holds things sync never carries.
    write(&alpha, ".config/demo/.git/HEAD", "ref: refs/heads/main\n");
    std::os::unix::fs::symlink("/etc/hosts", file(&alpha, ".config/demo/hosts")).unwrap();
    enable(&alpha);
    let fleet_secret = read(&alpha, ".config/demo/secret.json").unwrap();

    // An override on the master is laid in place; the fleet keeps its value.
    write(
        &alpha,
        ".elsecall/sync-overrides/.config/demo/secret.json",
        "{\"token\":\"alpha-only\"}",
    );
    push(&alpha);
    assert_eq!(
        read(&alpha, ".config/demo/secret.json").as_deref(),
        Some("{\"token\":\"alpha-only\"}")
    );
    // Other changes on the master still go out, and its own tree is untouched.
    write(&alpha, ".config/demo/run.sh", "#!/bin/sh\necho changed\n");
    push(&alpha);
    assert_eq!(
        read(&beta, ".config/demo/run.sh"),
        read(&alpha, ".config/demo/run.sh")
    );
    assert_eq!(
        read(&beta, ".config/demo/secret.json"),
        Some(fleet_secret.clone())
    );
    assert!(file(&alpha, ".config/demo/.git/HEAD").exists());
    assert!(std::fs::symlink_metadata(file(&alpha, ".config/demo/hosts")).is_ok());
    let source = status_of(&alpha, "alpha");
    assert_eq!(source["state"], "source", "{source}");
    assert_eq!(
        source["sync"]["paths"][0]["overridden"][0],
        "~/.config/demo/secret.json"
    );

    // Removing the override restores the fleet's value; it is never issued.
    std::fs::remove_file(file(
        &alpha,
        ".elsecall/sync-overrides/.config/demo/secret.json",
    ))
    .unwrap();
    push(&alpha);
    assert_eq!(
        read(&alpha, ".config/demo/secret.json"),
        Some(fleet_secret.clone())
    );
    assert_eq!(read(&beta, ".config/demo/secret.json"), Some(fleet_secret));
    let history = beta.ok(&["sync", "history", "--limit", "100"]).to_string();
    assert!(!history.contains("alpha-only"));
    assert!(file(&alpha, ".config/demo/.git/HEAD").exists());
}

#[test]
fn paths_the_master_no_longer_declares_are_paused() {
    let (alpha, beta) = pair();
    master_files(&alpha);
    enable(&alpha);
    let configuration = Paths::for_home(alpha.home.path()).unwrap().configuration();
    std::fs::write(&configuration, "").unwrap();
    write(&alpha, ".config/demo/sub/token", "while undeclared");
    push(&alpha);
    assert_eq!(
        read(&beta, ".config/demo/sub/token").as_deref(),
        Some("alpha-token")
    );
    let source = status_of(&alpha, "alpha");
    assert_eq!(source["sync"]["paused"][0], "~/.config/demo", "{source}");
    // Declared again, it resumes.
    std::fs::write(&configuration, DECLARED).unwrap();
    push(&alpha);
    assert_eq!(
        read(&beta, ".config/demo/sub/token").as_deref(),
        Some("while undeclared")
    );
}
