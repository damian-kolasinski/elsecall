//! Launch the actual UI process and require its native-window readiness handshake.
use elsecall::window_startup::{READY_ARGUMENT, WindowStartup};
use std::{
    process::{Command, Stdio},
    time::Duration,
};

#[test]
fn a_native_window_acknowledges_startup_and_a_crashed_window_does_not() {
    if std::env::var_os("ELSECALL_UI_SKIP_GOLDENS").is_some() {
        eprintln!("skipping native window launch with ELSECALL_UI_SKIP_GOLDENS");
        return;
    }
    let home = tempfile::Builder::new()
        .prefix("ec-window-home-")
        .tempdir_in("/tmp")
        .unwrap();
    let startup = WindowStartup::new().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_elsecall-ui"))
        .arg(READY_ARGUMENT)
        .arg(startup.socket())
        .env("HOME", home.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let ready = startup.wait(Duration::from_secs(15));
    let running = child.try_wait().unwrap().is_none();
    let _ = child.kill();
    let _ = child.wait();
    ready.expect("actual native UI must acknowledge readiness");
    assert!(running);

    let startup = WindowStartup::new().unwrap();
    let failed = Command::new(env!("CARGO_BIN_EXE_elsecall-ui"))
        .args(["--elsecall-ready"])
        .arg(startup.socket())
        .args(["--scene", "missing-startup-scene"])
        .env("HOME", home.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(!failed.success());
    assert_eq!(
        startup.wait(Duration::from_millis(25)).unwrap_err().code,
        "window_open_failed"
    );
}
