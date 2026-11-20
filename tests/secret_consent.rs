//! Configuration sync asks before syncing a change to files that look like
//! secrets: the change waits on the master, a `secret_consent` attention item
//! names the files (never their content), and a person confirms it once, or for
//! every later change too. Hard-blocked paths stay refused.
use crate::members::{TestMember, enrol, json};
use serde_json::{Value, json};
use std::path::PathBuf;

const DECLARED: &str = "[sync]\npaths = [\"~/.config/acme-ios\", \"~/.ssh/id_ed25519\"]\n";

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

/// Enables `path` from its plan as a person does. The plan lists secret-looking
/// files, so the CLI refuses to confirm it without a terminal, as an agent's
/// tool call has none; the person's confirmation goes to the local route.
fn enable(alpha: &TestMember, path: &str) -> Value {
    let planned = alpha.ok(&["sync", "enable", path]);
    let id = planned["plan"]["plan_id"].as_str().unwrap().to_owned();
    let refused = alpha.run(&["sync", "enable", path, "--confirm", &id]);
    assert_eq!(
        json(&refused)["error"]["code"],
        "person_required",
        "{refused:?}"
    );
    local(
        alpha,
        "/v1/sync/enable",
        json!({"path": path, "confirm": id}),
    )
    .unwrap()
}

/// A request to `member`'s local service, as its CLI sends it.
fn local(
    member: &TestMember,
    route: &str,
    body: Value,
) -> Result<Value, elsecall_contract::failure::Failure> {
    let socket = elsecall_host::paths::Paths::for_home(member.home.path())
        .unwrap()
        .socket();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(elsecall_host::client::post_large::<Value, _>(
            &socket, route, &body,
        ))
}

fn consent_items(member: &TestMember) -> Vec<Value> {
    member.fleet()["attention"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["category"] == "secret_consent")
        .cloned()
        .collect()
}

/// What a person's confirmation sends: the CLI refuses `--confirm` without a
/// terminal, so the test speaks to the local route as that CLI would.
fn confirm(
    member: &TestMember,
    path: &str,
    id: &str,
    always: bool,
) -> Result<Value, elsecall_contract::failure::Failure> {
    local(
        member,
        "/v1/sync/consent",
        json!({"path": path, "confirm": id, "always": always}),
    )
}

#[test]
fn a_secret_change_waits_for_a_person_once_or_always() {
    let alpha = TestMember::start_with("alpha", DECLARED, &[]);
    let beta = TestMember::start_with("beta", DECLARED, &[]);
    enrol(&alpha, &beta);
    // A sample assembled at run time, as a token in a real .env would read.
    let token = format!("gh{}", "p_aB3dE5fG7hJ9".repeat(3));
    write(
        &alpha,
        ".config/acme-ios/settings.json",
        "{\"scheme\":\"Acme\"}\n",
    );
    write(
        &alpha,
        ".config/acme-ios/.env",
        &format!("API_TOKEN={token}\n"),
    );

    // The plan names the secret-looking file; enabling it syncs what was shown.
    let planned = alpha.ok(&["sync", "plan", "~/.config/acme-ios"]);
    let secret = &planned["paths"][0]["secret_looking"];
    assert_eq!(secret[0]["file"], "~/.config/acme-ios/.env", "{planned}");
    assert_eq!(
        secret[0]["reasons"],
        json!(["name", "token", "secret_assignment"])
    );
    enable(&alpha, "~/.config/acme-ios");
    assert_eq!(
        read(&beta, ".config/acme-ios/.env"),
        read(&alpha, ".config/acme-ios/.env")
    );

    // A change to it waits on the master, ordinary files in the path with it.
    let rotated = format!("gh{}", "p_zY9xW8vU7tS6".repeat(3));
    write(
        &alpha,
        ".config/acme-ios/.env",
        &format!("API_TOKEN={rotated}\n"),
    );
    write(
        &alpha,
        ".config/acme-ios/settings.json",
        "{\"scheme\":\"Acme Beta\"}\n",
    );
    alpha.ok(&["sync", "push"]);
    assert!(
        read(&beta, ".config/acme-ios/.env")
            .unwrap()
            .contains(&token)
    );
    let items = consent_items(&alpha);
    assert_eq!(items.len(), 1, "{items:?}");
    let item = &items[0];
    assert_eq!(item["id"], "secret_consent:alpha:~/.config/acme-ios");
    assert_eq!(item["member"], "alpha");
    assert_eq!(item["subject"], "~/.config/acme-ios");
    assert!(
        item["message"]
            .as_str()
            .unwrap()
            .contains("~/.config/acme-ios/.env")
    );
    for action in item["actions"].as_array().unwrap() {
        assert_eq!(action["person_only"], true, "{item}");
        assert_eq!(action["run_on"], "alpha");
    }
    assert_eq!(
        item["actions"][1]["command"]
            .as_array()
            .unwrap()
            .last()
            .unwrap(),
        "--always"
    );
    assert_eq!(
        consent_items(&beta),
        items,
        "every member's fleet view shows it"
    );
    // Names only: no value reaches the fleet view, the log or sync history.
    for text in [
        alpha.fleet().to_string(),
        alpha.log(),
        alpha.ok(&["sync", "history"]).to_string(),
        alpha
            .ok(&["sync", "consent", "~/.config/acme-ios"])
            .to_string(),
    ] {
        assert!(
            !text.contains(&rotated) && !text.contains(&token),
            "a value leaked"
        );
    }

    // So does bringing one member up to date, as before a placed run.
    alpha.ok(&["sync", "push", "--member", "beta"]);
    assert!(
        read(&beta, ".config/acme-ios/.env")
            .unwrap()
            .contains(&token)
    );

    // Planning the path again and enabling it from that plan holds the change
    // too: a plan consents only to a path's first content.
    let replanned = alpha.ok(&["sync", "enable", "~/.config/acme-ios"]);
    let plan_id = replanned["plan"]["plan_id"].as_str().unwrap().to_owned();
    local(
        &alpha,
        "/v1/sync/enable",
        json!({"path": "~/.config/acme-ios", "confirm": plan_id}),
    )
    .unwrap();
    assert!(
        read(&beta, ".config/acme-ios/.env")
            .unwrap()
            .contains(&token)
    );
    assert_eq!(consent_items(&alpha), items);

    // An agent's command line has no terminal, so it cannot confirm.
    let id = item["actions"][0]["command"][5]
        .as_str()
        .unwrap()
        .to_owned();
    let refused = alpha.run(&["sync", "consent", "~/.config/acme-ios", "--confirm", &id]);
    assert_eq!(
        json(&refused)["error"]["code"],
        "person_required",
        "{refused:?}"
    );
    assert!(
        read(&beta, ".config/acme-ios/.env")
            .unwrap()
            .contains(&token)
    );

    // Confirmed once: the change syncs, and the next one asks again.
    let confirmed = confirm(&alpha, "~/.config/acme-ios", &id, false).unwrap();
    assert_eq!(confirmed["status"], "confirmed", "{confirmed}");
    assert!(
        read(&beta, ".config/acme-ios/.env")
            .unwrap()
            .contains(&rotated)
    );
    assert_eq!(
        read(&beta, ".config/acme-ios/settings.json").as_deref(),
        Some("{\"scheme\":\"Acme Beta\"}\n")
    );
    assert!(consent_items(&alpha).is_empty());
    write(
        &alpha,
        ".config/acme-ios/.env",
        &format!("API_TOKEN={token}\n"),
    );
    alpha.ok(&["sync", "push"]);
    assert!(
        read(&beta, ".config/acme-ios/.env")
            .unwrap()
            .contains(&rotated)
    );
    let item = consent_items(&alpha).remove(0);
    let id = item["actions"][1]["command"][5]
        .as_str()
        .unwrap()
        .to_owned();
    // A stale ID confirms nothing.
    let stale = confirm(&alpha, "~/.config/acme-ios", "0000000000000000", true);
    assert_eq!(stale.unwrap_err().code, "consent_changed");
    assert!(
        read(&beta, ".config/acme-ios/.env")
            .unwrap()
            .contains(&rotated)
    );

    // Always: this change syncs, and later ones to the file sync unasked.
    let confirmed = confirm(&alpha, "~/.config/acme-ios", &id, true).unwrap();
    assert_eq!(confirmed["always"], true);
    assert!(
        read(&beta, ".config/acme-ios/.env")
            .unwrap()
            .contains(&token)
    );
    write(
        &alpha,
        ".config/acme-ios/.env",
        &format!("API_TOKEN={rotated}\n"),
    );
    alpha.ok(&["sync", "push"]);
    assert!(
        read(&beta, ".config/acme-ios/.env")
            .unwrap()
            .contains(&rotated)
    );
    assert!(consent_items(&alpha).is_empty());
    // A new secret-looking file is asked about again.
    write(&alpha, ".config/acme-ios/signing.p12", "binary");
    alpha.ok(&["sync", "push"]);
    assert!(read(&beta, ".config/acme-ios/signing.p12").is_none());
    assert_eq!(consent_items(&alpha).len(), 1);
}

#[test]
fn ask_first_paths_sync_by_consent_and_hard_blocked_paths_stay_refused() {
    let alpha = TestMember::start_with("alpha", DECLARED, &[]);
    let beta = TestMember::start_with("beta", DECLARED, &[]);
    enrol(&alpha, &beta);
    // A key for git submodules, inside ~/.ssh: asked first, then synced.
    let key = format!("-----{} OPENSSH PRIVATE KEY-----\nAAAA\n", "BEGIN");
    write(&alpha, ".ssh/id_ed25519", &key);
    let planned = alpha.ok(&["sync", "plan", "~/.ssh/id_ed25519"]);
    let reasons = &planned["paths"][0]["secret_looking"][0]["reasons"];
    assert_eq!(
        reasons,
        &json!(["name", "sensitive_location", "private_key"]),
        "{planned}"
    );
    enable(&alpha, "~/.ssh/id_ed25519");
    assert_eq!(
        read(&beta, ".ssh/id_ed25519").as_deref(),
        Some(key.as_str())
    );

    for refused in [
        "~/.ssh",
        "~/.ssh/authorized_keys",
        "~/Library/Keychains/login.keychain-db",
        "~/Library/LaunchAgents",
        "~/.elsecall",
        "~/.local/bin",
    ] {
        let output = alpha.run(&["sync", "plan", refused]);
        assert_eq!(output.status.code(), Some(2), "{refused}: {output:?}");
    }
}
