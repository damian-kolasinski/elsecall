#![cfg(target_os = "macos")]
//! Prove isolation in a subprocess: never mutate a multithreaded test's environment.
mod members;
mod processes;
mod support;

use members::TestMember;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn outer_admission_and_unknown_controls_never_reach_test_services_or_scripts() {
    if std::env::var_os("TEST_ENVIRONMENT_PROBE").is_none() {
        // Deliberately bypass the fixture constructor to simulate an outer run.
        let output = std::process::Command::new("python3")
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/scripts/test-environment.py"
            ))
            .arg(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "outer_admission_and_unknown_controls_never_reach_test_services_or_scripts",
                "--nocapture",
            ])
            .env("TEST_ENVIRONMENT_PROBE", "1")
            .env("ELSECALL", "off")
            .env("ELSECALL_ADMISSION", "granted")
            .env("ELSECALL_RUN_ID", "outer-run")
            .env("ELSECALL_MEMBER", "outer-member")
            .env("ELSECALL_FUTURE_CONTROL", "must-not-leak")
            .env("ELSECALL_HOLD_LIMIT", "invalid-outer-limit")
            .env("ELSECALL_TAILNET_NAME", "wrong-member")
            .env("ELSECALL_FLEET_PORT", "invalid-outer-port")
            .env("ELSECALL_MACHINE_SPEED", "invalid-outer-speed")
            .env("ELSECALL_UI_SKIP_GOLDENS", "1")
            .env("ELSECALL_UI_BLESS", "1")
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        return;
    }
    assert_eq!(std::env::var("ELSECALL_UI_SKIP_GOLDENS").unwrap(), "1");
    assert_eq!(std::env::var("ELSECALL_UI_BLESS").unwrap(), "1");
    assert!(std::env::vars_os().all(|(k, _)| {
        k == "ELSECALL_UI_SKIP_GOLDENS"
            || k == "ELSECALL_UI_BLESS"
            || (k != "ELSECALL" && !k.as_encoded_bytes().starts_with(b"ELSECALL_"))
    }));
    for mut command in [
        processes::async_command("/usr/bin/env"),
        processes::command("/usr/bin/env").into(),
    ] {
        let output = command.output().await.unwrap();
        let environment = String::from_utf8_lossy(&output.stdout);
        assert!(
            !environment.lines().any(|s| s.starts_with("ELSECALL")),
            "inherited Elsecall controls leaked"
        );
    }
    // Real member startup must use its own tailnet and port. Its CLI must admit
    // work rather than inherit the outer off switch or invalid hold limit.
    let member = TestMember::start("isolated");
    let deadline = std::time::Instant::now() + processes::WAIT;
    while member.ok(&["runs"])["member"]["speed"] != 1.0 {
        assert!(
            std::time::Instant::now() < deadline,
            "fixture speed observation was not ready"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let output = member.run(&["run", "--", "/bin/sh", "-c",
        "test -z \"${ELSECALL_FUTURE_CONTROL+x}\" && test \"$ELSECALL_ADMISSION\" = granted && test \"$ELSECALL_MEMBER\" = isolated && test \"$ELSECALL_RUN_ID\" != outer-run"]);
    assert!(output.status.success(), "{output:?}");

    let mut fixture = support::Fixture::start(1).await;
    let pid = fixture
        .script("python3 -c 'import os; print(\"\\n\".join(os.environ))' > \"$DIR/environment\"");
    assert_eq!(fixture.exit_code(pid).await, 0);
    let environment = std::fs::read_to_string(fixture.path("environment")).unwrap();
    assert!(
        !environment.lines().any(|s| s.starts_with("ELSECALL")),
        "inherited Elsecall controls leaked"
    );
    let (code, json) = fixture.cli(&["lease", "acquire"]).await;
    assert_eq!(code, 0, "{json}");
    assert_eq!(json["data"]["state"], "granted", "{json}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subprocess_constructors_clear_inherited_controls_before_adding_fixture_controls() {
    if std::env::var_os("TEST_CONSTRUCTOR_PROBE").is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "subprocess_constructors_clear_inherited_controls_before_adding_fixture_controls",
                "--nocapture",
            ])
            .env("TEST_CONSTRUCTOR_PROBE", "1")
            .env("ELSECALL", "off")
            .env("ELSECALL_ADMISSION", "nested")
            .env("ELSECALL_FUTURE_CONTROL", "outer")
            .env("KEEP_TEST_ENVIRONMENT", "kept")
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        return;
    }
    assert_eq!(std::env::var("ELSECALL_ADMISSION").unwrap(), "nested");
    for mut command in [
        processes::async_command("/usr/bin/env"),
        processes::command("/usr/bin/env").into(),
    ] {
        // A fixture may deliberately set a control after cleaning inheritance.
        let output = command
            .env("ELSECALL_FIXTURE", "explicit")
            .output()
            .await
            .unwrap();
        let environment = String::from_utf8_lossy(&output.stdout);
        let controls: Vec<_> = environment
            .lines()
            .filter(|s| s.starts_with("ELSECALL"))
            .collect();
        assert_eq!(controls, ["ELSECALL_FIXTURE=explicit"]);
        assert!(
            environment
                .lines()
                .any(|s| s == "KEEP_TEST_ENVIRONMENT=kept")
        );
    }
}
