#![cfg(target_os = "macos")]
//! Public CLI/socket lifecycle. Only machine observation is faked.
mod processes;
mod support;
use elsecall::{
    client::{self, Wait},
    scheduling::{
        messages::{CommandStarted, Grant, RunEnded},
        record::Record,
        request::{AdmissionRequest, Kind, Placement, Urgency},
        timestamp::Timestamp,
    },
    workload_metadata::{Observation, Workload},
};
use serde_json::{Value, json};
use std::os::unix::process::ExitStatusExt;
use std::{process::Output, time::Duration};
use support::Fixture;

fn plan(key: char, stop: &str) -> Value {
    json!({"kind":"sequential", "planned_units":3, "stop_policy":stop,
        "estimate_key":key.to_string().repeat(64), "variant_key":"b".repeat(64)})
}
fn coverage(attempted: u32, terminal: u32, failed: u32) -> Value {
    json!({"attempted_units":attempted,"terminal_units":terminal,"failed_units":failed,"elapsed_ms":12})
}
fn all(failed: u32) -> Value {
    json!({"status":"all_units_observed","coverage":coverage(3,3,failed)})
}
fn observe(value: &Value) -> String {
    format!(
        "'{}' workload observe --observation '{}'",
        env!("CARGO_BIN_EXE_elsecall"),
        value
    )
}
async fn run(fx: &Fixture, label: &str, workload: Option<&Value>, body: &str) -> Output {
    let mut command = processes::async_command(env!("CARGO_BIN_EXE_elsecall"));
    command
        .env("HOME", fx.home.path())
        .current_dir(fx.home.path())
        .args(["run", "--pinned", "--label", label]);
    if let Some(workload) = workload {
        command.args(["--workload", &workload.to_string()]);
    }
    command
        .args(["--", "/bin/sh", "-c", body])
        .output()
        .await
        .unwrap()
}
async fn explained(fx: &Fixture, label: &str) -> (Value, Record) {
    let runs = fx.runs().await;
    let summary = support::record(&runs, label);
    let (_, value) = fx
        .cli(&["explain", summary["run_id"].as_str().unwrap()])
        .await;
    let record: Record = serde_json::from_value(value["data"].clone()).unwrap();
    assert_eq!(summary["workload"], value["data"]["workload"]);
    assert_eq!(
        summary["plan_observation"],
        value["data"]["plan_observation"]
    );
    (value["data"].clone(), record)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn declared_operations_round_trip_without_replacing_process_outcomes() {
    let mut fx = Fixture::start(2).await;
    let cases = [
        (
            "success",
            "continue_all",
            Some(all(0)),
            "exit 0",
            "all_units_observed",
        ),
        (
            "failed-full",
            "continue_all",
            Some(all(2)),
            "exit 1",
            "all_units_observed",
        ),
        (
            "early-stop",
            "stop_on_failure",
            Some(json!({"status":"prefix_observed","coverage":coverage(1,1,1),"reason":"failure"})),
            "exit 1",
            "prefix_observed",
        ),
        (
            "timeout",
            "continue_all",
            Some(json!({"status":"censored","coverage":coverage(2,1,0),"reason":"timeout"})),
            "exit 124",
            "censored",
        ),
        ("missing", "continue_all", None, "exit 0", "unknown"),
        (
            "unknown",
            "continue_all",
            Some(json!({"status":"unknown"})),
            "exit 1",
            "unknown",
        ),
        ("signal", "continue_all", None, "kill -TERM $$", "censored"),
        (
            "observed-then-signal",
            "continue_all",
            Some(all(1)),
            "kill -TERM $$",
            "all_units_observed",
        ),
    ];
    for (label, stop, assertion, end, status) in cases {
        let body = assertion
            .map(|a| format!("{} || exit 99; {end}", observe(&a)))
            .unwrap_or(end.into());
        let output = run(&fx, label, Some(&plan('a', stop)), &body).await;
        if end.contains("kill") {
            assert_eq!(output.status.signal(), Some(libc::SIGTERM), "{output:?}");
        } else {
            assert_eq!(
                output.status.code(),
                Some(if end == "exit 0" {
                    0
                } else if end == "exit 124" {
                    124
                } else {
                    1
                }),
                "{output:?}"
            );
        }
        assert!(
            !String::from_utf8_lossy(&output.stderr).contains("error"),
            "{output:?}"
        );
        let (record, typed) = explained(&fx, label).await;
        assert_eq!(record["plan_observation"]["status"], status, "{output:?}");
        assert_eq!(
            record["outcome"]["kind"],
            if end.contains("kill") {
                "signaled"
            } else {
                "exited"
            }
        );
        assert!(record["run_phases"]["execution_ms"].is_number());
        let fact = elsecall::reporting::facts::Fact::of(&typed).unwrap();
        assert_eq!(fact.workload, typed.workload);
        assert_eq!(fact.plan_observation, typed.plan_observation);
        assert!(fact.believable());
    }
    // Reports survive persistence/recovery; old records remain absent/unknown.
    let output = run(&fx, "legacy", None, "exit 0").await;
    assert!(output.status.success());
    let (legacy, _) = explained(&fx, "legacy").await;
    assert!(legacy.get("workload").is_none());
    assert!(legacy.get("plan_observation").is_none());
    fx.stop().await;
    fx.serve().await;
    assert_eq!(
        explained(&fx, "failed-full").await.0["plan_observation"],
        all(2)
    );
    assert!(
        explained(&fx, "legacy")
            .await
            .0
            .get("plan_observation")
            .is_none()
    );
}

fn request(workload: Value) -> AdmissionRequest {
    serde_json::from_value(json!({"kind":"run","label":"same display", "placement":"pinned", "urgency":{"interactive":{"hold_limit_ms":1000}}, "workload":workload})).unwrap()
}
async fn admitted(fx: &Fixture, workload: Value) -> Grant {
    client::post(
        &fx.paths.socket(),
        "/v1/admissions",
        &request(workload),
        Wait::Bounded,
    )
    .await
    .unwrap()
}
async fn end(fx: &Fixture, grant: &Grant) {
    client::post::<(), _>(
        &fx.paths.socket(),
        &format!("/v1/runs/{}/end", grant.run_id),
        &RunEnded::Exited {
            exit_code: 0,
            cpu_ms: None,
            run_phases: None,
            plan_observation: None,
        },
        Wait::Bounded,
    )
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fast_cli_report_waits_for_binding_and_rejects_invalid_cross_grant_and_late_writes() {
    let fx = Fixture::start(2).await;
    let grant = admitted(&fx, plan('a', "continue_all")).await;
    let other = admitted(&fx, plan('c', "continue_all")).await;
    let route = format!("/v1/runs/{}/observation", grant.run_id);
    let before = fx.cli(&["explain", grant.run_id.as_str()]).await.1;
    // The requester is not the executing command.
    assert!(
        client::post::<(), _>(&fx.paths.socket(), &route, &all(0), Wait::Bounded)
            .await
            .is_err()
    );
    assert_eq!(before, fx.cli(&["explain", grant.run_id.as_str()]).await.1);
    let body = format!(
        r#"
        {} > "$HOME/ok.json" || exit 92
        ELSECALL_RUN_ID={} {} > "$HOME/wrong.json"; test $? -eq 1 || exit 90
        {} > "$HOME/bad.json"; test $? -eq 2 || exit 91
        {} > "$HOME/duplicate.json"; test $? -eq 2 || exit 93
        "#,
        observe(&all(1)),
        other.run_id,
        observe(&all(0)),
        observe(&json!({"status":"all_units_observed","coverage":coverage(4,3,0)})),
        observe(&all(0))
    );
    let mut child = processes::async_command("/bin/sh")
        .args(["-c", &body])
        .env("HOME", fx.home.path())
        .env("ELSECALL_RUN_ID", grant.run_id.as_str())
        .env("ELSECALL_ADMISSION", "granted")
        .spawn()
        .unwrap();
    // Deterministically delay binding past the first public CLI observation.
    tokio::time::sleep(Duration::from_millis(150)).await;
    client::post::<(), _>(
        &fx.paths.socket(),
        &format!("/v1/runs/{}/command", grant.run_id),
        &CommandStarted {
            pid: child.id().unwrap(),
        },
        Wait::Bounded,
    )
    .await
    .unwrap();
    assert!(child.wait().await.unwrap().success());
    let (_, recorded) = explained(&fx, "same display").await;
    // The newest of these same-label records is the other grant; use IDs below.
    drop(recorded);
    let (_, first) = fx.cli(&["explain", grant.run_id.as_str()]).await;
    let (_, second) = fx.cli(&["explain", other.run_id.as_str()]).await;
    assert_eq!(first["data"]["plan_observation"], all(1));
    assert!(second["data"].get("plan_observation").is_none());
    assert_ne!(
        first["data"]["workload"]["estimate_key"],
        second["data"]["workload"]["estimate_key"]
    );
    end(&fx, &grant).await;
    let ended = fx.cli(&["explain", grant.run_id.as_str()]).await.1;
    assert!(
        client::post::<(), _>(&fx.paths.socket(), &route, &all(0), Wait::Bounded)
            .await
            .is_err()
    );
    assert_eq!(ended, fx.cli(&["explain", grant.run_id.as_str()]).await.1);
    end(&fx, &other).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn public_inputs_reject_unbounded_or_untyped_metadata_and_keep_legacy_wire_shape() {
    let fx = Fixture::start(2).await;
    for bad in [
        json!({"kind":"sequential","planned_units":0}),
        json!({"kind":"opaque","argv":"secret"}),
        {
            let mut p = plan('a', "continue_all");
            p["estimate_key"] = json!("a".repeat(2049));
            p
        },
        {
            let mut p = plan('a', "continue_all");
            p["planned_units"] = json!(1_000_001);
            p
        },
    ] {
        let payload = json!({"kind":"run","placement":"pinned","urgency":{"interactive":{"hold_limit_ms":0}},"workload":bad});
        let error = client::post::<Grant, _>(
            &fx.paths.socket(),
            "/v1/admissions",
            &payload,
            Wait::Bounded,
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, "invalid_request");
        assert!(!serde_json::to_string(&error).unwrap().contains("secret"));
    }
    assert!(fx.runs().await["runs"].as_array().unwrap().is_empty());
    let legacy = AdmissionRequest {
        kind: Kind::Run,
        holder_pid: None,
        size: None,
        label: None,
        placement: Placement::Pinned,
        urgency: Urgency::Interactive {
            hold_limit_ms: None,
        },
        workspace: None,
        settings: None,
        workload: None,
    };
    assert!(
        serde_json::to_value(&legacy)
            .unwrap()
            .get("workload")
            .is_none()
    );
    assert!(legacy.validate(Timestamp::now()).is_ok());
    let mut old = serde_json::to_value(request(plan('a', "continue_all"))).unwrap();
    old.as_object_mut().unwrap().remove("workload");
    assert!(
        serde_json::from_value::<AdmissionRequest>(old)
            .unwrap()
            .workload
            .is_none()
    );
    assert!(serde_json::from_value::<Workload>(plan('a', "continue_all")).is_ok());
    assert!(
        serde_json::from_value::<Observation>(
            json!({"status":"censored","coverage":null,"reason":"raw secret"})
        )
        .is_err()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_keeps_live_assertions_and_censors_unobserved_lost_commands() {
    let mut fx = Fixture::start(2).await;
    let plan = plan('a', "continue_all").to_string();
    let body = format!(
        "{} >/dev/null || exit 99; echo ready > \"$HOME/asserted\"; while [ ! -f \"$HOME/release\" ]; do sleep 0.02; done",
        observe(&all(1))
    );
    let child = fx.script(&format!(
        "exec \"$EC\" run --pinned --label survivor --workload '{}' -- /bin/sh -c '{}'",
        plan,
        body.replace("'", "'\\''")
    ));
    fx.wait_for_file("asserted").await;
    let original = explained(&fx, "survivor").await.0;
    fx.stop().await;
    fx.serve().await;
    let restored = explained(&fx, "survivor").await.0;
    assert_eq!(restored["state"], "running");
    assert_eq!(restored["workload"], original["workload"]);
    assert_eq!(restored["plan_observation"], all(1));
    fx.touch("release");
    assert_eq!(fx.exit_code(child).await, 0);
    assert_eq!(
        explained(&fx, "survivor").await.0["plan_observation"],
        all(1)
    );

    let lost=fx.script(&format!("exec \"$EC\" run --pinned --label lost --workload '{}' -- /bin/sh -c 'echo ready > \"$HOME/lost-pid\"; sleep 60'",plan));
    fx.wait_for_file("lost-pid").await;
    fx.stop().await;
    // Stop only this fixture's command group while the scheduler is down.
    unsafe {
        libc::kill(-(lost as i32), libc::SIGKILL);
    }
    let _ = fx.exit_code(lost).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    fx.serve().await;
    let recovered = explained(&fx, "lost").await.0;
    assert_eq!(recovered["state"], "ended");
    assert_eq!(
        recovered["plan_observation"],
        json!({"status":"censored","coverage":null,"reason":"service_restart"})
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn observation_binding_grace_includes_slow_request_time() {
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::UnixListener,
    };
    let home = tempfile::Builder::new()
        .prefix("ec-observe-")
        .tempdir_in("/tmp")
        .unwrap();
    let paths = elsecall::paths::Paths::for_home(home.path()).unwrap();
    elsecall::private_files::directory(&paths.data).unwrap();
    let listener = UnixListener::bind(paths.socket()).unwrap();
    let (seen, received) = tokio::sync::oneshot::channel();
    let peer = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut bytes = [0; 4096];
        let n = socket.read(&mut bytes).await.unwrap();
        assert!(String::from_utf8_lossy(&bytes[..n]).contains("/observation"));
        let _ = seen.send(std::time::Instant::now());
        tokio::time::sleep(Duration::from_secs(5)).await;
        let body = r#"{"schema_version":1,"error":{"code":"not_found","message":"not bound","hint":"retry"}}"#;
        let response = format!(
            "HTTP/1.1 404 Not Found\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = socket.write_all(response.as_bytes()).await;
    });
    let child = processes::async_command(env!("CARGO_BIN_EXE_elsecall"))
        .env("HOME", home.path())
        .env("ELSECALL_RUN_ID", "r-000000000001")
        .env("ELSECALL_ADMISSION", "granted")
        .args([
            "workload",
            "observe",
            "--observation",
            r#"{"status":"unknown"}"#,
        ])
        .kill_on_drop(true)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    // Exclude process startup under host load; measure the assertion's own grace.
    let started = tokio::time::timeout(processes::WAIT, received)
        .await
        .unwrap()
        .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(3), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        started.elapsed() < Duration::from_millis(2500),
        "{:?}",
        started.elapsed()
    );
    let response: Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(response["error"]["code"], "observation_timeout");
    peer.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bypassed_and_unmanaged_wrappers_cannot_report_inherited_outer_ids() {
    let fx = Fixture::start(2).await;
    let ec = env!("CARGO_BIN_EXE_elsecall");
    let body = format!(
        r#"
        ELSECALL=off '{ec}' run -- '{ec}' workload observe --observation '{}' > "$HOME/off.json"
        test $? -eq 2 || exit 90
        ELSECALL_ADMISSION=unmanaged {} > "$HOME/unmanaged.json"
        test $? -eq 2 || exit 91
        {} > "$HOME/outer.json" || exit 92
    "#,
        all(0),
        observe(&all(0)),
        observe(&all(1))
    );
    let output = run(&fx, "outer", Some(&plan('a', "continue_all")), &body).await;
    assert!(output.status.success(), "{output:?}");
    for file in ["off.json", "unmanaged.json"] {
        let result: Value = serde_json::from_slice(&std::fs::read(fx.path(file)).unwrap()).unwrap();
        assert_eq!(result["error"]["code"], "invalid_request");
    }
    assert_eq!(explained(&fx, "outer").await.0["plan_observation"], all(1));
}
