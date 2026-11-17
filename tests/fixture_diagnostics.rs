//! A failed fixture test reports its commands and logs through libtest's output.
mod diagnostics;
use std::process::Command;

/// Run only by `failure_reports_commands_and_registered_logs`, as a subprocess.
#[test]
#[ignore = "fails on purpose; run by its parent test"]
fn failing_helper() {
    let directory = std::env::var_os("EC_TEST_DIAGNOSTICS_DIR").expect("run by the parent test");
    let directory = std::path::Path::new(&directory);
    let kept = directory.join("kept.log");
    let dropped = directory.join("dropped.log");
    std::fs::write(&kept, "{\"event\":\"service_ready\"}\n").unwrap();
    std::fs::write(&dropped, "released before the failure\n").unwrap();
    let _kept = diagnostics::register("alpha service log", vec![kept, directory.join("absent")]);
    drop(diagnostics::register("bravo service log", vec![dropped]));
    diagnostics::note("both members recorded keep_local_margin 0.2".into());
    for millis in [30, 10, 20] {
        diagnostics::note_timed(
            "alpha [\"runs\"] -> exit 1 service_unavailable".into(),
            std::time::Duration::from_millis(millis),
        );
    }
    diagnostics::note_timed(
        "alpha [\"runs\"] -> ok".into(),
        std::time::Duration::from_millis(15),
    );
    panic!("the observed state never arrived");
}

#[test]
fn failure_reports_commands_and_registered_logs() {
    let directory = tempfile::tempdir().unwrap();
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "failing_helper"])
        .env("EC_TEST_DIAGNOSTICS_DIR", directory.path())
        // A report printed past the capture would not be under the test's failure.
        .env_remove("RUST_TEST_NOCAPTURE")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!output.status.success(), "{stdout}");
    let panic = stdout
        .find("the observed state never arrived")
        .expect(&stdout);
    let report = &stdout[panic..];
    assert!(report.contains("=== test diagnostics: t+"), "{stdout}");
    assert!(
        report.contains("s both members recorded keep_local_margin 0.2\n"),
        "{stdout}"
    );
    assert!(
        report.contains("alpha [\"runs\"] -> exit 1 service_unavailable (x3) in 0.010-0.030s\n"),
        "{stdout}"
    );
    assert!(
        report.contains("alpha [\"runs\"] -> ok in 0.015s\n"),
        "{stdout}"
    );
    assert!(report.contains("--- alpha service log ("), "{stdout}");
    assert!(report.contains("{\"event\":\"service_ready\"}"), "{stdout}");
    assert!(report.contains("absent)\n[absent]\n"), "{stdout}");
    assert!(!report.contains("released before the failure"), "{stdout}");
    assert!(report.contains("=== end of test diagnostics"), "{stdout}");
}
