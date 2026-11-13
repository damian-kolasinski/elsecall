//! `elsecall fleet upgrade` between real installed services in temporary homes,
//! with loopback standing in for the tailnet. Members are installed with their
//! build's own `install` and run as plain processes instead of LaunchAgents; the
//! builds are real executables: this test build, copies of it re-signed under
//! another identifier (a different build ID), and `/usr/bin/false`, a build that
//! never becomes ready.
#![cfg(target_os = "macos")]
mod members;
mod processes;

use elsecall::{
    build_identity::Digest,
    executable_format::Architecture,
    fleet_upgrade::{
        member::UpgradeStatus,
        protocol::{self, Manifest, Message, Os, UpgradeId},
    },
    peer_client,
};
use members::{TestMember, enrol, json};
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const BUILD: &str = env!("CARGO_BIN_EXE_elsecall");

/// A copy of this build signed under another identifier: the same program with
/// another build ID.
fn variant(directory: &Path, name: &str) -> PathBuf {
    let path = directory.join(name);
    std::fs::copy(BUILD, &path).unwrap();
    let signed = crate::processes::command("/usr/bin/codesign")
        .args(["--force", "--sign", "-", "--identifier"])
        .arg(format!("dev.elsecall.test.{name}"))
        .arg(&path)
        .output()
        .unwrap();
    assert!(signed.status.success(), "{signed:?}");
    path
}

fn builds() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("ec-builds-")
        .tempdir_in("/tmp")
        .unwrap()
}

fn build_of(path: &Path) -> String {
    Digest::of_file(path).unwrap().build_id().to_string()
}

fn health(member: &TestMember) -> Value {
    member.ok(&["status"])
}

/// Runs `fleet upgrade` and returns its exit code and data.
fn upgrade(master: &TestMember, args: &[&str]) -> (i32, Value) {
    let mut full = vec!["fleet", "upgrade"];
    full.extend_from_slice(args);
    let output = master.run(&full);
    let value = json(&output);
    (output.status.code().unwrap(), value)
}

fn outcome<'a>(report: &'a Value, member: &str) -> &'a Value {
    report["data"]["members"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["name"] == member)
        .unwrap_or_else(|| panic!("no outcome for {member}: {report}"))
}

fn skewed(view: &Value) -> Vec<String> {
    view["version_skew"]["members"]
        .as_array()
        .map(|members| {
            members
                .iter()
                .map(|m| m.as_str().unwrap().to_owned())
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn the_master_upgrades_one_member_then_every_member_and_skew_clears() {
    let directory = builds();
    let older = variant(directory.path(), "older");
    let alpha = TestMember::install("alpha", Path::new(BUILD));
    let bravo = TestMember::install("bravo", &older);
    let charlie = TestMember::install("charlie", &older);
    enrol(&alpha, &bravo);
    enrol(&alpha, &charlie);
    let new = build_of(Path::new(BUILD));
    let old = build_of(&older);
    assert_ne!(new, old);
    assert_eq!(health(&bravo)["build"], old);

    // Skew is flagged in the fleet view and doctor, and work between different
    // builds is refused with version_skew while status keeps working.
    let view = alpha.fleet();
    assert_eq!(skewed(&view), ["bravo", "charlie"], "{view}");
    assert_eq!(view["version_skew"]["master_build"], new);
    assert_eq!(view["version_skew"]["refused_with"], "version_skew");
    assert_eq!(alpha.view_of("bravo")["build"]["build"], old);
    assert_eq!(alpha.view_of("bravo")["version_skew"], true);
    assert_eq!(alpha.view_of("alpha")["version_skew"], false);
    // Doctor flags it; its verdict stays about this machine (here unhealthy only
    // if this machine's firewall blocks the temporary installation).
    let doctor = json(&bravo.run(&["doctor"]));
    assert_eq!(
        doctor["data"]["version_skew"]["members"][0], "bravo",
        "{doctor}"
    );
    assert_eq!(doctor["data"]["fleet_builds"][1]["name"], "bravo");
    assert_eq!(doctor["data"]["fleet_builds"][1]["version_skew"], true);
    assert_eq!(doctor["data"]["service"]["build"], old);
    let report = alpha.ok(&["report", "--since", "1h"]);
    let bravo_report = report["members"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["member"] == "bravo")
        .unwrap();
    assert_eq!(
        bravo_report["records_error"]["code"], "version_skew",
        "{bravo_report}"
    );

    // One member.
    let pid = bravo.installed_pid().unwrap();
    let (code, report) = upgrade(&alpha, &["--member", "bravo"]);
    assert_eq!(code, 0, "{report}");
    assert_eq!(report["data"]["complete"], true);
    assert_eq!(report["data"]["distributed"]["build"], new);
    assert_eq!(report["data"]["members"].as_array().unwrap().len(), 1);
    let bravo_outcome = outcome(&report, "bravo");
    assert_eq!(bravo_outcome["outcome"], "upgraded", "{report}");
    assert_eq!(bravo_outcome["from"]["build"], old);
    assert_eq!(bravo_outcome["running"]["build"], new);
    assert_eq!(health(&bravo)["build"], new);
    assert_ne!(bravo.installed_pid().unwrap(), pid, "the service restarted");
    // Installed through the same owned installation: same path, the new content.
    assert_eq!(
        std::fs::read(bravo.data().join("elsecall")).unwrap(),
        std::fs::read(BUILD).unwrap()
    );
    assert!(bravo.data().join("installation").exists());
    assert!(!bravo.data().join("upgrade/incoming").exists());
    assert!(!bravo.data().join("upgrade/previous").exists());
    assert_eq!(skewed(&alpha.fleet()), ["charlie"]);
    let report = alpha.ok(&["report", "--since", "1h"]);
    assert!(
        report["members"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["member"] == "bravo")
            .unwrap()["records_error"]
            .is_null(),
        "{report}"
    );

    // Every member: charlie is upgraded, bravo already runs the build.
    let (code, report) = upgrade(&alpha, &[]);
    assert_eq!(code, 0, "{report}");
    assert_eq!(outcome(&report, "charlie")["outcome"], "upgraded");
    assert_eq!(outcome(&report, "bravo")["outcome"], "already_current");
    assert!(
        report["data"]["members"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m["name"] != "alpha"),
        "the master is not upgraded over the fleet"
    );
    let view = alpha.fleet();
    assert!(view["version_skew"].is_null(), "{view}");
    for member in ["alpha", "bravo", "charlie"] {
        let seen = alpha.view_of(member);
        assert_eq!(seen["reachability"], "reachable");
        assert_eq!(seen["build"]["build"], new);
    }
    // The upgraded members are still enrolled and in sync.
    assert_eq!(alpha.view_of("charlie")["in_sync"], true);
    assert!(json(&charlie.run(&["doctor"]))["data"]["version_skew"].is_null());
}

#[test]
fn only_the_recorded_master_may_distribute_a_build() {
    let directory = builds();
    let older = variant(directory.path(), "older");
    let alpha = TestMember::install("alpha", Path::new(BUILD));
    let bravo = TestMember::install("bravo", Path::new(BUILD));
    let charlie = TestMember::install("charlie", &older);
    enrol(&alpha, &bravo);
    enrol(&alpha, &charlie);

    // The CLI on a non-master says where to run it.
    assert_eq!(bravo.fails(&["fleet", "upgrade"]), "not_master");
    // The master does not upgrade itself over the fleet.
    let (code, report) = upgrade(&alpha, &["--member", "alpha"]);
    assert_eq!(code, 2, "{report}");
    assert_eq!(report["error"]["code"], "invalid_request");

    // A non-master's key cannot open the upgrade channel, even signing as itself.
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let refused = runtime
        .block_on(peer_client::channel(
            &bravo.key(),
            &charlie.as_member(),
            protocol::ROUTE,
            protocol::PURPOSE,
            protocol::PROTOCOL,
        ))
        .err()
        .expect("refused");
    assert_eq!(refused.code, "not_master");
    assert_eq!(health(&charlie)["build"], build_of(&older));
}

/// Opens the upgrade channel as `master` and offers `manifest`, then sends
/// `content` if the member accepts; returns the member's last answer.
fn offer(
    runtime: &tokio::runtime::Runtime,
    master: &TestMember,
    member: &TestMember,
    manifest: protocol::SignedManifest,
    content: &[u8],
) -> Message {
    runtime.block_on(async {
        let mut channel = peer_client::channel(
            &master.key(),
            &member.as_member(),
            protocol::ROUTE,
            protocol::PURPOSE,
            protocol::PROTOCOL,
        )
        .await
        .unwrap();
        protocol::send(&mut channel, &Message::Offer { manifest })
            .await
            .unwrap();
        match protocol::receive(&mut channel).await.unwrap() {
            Message::Accepted => (),
            other => return other,
        }
        protocol::send_blob(&mut channel, content).await.unwrap();
        protocol::receive(&mut channel).await.unwrap()
    })
}

fn manifest(member: &TestMember, content: &[u8]) -> Manifest {
    Manifest {
        id: UpgradeId::generate().unwrap(),
        recipient: member.as_member().key,
        digest: Digest::of(content),
        size: content.len() as u64,
        os: Os::Macos,
        architectures: vec![Architecture::this().unwrap()],
        wait_seconds: 0,
        issued_at: elsecall::peer_auth::now(),
    }
}

fn refusal(message: Message) -> String {
    match message {
        Message::Refused { failure } => failure.code,
        other => panic!("not refused: {other:?}"),
    }
}

/// The member's latest upgrade record, as the master reads it.
fn latest(runtime: &tokio::runtime::Runtime, master: &TestMember, member: &TestMember) -> Value {
    let status: UpgradeStatus = runtime
        .block_on(peer_client::call(
            &master.key(),
            &member.as_member(),
            hyper::Method::GET,
            protocol::STATUS_ROUTE,
            None::<&()>,
            Duration::from_secs(2),
        ))
        .unwrap();
    serde_json::to_value(status.latest).unwrap()
}

/// Waits until the member reports it is waiting for work labelled `label`.
fn until_draining(
    runtime: &tokio::runtime::Runtime,
    master: &TestMember,
    member: &TestMember,
    label: &str,
) {
    let deadline = Instant::now() + crate::processes::WAIT;
    loop {
        let value = latest(runtime, master, member);
        if value["state"] == "draining" && value["in_flight"][0]["label"] == label {
            return;
        }
        assert!(Instant::now() < deadline, "never waited: {value}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Holds a lease on `member` labelled `label` until `$HOME/release` appears.
fn hold_lease(member: &TestMember, label: &str) -> std::process::Child {
    let home = member.home.path().to_owned();
    let holder = crate::processes::command("/bin/sh")
        .arg("-c")
        .arg(format!(
            "\"$EC\" lease acquire --size 1 --label {label} >/dev/null || exit 1; \
             touch \"$HOME/held\"; \
             while [ ! -f \"$HOME/release\" ]; do sleep 0.05; done; \
             \"$EC\" lease release >/dev/null"
        ))
        .env("HOME", &home)
        .env("EC", BUILD)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + crate::processes::WAIT;
    while !home.join("held").exists() {
        assert!(Instant::now() < deadline, "the lease was never granted");
        std::thread::sleep(Duration::from_millis(20));
    }
    holder
}

/// Runs `fleet upgrade` on `master` in the background.
fn upgrade_in_background(
    master: &TestMember,
    args: &[&str],
) -> std::thread::JoinHandle<std::process::Output> {
    let home = master.home.path().to_owned();
    let mut full = vec!["fleet".to_owned(), "upgrade".to_owned()];
    full.extend(args.iter().map(|a| (*a).to_owned()));
    std::thread::spawn(move || {
        crate::processes::command(BUILD)
            .env("HOME", home)
            .args(full)
            .output()
            .unwrap()
    })
}

#[test]
fn tampered_unsigned_and_foreign_builds_are_refused_and_change_nothing() {
    let directory = builds();
    let older = variant(directory.path(), "older");
    let alpha = TestMember::install("alpha", Path::new(BUILD));
    let bravo = TestMember::install("bravo", &older);
    enrol(&alpha, &bravo);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let genuine = std::fs::read(BUILD).unwrap();
    let before = std::fs::read(bravo.data().join("elsecall")).unwrap();
    let pid = bravo.installed_pid().unwrap();

    // Content altered after the master signed its digest: one flipped byte.
    let mut tampered = genuine.clone();
    let middle = tampered.len() / 2;
    tampered[middle] ^= 0xff;
    let signed = manifest(&bravo, &genuine).sign(&alpha.key());
    assert_eq!(
        refusal(offer(&runtime, &alpha, &bravo, signed, &tampered)),
        "digest_mismatch"
    );

    // A manifest the master did not sign, sent over the master's own channel.
    let signed = manifest(&bravo, &genuine).sign(&bravo.key());
    assert_eq!(
        refusal(offer(&runtime, &alpha, &bravo, signed, &genuine)),
        "manifest_unverified"
    );

    // A manifest signed for another member.
    let mut other = manifest(&bravo, &genuine);
    other.recipient = alpha.as_member().key;
    assert_eq!(
        refusal(offer(
            &runtime,
            &alpha,
            &bravo,
            other.sign(&alpha.key()),
            &genuine
        )),
        "manifest_unverified"
    );

    // A manifest issued long ago, and one claiming a build beyond the size limit,
    // are refused before any content.
    let mut stale = manifest(&bravo, &genuine);
    stale.issued_at -= 3600;
    assert_eq!(
        refusal(offer(
            &runtime,
            &alpha,
            &bravo,
            stale.sign(&alpha.key()),
            &genuine
        )),
        "manifest_expired"
    );
    let mut huge = manifest(&bravo, &genuine);
    huge.size = protocol::MAXIMUM_EXECUTABLE + 1;
    assert_eq!(
        refusal(offer(
            &runtime,
            &alpha,
            &bravo,
            huge.sign(&alpha.key()),
            &genuine
        )),
        "build_too_large"
    );

    // A build declared for another architecture is refused before any content.
    let foreign = match Architecture::this().unwrap() {
        Architecture::Aarch64 => Architecture::X86_64,
        Architecture::X86_64 => Architecture::Aarch64,
    };
    let mut declared = manifest(&bravo, &genuine);
    declared.architectures = vec![foreign];
    assert_eq!(
        refusal(offer(
            &runtime,
            &alpha,
            &bravo,
            declared.sign(&alpha.key()),
            &genuine
        )),
        "wrong_platform"
    );

    // Content that is not an executable for this machine, whatever it declares:
    // a script, and a Mach-O header for the other architecture.
    let script = b"#!/bin/sh\nexit 0\n".to_vec();
    let signed = manifest(&bravo, &script).sign(&alpha.key());
    assert_eq!(
        refusal(offer(&runtime, &alpha, &bravo, signed, &script)),
        "wrong_platform"
    );
    let mut header = Vec::new();
    header.extend_from_slice(&0xfeed_facfu32.to_le_bytes());
    let cpu: u32 = match foreign {
        Architecture::Aarch64 => 0x0100_000c,
        Architecture::X86_64 => 0x0100_0007,
    };
    header.extend_from_slice(&cpu.to_le_bytes());
    header.extend_from_slice(&0u32.to_le_bytes());
    header.extend_from_slice(&2u32.to_le_bytes());
    header.resize(4096, 0);
    let signed = manifest(&bravo, &header).sign(&alpha.key());
    assert_eq!(
        refusal(offer(&runtime, &alpha, &bravo, signed, &header)),
        "wrong_platform"
    );

    // Nothing changed: the same executable, the same process, nothing staged.
    assert_eq!(
        std::fs::read(bravo.data().join("elsecall")).unwrap(),
        before
    );
    assert_eq!(bravo.installed_pid().unwrap(), pid);
    assert_eq!(health(&bravo)["build"], build_of(&older));
    assert!(!bravo.data().join("upgrade/incoming").exists());

    // The genuine build still goes through afterwards.
    let (code, report) = upgrade(&alpha, &[]);
    assert_eq!(code, 0, "{report}");
    assert_eq!(outcome(&report, "bravo")["outcome"], "upgraded");
}

#[test]
fn a_build_that_never_becomes_ready_is_rolled_back_to_the_previous_one() {
    let directory = builds();
    // A real executable for this machine that exits at once instead of serving.
    let broken = directory.path().join("broken");
    std::fs::copy("/usr/bin/false", &broken).unwrap();
    let alpha = TestMember::install("alpha", Path::new(BUILD));
    let bravo = TestMember::install("bravo", Path::new(BUILD));
    enrol(&alpha, &bravo);
    let build = build_of(Path::new(BUILD));

    let started = Instant::now();
    let (code, report) = upgrade(
        &alpha,
        &[
            "--member",
            "bravo",
            "--executable",
            broken.to_str().unwrap(),
        ],
    );
    assert_eq!(code, 1, "{report}");
    assert_eq!(report["data"]["complete"], false);
    assert_eq!(report["data"]["distributed"]["build"], build_of(&broken));
    let bravo_outcome = outcome(&report, "bravo");
    assert_eq!(bravo_outcome["outcome"], "rolled_back", "{report}");
    assert_eq!(bravo_outcome["error"]["code"], "startup_failed");
    assert_eq!(bravo_outcome["running"]["build"], build);
    assert!(started.elapsed() < Duration::from_secs(90));

    // The member serves its previous build from the same installation.
    assert_eq!(health(&bravo)["build"], build);
    assert_eq!(
        std::fs::read(bravo.data().join("elsecall")).unwrap(),
        std::fs::read(BUILD).unwrap()
    );
    assert_eq!(alpha.view_of("bravo")["reachability"], "reachable");
    assert!(alpha.fleet()["version_skew"].is_null());
    assert!(!bravo.data().join("upgrade/incoming").exists());
    assert!(!bravo.data().join("upgrade/previous").exists());
    let log = bravo.log();
    assert!(log.contains("upgrade_rolling_back"), "{log}");
}

#[test]
fn a_members_runs_and_leases_are_waited_for_or_the_upgrade_is_deferred() {
    let directory = builds();
    let older = variant(directory.path(), "older");
    let alpha = TestMember::install("alpha", Path::new(BUILD));
    let bravo = TestMember::install("bravo", &older);
    enrol(&alpha, &bravo);
    let mut holder = hold_lease(&bravo, "nightly-build");

    // With a short wait the upgrade is deferred, and the lease is untouched.
    let pid = bravo.installed_pid().unwrap();
    let (code, report) = upgrade(&alpha, &["--wait", "1s"]);
    assert_eq!(code, 1, "{report}");
    let deferred = outcome(&report, "bravo");
    assert_eq!(deferred["outcome"], "deferred", "{report}");
    assert_eq!(deferred["in_flight"][0]["kind"], "lease");
    assert_eq!(deferred["in_flight"][0]["label"], "nightly-build");
    assert_eq!(deferred["in_flight"][0]["state"], "running");
    assert!(deferred["hint"].as_str().unwrap().contains("--wait"));
    assert_eq!(bravo.installed_pid().unwrap(), pid);
    assert_eq!(health(&bravo)["build"], build_of(&older));
    let runs = bravo.ok(&["runs"]);
    assert_eq!(
        labelled(&runs, "nightly-build")["state"],
        "running",
        "{runs}"
    );

    // With a longer wait the member waits for the lease, then upgrades.
    let master = upgrade_in_background(&alpha, &["--wait", "60s"]);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    until_draining(&runtime, &alpha, &bravo, "nightly-build");
    // A second upgrade meanwhile is refused, and changes nothing.
    let genuine = std::fs::read(BUILD).unwrap();
    let signed = manifest(&bravo, &genuine).sign(&alpha.key());
    assert_eq!(
        refusal(offer(&runtime, &alpha, &bravo, signed, &genuine)),
        "upgrade_in_progress"
    );
    assert_eq!(bravo.installed_pid().unwrap(), pid, "nothing restarted yet");
    std::fs::write(bravo.home.path().join("release"), "").unwrap();
    assert!(holder.wait().unwrap().success());
    let output = master.join().unwrap();
    let report = json(&output);
    assert!(output.status.success(), "{report}");
    assert_eq!(outcome(&report, "bravo")["outcome"], "upgraded");
    assert_eq!(health(&bravo)["build"], build_of(Path::new(BUILD)));
    let runs = bravo.ok(&["runs"]);
    assert_eq!(
        labelled(&runs, "nightly-build")["outcome"]["kind"],
        "released",
        "{runs}"
    );
}

/// The newest record labelled `label` in `runs` output. Elsecall's own passes may
/// have been recorded since, so the newest record need not be the test's.
fn labelled<'a>(runs: &'a Value, label: &str) -> &'a Value {
    runs["runs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["label"] == label)
        .unwrap_or_else(|| panic!("no {label} record: {runs}"))
}

/// A process killed when dropped, even when an assertion fails first.
struct Killed(std::process::Child);
impl Drop for Killed {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Waits until `member` records a request labelled `label` in `state`.
fn until_recorded(member: &TestMember, label: &str, state: &str) {
    let deadline = Instant::now() + crate::processes::WAIT;
    loop {
        let runs = member.ok(&["runs"]);
        if runs["runs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["label"] == label && r["state"] == state)
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "no {state} {label} request: {runs}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_member_under_pressure_is_not_held_back_by_its_own_queued_housekeeping() {
    let directory = builds();
    let older = variant(directory.path(), "older");
    let alpha = TestMember::install("alpha", Path::new(BUILD));
    // Memory pressure closes bravo's admission gate, so nothing starts there. Its
    // housekeeping asks for room every two seconds and, promoted at its deadline,
    // stays queued, as it would for hours on a member under sustained pressure.
    let load = directory.path().join("bravo-load.json");
    std::fs::write(&load, r#"{"memory_pressure":"warning"}"#).unwrap();
    let scripted = format!("file:{}", load.display());
    let bravo = TestMember::install_with(
        "bravo",
        &older,
        "[housekeeping]\ninterval = \"2s\"\nat_deadline = \"start\"\n",
        &[("ELSECALL_MACHINE_LOAD", &scripted)],
    );
    enrol(&alpha, &bravo);
    until_recorded(&bravo, "elsecall housekeeping", "queued");

    // An operator's request queued behind the same gate is waited for: the upgrade
    // defers, and lists only that request.
    let waiting = Killed(
        crate::processes::command(BUILD)
            .env("HOME", bravo.home.path())
            .args([
                "lease",
                "acquire",
                "--size",
                "1",
                "--label",
                "nightly-build",
            ])
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    until_recorded(&bravo, "nightly-build", "queued");
    let pid = bravo.installed_pid().unwrap();
    let (code, report) = upgrade(&alpha, &["--wait", "1s"]);
    assert_eq!(code, 1, "{report}");
    let deferred = outcome(&report, "bravo");
    assert_eq!(deferred["outcome"], "deferred", "{report}");
    let in_flight = deferred["in_flight"].as_array().unwrap();
    assert_eq!(in_flight.len(), 1, "{report}");
    assert_eq!(in_flight[0]["label"], "nightly-build");
    assert_eq!(in_flight[0]["state"], "queued");
    assert_eq!(in_flight[0]["internal"], false);
    assert_eq!(bravo.installed_pid().unwrap(), pid);
    assert_eq!(health(&bravo)["build"], build_of(&older));

    // Once the operator gives up, only Elsecall's own queued housekeeping is left,
    // and the member upgrades without waiting for it.
    drop(waiting);
    until_recorded(&bravo, "nightly-build", "ended");
    until_recorded(&bravo, "elsecall housekeeping", "queued");
    let (code, report) = upgrade(&alpha, &["--wait", "30s"]);
    assert_eq!(code, 0, "{report}");
    assert_eq!(outcome(&report, "bravo")["outcome"], "upgraded", "{report}");
    assert_eq!(health(&bravo)["build"], build_of(Path::new(BUILD)));
}

#[test]
fn an_attempt_that_cannot_finish_is_closed_and_the_member_keeps_working() {
    let directory = builds();
    let older = variant(directory.path(), "older");
    let alpha = TestMember::install("alpha", Path::new(BUILD));
    let mut bravo = TestMember::install("bravo", &older);
    enrol(&alpha, &bravo);
    let runtime = tokio::runtime::Runtime::new().unwrap();

    // The service restarts while it waits for a lease: the attempt is interrupted,
    // nothing is installed, and the lease survives the restart.
    let mut holder = hold_lease(&bravo, "long-test");
    let master = upgrade_in_background(&alpha, &["--wait", "60s"]);
    until_draining(&runtime, &alpha, &bravo, "long-test");
    bravo.restart();
    let output = master.join().unwrap();
    let report = json(&output);
    assert_eq!(output.status.code(), Some(1), "{report}");
    let interrupted = outcome(&report, "bravo");
    assert_eq!(interrupted["outcome"], "interrupted", "{report}");
    assert_eq!(interrupted["error"]["code"], "service_restarted");
    assert_eq!(health(&bravo)["build"], build_of(&older));
    assert!(!bravo.data().join("upgrade/incoming").exists());
    let runs = bravo.ok(&["runs"]);
    assert_eq!(labelled(&runs, "long-test")["state"], "running", "{runs}");
    std::fs::write(bravo.home.path().join("release"), "").unwrap();
    assert!(holder.wait().unwrap().success());

    // An install refused before anything stopped (another install holds the
    // installation lock) leaves the service running and the member upgradable.
    let pid = bravo.installed_pid().unwrap();
    let lock = elsecall::private_files::try_lock(&bravo.data().join("installation.lock"))
        .unwrap()
        .expect("no install runs");
    let (code, report) = upgrade(&alpha, &[]);
    assert_eq!(code, 1, "{report}");
    let refused = outcome(&report, "bravo");
    assert_eq!(refused["outcome"], "interrupted", "{report}");
    assert_eq!(refused["error"]["code"], "operation_in_progress");
    assert_eq!(bravo.installed_pid().unwrap(), pid);
    assert_eq!(health(&bravo)["build"], build_of(&older));
    drop(lock);
    let (code, report) = upgrade(&alpha, &[]);
    assert_eq!(code, 0, "{report}");
    assert_eq!(outcome(&report, "bravo")["outcome"], "upgraded");
}
