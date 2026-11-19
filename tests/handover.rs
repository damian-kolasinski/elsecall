#![cfg(target_os = "macos")]
//! Requests made while an install replaces the service. A separate test binary,
//! because it signals its own process the way launchd signals the service.
mod processes;
mod support;

use std::time::Duration;
use support::{Fixture, queued, record};

/// A script that takes a lease labelled NAME, writes acquire's output to
/// $DIR/NAME.json, and holds the lease until $DIR/release exists.
fn holder(name: &str) -> String {
    format!(
        r#""$EC" lease acquire --label {name} > "$DIR/{name}.json" || exit 9
echo held > "$DIR/{name}.held"
while [ ! -f "$DIR/release" ]; do sleep 0.02; done"#
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn requests_a_handover_interrupts_wait_for_the_new_service() {
    let mut fx = Fixture::start(1).await;
    fx.script(&holder("running"));
    fx.wait_for_file("running.held").await;
    let queued_before = fx.script(&holder("queued"));
    fx.until(|r| queued(r, "interactive") == 1).await;

    // An install takes the installation lock and stops the service, as launchd
    // does: the queued request is turned away, and a request made while the
    // service is away finds nothing to connect to.
    let install = elsecall_installation::installation::Transaction::begin(&fx.paths).unwrap();
    // SAFETY: the service under test handles SIGTERM as its shutdown signal.
    unsafe { libc::kill(std::process::id() as i32, libc::SIGTERM) };
    assert!(fx.stopped_within(processes::WAIT).await);
    let made_meanwhile = fx.script(&holder("meanwhile"));
    tokio::time::sleep(Duration::from_secs(1)).await;
    let answered = |name: &str| {
        std::fs::read(fx.path(&format!("{name}.json"))).is_ok_and(|bytes| !bytes.is_empty())
    };
    assert!(
        !answered("queued") && !answered("meanwhile"),
        "neither request may end while the service is away"
    );

    // The new service adopts the running lease and is asked again for both.
    fx.serve().await;
    drop(install);
    let runs = fx.until(|r| queued(r, "interactive") == 2).await;
    assert_eq!(record(&runs, "running")["state"], "running");
    fx.touch("release");
    fx.wait_for_file("queued.held").await;
    fx.wait_for_file("meanwhile.held").await;
    for (name, pid) in [("queued", queued_before), ("meanwhile", made_meanwhile)] {
        assert_eq!(fx.exit_code(pid).await, 0, "{name}");
        let granted: serde_json::Value =
            serde_json::from_slice(&std::fs::read(fx.path(&format!("{name}.json"))).unwrap())
                .unwrap();
        assert!(granted["data"]["run_id"].is_string(), "{name}: {granted}");
    }

    // Once the install has ended, a service that stops is no handover: a request
    // fails at once, as it always did.
    // SAFETY: as above.
    unsafe { libc::kill(std::process::id() as i32, libc::SIGTERM) };
    assert!(fx.stopped_within(processes::WAIT).await);
    let started = std::time::Instant::now();
    let (code, json) = fx.cli(&["lease", "acquire", "--label", "after"]).await;
    assert_eq!(code, 1, "{json}");
    assert_eq!(json["error"]["code"], "service_unavailable");
    assert!(started.elapsed() < Duration::from_secs(5));
}
