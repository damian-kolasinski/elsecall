#![cfg(target_os = "macos")]
//! Stopping the service while requests queue. A separate test binary, because it
//! signals its own process the way launchd signals the service.
mod processes;
mod support;

use support::{Fixture, queued};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stopping_fails_queued_requests_instead_of_waiting_for_them() {
    let mut fx = Fixture::start(1).await;
    fx.script(
        r#""$EC" lease acquire >/dev/null; echo held > "$DIR/held"; while [ ! -f "$DIR/release" ]; do sleep 0.02; done"#,
    );
    fx.wait_for_file("held").await;
    let waiter = fx.script(r#""$EC" lease acquire > "$DIR/waiter.json""#);
    fx.until(|r| queued(r, "interactive") == 1).await;
    // SAFETY: the service under test handles SIGTERM as its shutdown signal.
    unsafe { libc::kill(std::process::id() as i32, libc::SIGTERM) };
    assert!(
        fx.stopped_within(crate::processes::WAIT).await,
        "the service stops although a request was queued for its full hold limit"
    );
    assert_eq!(fx.exit_code(waiter).await, 1);
    let waiter: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(fx.path("waiter.json")).unwrap()).unwrap();
    assert_eq!(waiter["error"]["code"], "service_unavailable");
    fx.touch("release");
}
