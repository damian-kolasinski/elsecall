//! Full service configuration lifecycle with real temporary member services.
mod file_gate;
mod members;
mod processes;
mod sync_gate;
use elsecall::{
    config_sync::{
        manifest::{FileNode, Manifest},
        protocol::{self, Message, Purpose},
        state::State,
    },
    peer_client,
};
use members::{TestMember, enrol};
use serde_json::Value;
use std::{
    process::Stdio,
    thread::sleep,
    time::{Duration, Instant},
};

const CONFIG: &str = r#"
[defaults.logs]
max_size = "2MB"
keep = 3
[defaults.retention]
sync_backups = 50
[defaults.placement]
keep_local_margin = 0.271828
[defaults.admission]
saturated_utilization = 0.75
[members.alpha.capacity]
reserved_cores = 1
[members.beta.capacity]
reserved_cores = 3
[members.beta.workspace]
root = "/tmp/service-config-workspaces"
object_sources = ["/tmp/SERVICE-CONFIG-SENTINEL"]
"#;
fn source(member: &TestMember, text: &str) {
    std::fs::write(member.data().join("fleet-config.toml"), text).unwrap();
}
fn held(member: &TestMember) -> Manifest {
    State::load(&member.data().join("config-sync/state.json"))
        .unwrap()
        .manifest
        .unwrap()
}
fn service(member: &TestMember, name: &str) -> Value {
    let result = member.run(&["fleet"]);
    if !result.status.success() {
        return Value::Null;
    }
    members::json(&result)["data"]["members"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["name"] == name)
        .unwrap()["status"]["config_sync"]["service"]
        .clone()
}
fn until(member: &TestMember, predicate: impl Fn() -> bool) {
    let deadline = Instant::now() + crate::processes::WAIT;
    while !predicate() {
        assert!(Instant::now() < deadline, "timed out: {}", member.log());
        sleep(Duration::from_millis(100));
    }
}
fn current(member: &TestMember, name: &str) {
    let deadline = Instant::now() + crate::processes::WAIT;
    loop {
        let observed = service(member, name);
        if observed["current"] == true {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{name}: {observed}; {}; fleet: {:?}",
            member.log(),
            member.run(&["fleet"])
        );
        sleep(Duration::from_millis(100));
    }
}
/// A member whose core count stays above the fleet reserve on a small host.
fn scripted_member(name: &'static str, configuration: &str) -> (TestMember, tempfile::TempDir) {
    let directory = tempfile::tempdir().unwrap();
    let load = directory.path().join("load.json");
    std::fs::write(&load, r#"{"performance_cores":8}"#).unwrap();
    let script = format!("file:{}", load.display());
    let member = TestMember::start_with(name, configuration, &[("ELSECALL_MACHINE_LOAD", &script)]);
    (member, directory)
}
fn enable(member: &TestMember) -> Value {
    let plan = member.ok(&["sync", "service"]);
    assert_eq!(plan["changed"], false);
    member.ok(&[
        "sync",
        "service",
        "--confirm",
        plan["plan"]["plan_id"].as_str().unwrap(),
    ])
}

#[test]
fn removing_a_lease_retries_partial_delivery_without_another_generation() {
    use elsecall::{
        paths::Paths, scheduling::timestamp::Timestamp, service_configuration::RehearsalSettings,
    };
    let alpha = TestMember::start("alpha");
    let beta = TestMember::start("beta");
    enrol(&alpha, &beta);
    let gate = sync_gate::SyncGate::start(&beta);
    gate.enroll(&alpha, &beta);
    source(&alpha, CONFIG);
    enable(&alpha);
    current(&alpha, "alpha");
    current(&alpha, "beta");
    let before = held(&alpha).service.unwrap();
    let effective_before = [
        service(&alpha, "alpha")["effective_fingerprint"].clone(),
        service(&alpha, "beta")["effective_fingerprint"].clone(),
    ];
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let socket = Paths::for_home(alpha.home.path()).unwrap().socket();
    let call = |until, settings| {
        runtime
            .block_on(elsecall::client::post_large::<Value, _>(
                &socket,
                "/v1/sync/rehearsal",
                &RehearsalSettings {
                    session: "0123456789abcdef01234567".into(),
                    until,
                    settings,
                },
            ))
            .unwrap()
    };
    let applied = call(
        Some(Timestamp::now().after(Duration::from_secs(180))),
        std::collections::BTreeMap::from([("placement.keep_local_margin".into(), "0.4".into())]),
    );
    assert_eq!(applied["round"]["complete"], true, "{applied}");
    current(&alpha, "alpha");
    current(&alpha, "beta");
    let leased = held(&beta).service.unwrap();
    assert_ne!(leased, before);
    // Keep real validation intact; lose only the following delivery connection.
    gate.fail_sync_after(2);
    let partial = call(None, Default::default());
    assert_eq!(partial["round"]["complete"], false, "{partial}");
    let restored = held(&alpha);
    assert_eq!(restored.service, Some(before));
    assert_eq!(held(&beta).service, Some(leased));
    let retried = call(None, Default::default());
    assert_eq!(retried["round"]["complete"], true, "{retried}");
    assert_eq!(retried["generation"], restored.generation);
    assert_eq!(held(&alpha), restored);
    assert_eq!(held(&beta), restored);
    current(&alpha, "alpha");
    current(&alpha, "beta");
    for (name, before) in ["alpha", "beta"].into_iter().zip(effective_before) {
        assert_eq!(service(&alpha, name)["effective_fingerprint"], before);
    }
}
fn pinned(member: &TestMember, label: &str, script: &str) -> std::process::Child {
    pinned_size(member, label, script, "1")
}
fn pinned_size(member: &TestMember, label: &str, script: &str, size: &str) -> std::process::Child {
    crate::processes::command(env!("CARGO_BIN_EXE_elsecall"))
        .env("HOME", member.home.path())
        .current_dir(member.home.path())
        .args([
            "run", "--size", size, "--label", label, "--", "/bin/sh", "-c", script,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

#[test]
fn migration_validates_plans_reloads_between_work_and_preserves_decision_parameters() {
    let alpha = TestMember::start("alpha");
    let (beta, _load) = scripted_member("beta", "[capacity]\nreserved_cores=2\n");
    enrol(&alpha, &beta);
    source(&alpha, CONFIG);
    let plan = alpha.ok(&["sync", "service"]);
    assert!(
        !beta.data().join("fleet-config.toml").exists(),
        "dry run writes no member settings"
    );
    source(&alpha, &CONFIG.replace("0.271828", "0.4"));
    assert_eq!(
        alpha.fails(&[
            "sync",
            "service",
            "--confirm",
            plan["plan"]["plan_id"].as_str().unwrap()
        ]),
        "plan_changed"
    );
    assert!(!beta.data().join("fleet-config.toml").exists());
    source(&alpha, CONFIG);
    let plan = alpha.ok(&["sync", "service"]);
    std::fs::write(
        beta.data().join("config.toml"),
        "[capacity]\nreserved_cores=1\n",
    )
    .unwrap();
    assert_eq!(
        alpha.fails(&[
            "sync",
            "service",
            "--confirm",
            plan["plan"]["plan_id"].as_str().unwrap()
        ]),
        "plan_changed"
    );
    std::fs::write(
        beta.data().join("config.toml"),
        "[capacity]\nreserved_cores=2\n",
    )
    .unwrap();
    // An explicit boundary signal proves the process is running before issuing.
    let signal = beta.home.path().join("started");
    let finish = beta.home.path().join("finish");
    let finish_gate = file_gate::FileGate::new(beta.home.path(), "finish");
    let capacity = beta.ok(&["runs"])["member"]["capacity"]["capacity"]
        .as_f64()
        .unwrap()
        .to_string();
    let run = pinned_size(
        &beta,
        "old-settings",
        &format!(
            "touch '{}'; while [ -f '{}' ]; do sleep 0.1; done; printf survived",
            signal.display(),
            finish.display()
        ),
        &capacity,
    );
    until(&beta, || signal.exists());
    let queued_signal = beta.home.path().join("queued-started");
    let queued_finish = beta.home.path().join("queued-finish");
    let queued_gate = file_gate::FileGate::new(beta.home.path(), "queued-finish");
    let queued = pinned(
        &beta,
        "queued-old-settings",
        &format!(
            "touch '{}'; while [ -f '{}' ]; do sleep 0.1; done",
            queued_signal.display(),
            queued_finish.display()
        ),
    );
    until(&beta, || {
        beta.ok(&["runs"])["member"]["queued"]["interactive"] == 1
    });
    let before_log = beta.log();
    let result = enable(&alpha);
    assert_eq!(result["round"]["complete"], true, "{result}");
    assert_eq!(service(&beta, "beta")["pending"], true);
    sleep(Duration::from_millis(1200));
    assert_eq!(
        beta.log().matches("service_ready").count(),
        before_log.matches("service_ready").count(),
        "no restart while work runs"
    );
    finish_gate.open();
    let result = run.wait_with_output().unwrap();
    assert!(result.status.success(), "{result:?}");
    assert_eq!(result.stdout, b"survived");
    until(&beta, || queued_signal.exists());
    assert_eq!(service(&beta, "beta")["pending"], true);
    queued_gate.open();
    assert!(queued.wait_with_output().unwrap().status.success());
    current(&alpha, "alpha");
    current(&alpha, "beta");
    assert_eq!(
        std::fs::read_to_string(beta.data().join("config.toml")).unwrap(),
        "[capacity]\nreserved_cores=2\n"
    );
    assert_eq!(
        std::fs::read_to_string(beta.data().join("fleet-config.toml")).unwrap(),
        CONFIG
    );
    let state = beta.ok(&["runs"])["member"].clone();
    assert_eq!(state["capacity"]["reserved"], 3.0, "{state}");
    let old = beta.ok(&["runs"])["runs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["label"] == "old-settings")
        .unwrap()
        .clone();
    let old_record = beta.ok(&["explain", old["run_id"].as_str().unwrap()]);
    assert_eq!(
        old_record["decision"]["parameters"]["admission"]["saturated_utilization"], 0.9,
        "{old_record}"
    );
    let run = pinned(&beta, "new-settings", "true")
        .wait_with_output()
        .unwrap();
    assert!(run.status.success(), "{run:?}");
    let new = beta.ok(&["runs"])["runs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["label"] == "new-settings")
        .unwrap()
        .clone();
    let record = beta.ok(&["explain", new["run_id"].as_str().unwrap()]);
    assert_eq!(
        record["decision"]["parameters"]["admission"]["saturated_utilization"], 0.75,
        "{record}"
    );
    assert_eq!(
        record["decision"]["parameters"]["placement"]["keep_local_margin"], 0.271828,
        "{record}"
    );
    let diagnostic = format!(
        "{}{}{}{}",
        beta.log(),
        alpha.log(),
        beta.ok(&["fleet"]),
        members::json(&beta.run(&["doctor"]))
    );
    assert!(!diagnostic.contains("SERVICE-CONFIG-SENTINEL"));
    // An invalid master change cannot issue or alter the working generation.
    let generation = held(&alpha).generation;
    source(&alpha, "[defaults.placement]\nkeep_local_margin=2\n");
    assert_eq!(
        alpha.fails(&["sync", "push"]),
        "invalid_service_configuration"
    );
    assert_eq!(held(&alpha).generation, generation);
    assert!(
        alpha.ok(&["sync", "status"])["next_step"]
            .as_str()
            .unwrap()
            .contains("rejected")
    );
    assert_eq!(
        service(&alpha, "alpha")["rejection"]["code"],
        "invalid_service_configuration"
    );
    source(&alpha, CONFIG);
    alpha.ok(&["sync", "push"]);
    current(&alpha, "alpha");
    // Local member edits cannot override the accepted settings or break startup.
    std::fs::write(beta.data().join("config.toml"), "not toml").unwrap();
    source(&alpha, &CONFIG.replace("0.75", "0.8"));
    alpha.ok(&["sync", "push"]);
    current(&alpha, "beta");
    assert_eq!(beta.ok(&["runs"])["member"]["capacity"]["reserved"], 3.0);
    // Replica restoration happens at role change; subsequent edits on the new
    // master survive and are issued from it.
    source(&beta, "# unpublished member drift");
    alpha.ok(&["fleet", "move-master", "beta"]);
    until(&beta, || {
        std::fs::read_to_string(beta.data().join("fleet-config.toml")).unwrap()
            == CONFIG.replace("0.75", "0.8")
    });
    source(&beta, &CONFIG.replace("0.75", "0.85"));
    beta.ok(&["sync", "push"]);
    current(&alpha, "alpha");
    current(&beta, "beta");
    current(&beta, "alpha");
    assert_eq!(
        std::fs::read_to_string(alpha.data().join("fleet-config.toml")).unwrap(),
        CONFIG.replace("0.75", "0.85")
    );
    assert_eq!(alpha.fails(&["sync", "service"]), "not_master");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_member_validates_the_received_document_and_keeps_its_working_generation() {
    let alpha = TestMember::start("alpha");
    let beta = TestMember::start("beta");
    enrol(&alpha, &beta);
    source(&alpha, CONFIG);
    enable(&alpha);
    current(&alpha, "beta");
    current(&alpha, "alpha");
    let old = held(&beta);
    let mut offered = old.clone();
    offered.generation += 1;
    let bytes = b"[members.beta.workspace]\nroot='SERVICE-CONFIG-SENTINEL'\n";
    offered.service = Some(FileNode {
        mode: 0o600,
        size: bytes.len() as u64,
        fingerprint: old.key.fingerprint(bytes),
    });
    let key = alpha.key();
    let target = beta.as_member();
    let mut channel = peer_client::channel(
        &key,
        &target,
        protocol::ROUTE,
        protocol::PURPOSE,
        protocol::PROTOCOL,
    )
    .await
    .unwrap();
    protocol::send(
        &mut channel,
        &Message::Offer {
            purpose: Purpose::Push,
            manifest: offered,
        },
    )
    .await
    .unwrap();
    let Message::Need { fingerprints } = protocol::receive(&mut channel).await.unwrap() else {
        panic!("need");
    };
    assert_eq!(fingerprints.len(), 1);
    protocol::send_blob(&mut channel, bytes).await.unwrap();
    protocol::send(&mut channel, &Message::Commit)
        .await
        .unwrap();
    let Message::Failed { failure } = protocol::receive(&mut channel).await.unwrap() else {
        panic!("failed");
    };
    assert_eq!(failure.code, "invalid_service_configuration");
    assert!(!failure.message.contains("SERVICE-CONFIG-SENTINEL"));
    assert_eq!(held(&beta), old);
    assert_eq!(
        service(&alpha, "beta")["rejection"]["code"],
        "invalid_service_configuration"
    );
    assert!(!beta.log().contains("SERVICE-CONFIG-SENTINEL"));
    alpha.ok(&["sync", "push", "--member", "beta"]);
    current(&alpha, "beta");
    assert!(service(&alpha, "beta")["rejection"].is_null());
}

#[test]
fn a_managed_solo_service_restarts_without_live_tailnet_presence() {
    let alpha = TestMember::start("alpha");
    source(&alpha, "[members.alpha.capacity]\nreserved_cores=1\n");
    enable(&alpha);
    current(&alpha, "alpha");
    assert_eq!(alpha.ok(&["runs"])["member"]["capacity"]["reserved"], 1.0);
}

#[test]
fn a_future_workspace_root_overlapping_a_mirrored_path_is_rejected_in_the_plan() {
    let alpha = TestMember::start_with("alpha", "[sync]\npaths=['~/.config/demo']\n", &[]);
    let beta = TestMember::start("beta");
    enrol(&alpha, &beta);
    let path = alpha.home.path().join(".config/demo");
    std::fs::create_dir_all(&path).unwrap();
    std::fs::write(path.join("file"), "hello").unwrap();
    let plan = alpha.ok(&["sync", "enable", "~/.config/demo"]);
    alpha.ok(&[
        "sync",
        "enable",
        "~/.config/demo",
        "--confirm",
        plan["plan"]["plan_id"].as_str().unwrap(),
    ]);
    let old = held(&beta);
    source(
        &alpha,
        &format!(
            "[members.beta.workspace]\nroot={:?}\n",
            beta.home.path().join(".config/demo").to_str().unwrap()
        ),
    );
    let plan = alpha.ok(&["sync", "service"]);
    assert_eq!(plan["plan"]["complete"], false, "{plan}");
    let beta_plan = plan["plan"]["members"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["member"] == "beta")
        .unwrap();
    assert_eq!(
        beta_plan["error"]["code"], "invalid_service_configuration",
        "{plan}"
    );
    assert_eq!(
        alpha.fails(&[
            "sync",
            "service",
            "--confirm",
            plan["plan"]["plan_id"].as_str().unwrap()
        ]),
        "service_plan_incomplete"
    );
    assert_eq!(held(&beta), old);
    assert!(!beta.data().join("fleet-config.toml").exists());
    std::os::unix::fs::symlink(
        beta.home.path().join(".config/demo"),
        beta.home.path().join("alias"),
    )
    .unwrap();
    source(
        &alpha,
        &format!(
            "[members.beta.workspace]\nroot={:?}\n",
            beta.home
                .path()
                .join("alias/missing/workspaces")
                .to_str()
                .unwrap()
        ),
    );
    let plan = alpha.ok(&["sync", "service"]);
    assert_eq!(plan["plan"]["complete"], false, "{plan}");
    assert_eq!(held(&beta), old);
    assert!(!beta.home.path().join(".config/demo/missing").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn automatic_application_waits_for_an_incoming_sync_exchange_to_commit() {
    let alpha = TestMember::start("alpha");
    let (beta, _load) = scripted_member("beta", "");
    enrol(&alpha, &beta);
    source(&alpha, CONFIG);
    enable(&alpha);
    current(&alpha, "alpha");
    current(&alpha, "beta");
    let started = beta.home.path().join("started");
    let finished = beta.home.path().join("finished");
    let finish_gate = file_gate::FileGate::new(beta.home.path(), "finished");
    let work = pinned(
        &beta,
        "hold-before-sync",
        &format!(
            "touch '{}'; while [ -f '{}' ]; do sleep 0.1; done",
            started.display(),
            finished.display()
        ),
    );
    until(&beta, || started.exists());
    source(&alpha, &CONFIG.replace("0.75", "0.8"));
    alpha.ok(&["sync", "push"]);
    assert_eq!(service(&beta, "beta")["pending"], true);
    let held = held(&alpha);
    let before = beta.log().matches("service_ready").count();
    let mut channel = peer_client::channel(
        &alpha.key(),
        &beta.as_member(),
        protocol::ROUTE,
        protocol::PURPOSE,
        protocol::PROTOCOL,
    )
    .await
    .unwrap();
    protocol::send(
        &mut channel,
        &Message::Offer {
            purpose: Purpose::Push,
            manifest: held,
        },
    )
    .await
    .unwrap();
    assert!(matches!(
        protocol::receive(&mut channel).await.unwrap(),
        Message::Need { .. }
    ));
    finish_gate.open();
    assert!(work.wait_with_output().unwrap().status.success());
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(
        beta.log().matches("service_ready").count(),
        before,
        "the peer exchange keeps restart deferred"
    );
    protocol::send(&mut channel, &Message::Commit)
        .await
        .unwrap();
    assert!(matches!(
        protocol::receive(&mut channel).await.unwrap(),
        Message::Applied { .. }
    ));
    drop(channel);
    current(&alpha, "beta");
}

#[test]
fn corrupt_unmanaged_sync_state_does_not_take_down_local_diagnostics() {
    let mut alpha = TestMember::start("alpha");
    alpha.stop();
    std::fs::write(alpha.data().join("config-sync/state.json"), "broken").unwrap();
    alpha.restart();
    alpha.ok(&["status"]);
    assert_eq!(alpha.fails(&["sync", "status"]), "invalid_sync_state");
}

#[test]
fn startup_uses_the_generation_finished_during_master_recovery() {
    use elsecall::config_sync::store::{FileStore, ReplicaStore};
    let mut alpha = TestMember::start("alpha");
    let beta = TestMember::start("beta");
    enrol(&alpha, &beta);
    source(&alpha, CONFIG);
    enable(&alpha);
    current(&alpha, "alpha");
    current(&alpha, "beta");
    alpha.stop();
    let path = alpha.data().join("config-sync/state.json");
    let mut state = State::load(&path).unwrap();
    let mut recovering = state.manifest.clone().unwrap();
    recovering.generation += 1;
    let bytes = CONFIG.replace("0.75", "0.8");
    let node = FileNode {
        mode: 0o600,
        size: bytes.len() as u64,
        fingerprint: recovering.key.fingerprint(bytes.as_bytes()),
    };
    FileStore::open(&alpha.data().join("config-sync/objects"))
        .unwrap()
        .put(&recovering.key, &node, bytes.as_bytes())
        .unwrap();
    recovering.service = Some(node);
    state.applying = Some(recovering);
    state.acting_as_source = false;
    state.service_source_master = false;
    state.save(&path).unwrap();
    alpha.restart();
    current(&alpha, "alpha");
    let run = pinned(&alpha, "after-recovery", "true")
        .wait_with_output()
        .unwrap();
    assert!(run.status.success());
    let record = alpha.ok(&["runs"])["runs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["label"] == "after-recovery")
        .unwrap()
        .clone();
    assert_eq!(
        alpha.ok(&["explain", record["run_id"].as_str().unwrap()])["decision"]["parameters"]["admission"]
            ["saturated_utilization"],
        0.8
    );
    assert_eq!(
        std::fs::read_to_string(alpha.data().join("fleet-config.toml")).unwrap(),
        bytes
    );
}

#[test]
fn consecutive_generations_restart_both_services_without_stranding_the_service_lock() {
    let alpha = TestMember::start("alpha");
    let beta = TestMember::start("beta");
    enrol(&alpha, &beta);
    source(&alpha, CONFIG);
    enable(&alpha);
    current(&alpha, "alpha");
    current(&alpha, "beta");
    for utilization in ["0.76", "0.77", "0.78", "0.79"] {
        source(&alpha, &CONFIG.replace("0.75", utilization));
        alpha.ok(&["sync", "push"]);
        current(&alpha, "alpha");
        current(&alpha, "beta");
    }
    assert!(alpha.log().matches("service_ready").count() >= 6);
    assert!(beta.log().matches("service_ready").count() >= 6);
    assert!(!alpha.log().contains("already_running"));
    assert!(!beta.log().contains("already_running"));
}
