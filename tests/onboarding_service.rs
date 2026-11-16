#![cfg(target_os = "macos")]
//! Real local service/CLI under an isolated temporary HOME; no live fleet changes.
mod members;
mod processes;

#[test]
fn onboarding_blocker_and_cancellation_survive_real_service_restart() {
    let mut member = members::TestMember::start("mac-mini");
    let installed = member.data().join("elsecall");
    std::fs::write(&installed, b"existing installation").unwrap();
    let prepared = member.run(&["onboarding", "prepare"]);
    assert!(!prepared.status.success());
    let prepared = members::json(&prepared);
    let data = &prepared["data"];
    assert_eq!(data["stage"], "blocked");
    assert!(data["artifact"].is_null());
    let id = data["id"].as_str().unwrap();
    member.stop();
    member.restart();
    let status = members::json(&member.run(&["onboarding", "status", id]));
    assert_eq!(status, prepared);
    let cancelled = member.ok(&["onboarding", "cancel", id]);
    assert_eq!(cancelled["stage"], "cancelled");
    member.stop();
    member.restart();
    assert_eq!(member.ok(&["onboarding", "status", id]), cancelled);
    assert_eq!(member.ok(&["onboarding", "cancel", id]), cancelled);
    assert!(!member.data().join("fleet.json").exists());
    assert!(!member.data().join("onboarding/claims.sqlite").exists());
    assert_eq!(std::fs::read(installed).unwrap(), b"existing installation");

    // A nonregular status file must be refused before a blocking read/open. This
    // runs in the service process so a regression is bounded by the CLI timeout.
    let status_file = member.data().join("onboarding").join(format!("{id}.json"));
    std::fs::remove_file(&status_file).unwrap();
    let fifo = std::ffi::CString::new(status_file.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: fifo is a valid NUL-terminated path; mkfifo retains no pointer.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    let refused = members::json(&member.run(&["onboarding", "status", id]));
    assert_eq!(refused["error"]["code"], "onboarding_state_unavailable");
    let retry = members::json(&member.run(&["onboarding", "prepare"]));
    assert_eq!(retry["data"]["stage"], "blocked");
}
