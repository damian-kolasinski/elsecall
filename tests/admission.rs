#![cfg(target_os = "macos")]
//! Admission and leases through the real CLI, service, socket and process
//! watcher. Only machine load is faked.
mod processes;
mod support;

use elsecall::inspection::MemoryPressure;
use std::time::{Duration, Instant};
use support::{Fixture, in_use, queued, record};

/// A script that takes a lease, logs NAME, and holds it until $DIR/release exists.
fn holder(name: &str, options: &str) -> String {
    format!(
        r#""$EC" lease acquire --label {name} {options} > "$DIR/{name}.json" || exit 9
echo {name} >> "$DIR/order"
while [ ! -f "$DIR/release" ]; do sleep 0.02; done"#
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_requests_never_overcommit() {
    let mut fx = Fixture::start(2).await;
    let pids: Vec<u32> = (0..6)
        .map(|_| {
            fx.script(
                r#""$EC" lease acquire --label concurrent >/dev/null || exit 9
echo start >> "$DIR/log"
sleep 0.3
echo end >> "$DIR/log""#,
            )
        })
        .collect();
    for pid in pids {
        assert_eq!(fx.exit_code(pid).await, 0);
    }
    let log = std::fs::read_to_string(fx.path("log")).unwrap();
    let (mut running, mut most) = (0, 0);
    for line in log.lines() {
        running += if line == "start" { 1 } else { -1 };
        most = most.max(running);
    }
    assert!(
        (1..=2).contains(&most),
        "capacity 2 is never exceeded:\n{log}"
    );
    let runs = fx.until(|r| in_use(r) == 0.0).await;
    let records = runs["runs"].as_array().unwrap();
    assert_eq!(records.len(), 6);
    assert!(
        records
            .iter()
            .all(|r| r["outcome"]["kind"] == "holder_exited")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interactive_work_goes_first_in_arrival_order_then_deferrable() {
    let mut fx = Fixture::start(1).await;
    fx.script(
        r#""$EC" lease acquire --label first >/dev/null
echo first >> "$DIR/order"
while [ ! -f "$DIR/go" ]; do sleep 0.02; done"#,
    );
    fx.until(|r| in_use(r) == 1.0).await;
    let later = r#"; sleep 0.1"#;
    fx.script(&format!(
        r#""$EC" lease acquire --label deferred --deferrable --deadline 1h --at-deadline start >/dev/null
echo deferred >> "$DIR/order"{later}"#
    ));
    fx.until(|r| queued(r, "deferrable") == 1).await;
    for (n, name) in ["second", "third"].iter().enumerate() {
        fx.script(&format!(
            r#""$EC" lease acquire --label {name} >/dev/null
echo {name} >> "$DIR/order"{later}"#
        ));
        fx.until(|r| queued(r, "interactive") == n as u64 + 1).await;
    }
    fx.touch("go");
    fx.until(|r| in_use(r) == 0.0 && queued(r, "deferrable") == 0)
        .await;
    assert_eq!(
        std::fs::read_to_string(fx.path("order")).unwrap(),
        "first\nsecond\nthird\ndeferred\n"
    );
    let runs = fx.runs().await;
    let deferred = record(&runs, "deferred");
    let (code, explained) = fx
        .cli(&["explain", deferred["run_id"].as_str().unwrap()])
        .await;
    assert_eq!(code, 0);
    assert_eq!(explained["data"]["decision"]["verdict"], "started");
    assert_eq!(
        explained["data"]["decision"]["reason"],
        "Capacity was free and no interactive work waited."
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn busy_member_fails_after_the_hold_limit_with_a_retry_hint() {
    let mut fx = Fixture::start(1).await;
    fx.script(&holder("holding", ""));
    fx.wait_for_file("order").await;
    let started = Instant::now();
    let (code, busy) = fx
        .cli(&[
            "lease",
            "acquire",
            "--label",
            "waiter",
            "--hold-limit",
            "1s",
        ])
        .await;
    assert!(started.elapsed() >= Duration::from_secs(1));
    assert_eq!(code, 1);
    let error = &busy["error"];
    assert_eq!(error["code"], "fleet_busy");
    assert!(error["hint"].as_str().unwrap().contains("Retry in about"));
    assert!(error["details"]["retry_after_seconds"].as_u64().unwrap() > 0);
    assert_eq!(error["details"]["running"][0]["label"], "holding");
    let (_, explained) = fx
        .cli(&["explain", error["run_id"].as_str().unwrap()])
        .await;
    let record = &explained["data"];
    assert_eq!(record["outcome"]["code"], "fleet_busy");
    assert_eq!(record["hold_limit"]["seconds"], 1.0);
    assert_eq!(record["hold_limit"]["basis"], "requested");
    assert_eq!(record["decision"]["verdict"], "fleet_busy");
    let candidate = &record["decision"]["candidates"][0];
    assert_eq!(candidate["member"], "test-member");
    assert_eq!(candidate["free"], 0.0);

    let (code, large) = fx.cli(&["lease", "acquire", "--size", "1.5"]).await;
    assert_eq!(code, 1);
    assert_eq!(large["error"]["code"], "request_too_large");
    assert!(
        large["error"]["run_id"].is_string(),
        "too large is recorded"
    );

    let (code, invalid) = fx.cli(&["lease", "acquire", "--size", "0"]).await;
    assert_eq!(code, 2);
    assert_eq!(invalid["error"]["code"], "invalid_request");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn outside_load_and_memory_pressure_admit_nothing_new() {
    let fx = Fixture::start(4).await;
    fx.set_load(|load| load.cpu_utilization = Some(0.97));
    fx.until(|r| r["member"]["gate"]["verdict"] == "saturated")
        .await;
    let (code, busy) = fx.cli(&["lease", "acquire", "--hold-limit", "1s"]).await;
    assert_eq!(code, 1);
    assert!(
        busy["error"]["message"]
            .as_str()
            .unwrap()
            .contains("saturated by outside load"),
        "{busy}"
    );
    fx.set_load(|load| {
        load.cpu_utilization = Some(0.0);
        load.memory_pressure = MemoryPressure::Warning;
    });
    fx.until(|r| r["member"]["gate"]["verdict"] == "memory_pressure")
        .await;
    let (code, busy) = fx.cli(&["lease", "acquire", "--hold-limit", "1s"]).await;
    assert_eq!(code, 1);
    assert!(
        busy["error"]["message"]
            .as_str()
            .unwrap()
            .contains("memory pressure")
    );
    fx.set_load(|load| load.memory_pressure = MemoryPressure::Normal);
    fx.until(|r| r["member"]["gate"]["verdict"] == "open").await;
    let (code, granted) = fx.cli(&["lease", "acquire", "--hold-limit", "1s"]).await;
    assert_eq!(code, 0, "{granted}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deferrable_work_starts_ahead_or_skips_at_its_deadline() {
    let mut fx = Fixture::start(1).await;
    fx.script(&holder("holding", ""));
    fx.wait_for_file("order").await;

    let started = Instant::now();
    let (code, skipped) = fx
        .cli(&[
            "lease",
            "acquire",
            "--deferrable",
            "--deadline",
            "1s",
            "--at-deadline",
            "skip",
        ])
        .await;
    assert!(started.elapsed() >= Duration::from_secs(1));
    assert_eq!(code, 1);
    assert_eq!(skipped["error"]["code"], "deadline_skipped");

    fx.script(&holder("interactive", ""));
    fx.until(|r| queued(r, "interactive") == 1).await;
    fx.script(&holder(
        "deadline",
        "--deferrable --deadline 1s --at-deadline start",
    ));
    // After the deadline the deferrable request is first in line, but capacity
    // is still held, so it keeps waiting rather than overcommitting.
    fx.until(|r| queued(r, "interactive") == 2 && queued(r, "deferrable") == 0)
        .await;
    assert_eq!(in_use(&fx.runs().await), 1.0);
    fx.touch("release");
    fx.until(|r| record(r, "interactive")["state"] != "queued")
        .await;
    // The script logs its start just after its grant, so wait for the log.
    let deadline = Instant::now() + crate::processes::WAIT;
    let order = loop {
        let order = std::fs::read_to_string(fx.path("order")).unwrap();
        if order.lines().count() == 3 || Instant::now() > deadline {
            break order;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(order, "holding\ndeadline\ninteractive\n");
    let runs = fx.runs().await;
    let (_, explained) = fx
        .cli(&[
            "explain",
            record(&runs, "deadline")["run_id"].as_str().unwrap(),
        ])
        .await;
    assert_eq!(
        explained["data"]["decision"]["reason"],
        "Deadline passed; started ahead of interactive work once it fit."
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn leases_end_on_release_holder_exit_and_sigkill() {
    let mut fx = Fixture::start(1).await;
    // Explicit release while the holder keeps running.
    let released = fx.script(
        r#""$EC" lease acquire --label released >/dev/null || exit 9
"$EC" lease release > "$DIR/release.json" || exit 8
while [ ! -f "$DIR/go" ]; do sleep 0.02; done"#,
    );
    fx.wait_for_file("release.json").await;
    let runs = fx.until(|r| in_use(r) == 0.0).await;
    assert_eq!(record(&runs, "released")["outcome"]["kind"], "released");
    fx.touch("go");
    assert_eq!(fx.exit_code(released).await, 0);

    // SIGKILL of the holder releases at once, though its child keeps running.
    let killed = fx.script(&holder("killed", ""));
    fx.wait_for_file("order").await;
    assert_eq!(in_use(&fx.runs().await), 1.0);
    // SAFETY: kill has no memory-safety preconditions.
    assert_eq!(unsafe { libc::kill(killed as i32, libc::SIGKILL) }, 0);
    let runs = fx.until(|r| in_use(r) == 0.0).await;
    assert_eq!(record(&runs, "killed")["outcome"]["kind"], "holder_exited");

    // A caller killed while waiting leaves no grant behind.
    let blocker = fx.script(&holder("blocker", ""));
    fx.until(|r| in_use(r) == 1.0).await;
    let waiter = fx.script(r#"exec "$EC" lease acquire --label abandoned"#);
    fx.until(|r| queued(r, "interactive") == 1).await;
    // SAFETY: as above.
    unsafe { libc::kill(waiter as i32, libc::SIGKILL) };
    let runs = fx.until(|r| queued(r, "interactive") == 0).await;
    assert_eq!(record(&runs, "abandoned")["outcome"]["kind"], "cancelled");
    let _ = blocker;
    fx.touch("release");
    fx.until(|r| in_use(r) == 0.0).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nested_requests_reuse_the_outer_grant_without_queueing() {
    let mut fx = Fixture::start(1).await;
    // The inner shell runs a second command, so it does not exec elsecall and
    // stays the inner lease's holder: a descendant, not the outer holder itself.
    fx.script(
        r#""$EC" lease acquire --label outer > "$DIR/outer.json" || exit 9
while [ ! -f "$DIR/go" ]; do sleep 0.02; done
sh -c '"$EC" lease acquire --label inner; exit $?' > "$DIR/inner.json"
echo done >> "$DIR/done"
while [ ! -f "$DIR/release" ]; do sleep 0.02; done"#,
    );
    fx.until(|r| in_use(r) == 1.0).await;
    fx.script(r#""$EC" lease acquire --label outsider >/dev/null"#);
    fx.until(|r| queued(r, "interactive") == 1).await;
    fx.touch("go");
    fx.wait_for_file("done").await;
    let outer: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(fx.path("outer.json")).unwrap()).unwrap();
    let inner: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(fx.path("inner.json")).unwrap()).unwrap();
    assert_eq!(inner["data"]["nested_in"], outer["data"]["run_id"]);
    let runs = fx.runs().await;
    assert_eq!(in_use(&runs), 1.0, "nested work uses no extra capacity");
    assert_eq!(queued(&runs, "interactive"), 1, "the outsider still waits");
    assert_eq!(record(&runs, "outsider")["state"], "queued");
    fx.touch("release");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grants_and_nested_families_survive_a_service_restart() {
    let mut fx = Fixture::start(1).await;
    // The outer holder waits for "go", then exits; the nested holder lives on.
    let outer = fx.script(
        r#""$EC" lease acquire --label outer >/dev/null || exit 9
sh -c '"$EC" lease acquire --label inner >/dev/null; echo inner >> "$DIR/order"; while [ ! -f "$DIR/release" ]; do sleep 0.02; done' &
while [ ! -f "$DIR/go" ]; do sleep 0.02; done"#,
    );
    fx.wait_for_file("order").await;
    fx.stop().await;
    fx.serve().await;
    assert_eq!(
        in_use(&fx.runs().await),
        1.0,
        "surviving grants still hold capacity"
    );
    let (code, _) = fx.cli(&["lease", "acquire", "--hold-limit", "1s"]).await;
    assert_eq!(code, 1, "no overcommit after restart");
    fx.touch("go");
    assert_eq!(fx.exit_code(outer).await, 0);
    let runs = fx.until(|r| record(r, "outer")["state"] == "ended").await;
    assert_eq!(
        in_use(&runs),
        1.0,
        "the nested lease keeps its family's capacity"
    );
    fx.touch("release");
    let runs = fx.until(|r| in_use(r) == 0.0).await;
    assert_eq!(record(&runs, "inner")["outcome"]["kind"], "holder_exited");

    use std::os::unix::fs::PermissionsExt;
    let records = fx.paths.records();
    for file in [
        records.clone(),
        records.with_extension("sqlite-wal"),
        records.with_extension("sqlite-shm"),
    ] {
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600,
            "{}",
            file.display()
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_caller_that_disconnects_while_waiting_is_withdrawn() {
    use elsecall::{
        client::{self, Wait},
        scheduling::{
            messages::Grant,
            request::{AdmissionRequest, Kind, Placement, Urgency},
        },
    };
    let mut fx = Fixture::start(1).await;
    fx.script(&holder("holding", ""));
    fx.wait_for_file("order").await;
    // The caller stays alive and only drops its connection, so nothing but the
    // dropped request can withdraw it.
    let request = AdmissionRequest {
        workload: None,
        kind: Kind::Lease,
        // SAFETY: getppid has no preconditions.
        holder_pid: Some(unsafe { libc::getppid() } as u32),
        size: None,
        label: Some("disconnected".into()),
        placement: Placement::Pinned,
        urgency: Urgency::Interactive {
            hold_limit_ms: None,
        },
        workspace: None,
        settings: None,
    };
    let socket = fx.paths.socket();
    {
        let waiting =
            client::post::<Grant, _>(&socket, "/v1/admissions", &request, Wait::Unbounded);
        tokio::pin!(waiting);
        tokio::select! {
            result = &mut waiting => panic!("a queued request completed: {result:?}"),
            _ = fx.until(|r| queued(r, "interactive") == 1) => (),
        }
        // Drop the connected future only after proving the server received it.
    }
    let runs = fx.until(|r| queued(r, "interactive") == 0).await;
    assert_eq!(
        record(&runs, "disconnected")["outcome"]["reason"],
        "The caller disconnected while waiting."
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_run_is_bound_to_its_caller_and_command_until_it_reports_the_end() {
    use elsecall::{
        client::{self, Wait},
        scheduling::{
            messages::{CommandStarted, Grant, RunEnded},
            request::{AdmissionRequest, Kind, Placement, Urgency},
        },
    };
    let fx = Fixture::start(1).await;
    let socket = fx.paths.socket();
    let request = AdmissionRequest {
        workload: None,
        kind: Kind::Run,
        holder_pid: None,
        size: None,
        label: Some("direct".into()),
        placement: Placement::Pinned,
        urgency: Urgency::Interactive {
            hold_limit_ms: None,
        },
        workspace: None,
        settings: None,
    };
    let grant: Grant = client::post(&socket, "/v1/admissions", &request, Wait::Unbounded)
        .await
        .unwrap();
    let command = |pid| CommandStarted { pid };
    let route = |action: &str| format!("/v1/runs/{}/{action}", grant.run_id);
    // SAFETY: getppid has no preconditions.
    let not_a_child = unsafe { libc::getppid() } as u32;
    let refused = client::post::<(), _>(
        &socket,
        &route("command"),
        &command(not_a_child),
        Wait::Bounded,
    )
    .await
    .unwrap_err();
    assert_eq!(refused.code, "invalid_request");
    let mut child = crate::processes::command("/bin/sleep")
        .arg("30")
        .spawn()
        .unwrap();
    client::post::<(), _>(
        &socket,
        &route("command"),
        &command(child.id()),
        Wait::Bounded,
    )
    .await
    .unwrap();
    let (_, explained) = fx.cli(&["explain", grant.run_id.as_str()]).await;
    assert_eq!(explained["data"]["processes"].as_array().unwrap().len(), 2);
    client::post::<(), _>(
        &socket,
        &route("end"),
        &RunEnded::Exited {
            plan_observation: None,
            exit_code: 4,
            cpu_ms: None,
            run_phases: None,
        },
        Wait::Bounded,
    )
    .await
    .unwrap();
    let ended = client::post::<(), _>(
        &socket,
        &route("end"),
        &RunEnded::Exited {
            plan_observation: None,
            exit_code: 0,
            cpu_ms: None,
            run_phases: None,
        },
        Wait::Bounded,
    )
    .await
    .unwrap_err();
    assert_eq!(ended.code, "not_found", "a run ends once");
    let (_, explained) = fx.cli(&["explain", grant.run_id.as_str()]).await;
    assert_eq!(explained["data"]["outcome"]["exit_code"], 4);
    assert_eq!(in_use(&fx.runs().await), 0.0);
    child.kill().unwrap();
    child.wait().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn elsecalls_own_grants_do_not_count_as_outside_load() {
    let mut fx = Fixture::start(2).await;
    fx.script(&holder("own", ""));
    fx.wait_for_file("order").await;
    fx.set_load(|load| load.cpu_utilization = Some(0.95));
    fx.wait_for_load_window().await;
    let runs = fx.runs().await;
    assert_eq!(
        runs["member"]["gate"]["verdict"], "open",
        "half the machine is Elsecall's own grant: {runs}"
    );
    let (code, granted) = fx.cli(&["lease", "acquire", "--hold-limit", "1s"]).await;
    assert_eq!(code, 0, "{granted}");
    let (code, _) = fx
        .cli(&[
            "lease",
            "release",
            granted["data"]["run_id"].as_str().unwrap(),
        ])
        .await;
    assert_eq!(code, 0);
    fx.touch("release");
    fx.until(|r| r["member"]["gate"]["verdict"] == "saturated")
        .await;
}
