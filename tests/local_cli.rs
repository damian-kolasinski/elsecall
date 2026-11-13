#![cfg(target_os = "macos")]
mod processes;
use elsecall::paths::Paths;
use serde_json::Value;
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    process::{Child, Stdio},
    thread::sleep,
    time::{Duration, Instant},
};

fn home() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("ec-")
        .tempdir_in("/tmp")
        .unwrap()
}
fn cli(home: &std::path::Path, args: &[&str]) -> std::process::Output {
    crate::processes::command(env!("CARGO_BIN_EXE_elsecall"))
        .env("HOME", home)
        .current_dir(home)
        .args(args)
        .output()
        .unwrap()
}
fn json(output: &std::process::Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|_| panic!("not JSON: {:?}", output))
}
struct Service(Child);
impl Drop for Service {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}
fn start(home: &std::path::Path) -> Service {
    let service = Service(
        crate::processes::command(env!("CARGO_BIN_EXE_elsecall"))
            .env("HOME", home)
            .current_dir(home)
            // Loopback stands in for the tailnet so tests never bind real addresses.
            .env("ELSECALL_TAILNET_NAME", "local-cli")
            .env("ELSECALL_TAILNET_ADDRESSES", "127.0.0.1")
            .env("ELSECALL_FLEET_PORT", free_port().to_string())
            .env("ELSECALL_MACHINE_SPEED", "1")
            .arg("serve")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + crate::processes::WAIT;
    while !cli(home, &["status"]).status.success() {
        assert!(Instant::now() < deadline, "service did not start");
        sleep(Duration::from_millis(20));
    }
    service
}

#[test]
fn real_cli_and_service_inspect_restart_after_crash_and_report_unavailability() {
    let home = home();
    let paths = Paths::for_home(home.path()).unwrap();
    let unavailable = cli(home.path(), &["status"]);
    assert_eq!(unavailable.status.code(), Some(1));
    assert_eq!(json(&unavailable)["error"]["code"], "service_unavailable");
    assert!(
        json(&unavailable)["error"]["hint"]
            .as_str()
            .unwrap()
            .contains("doctor")
    );
    let mut service = start(home.path());
    let health = json(&cli(home.path(), &["status"]));
    assert_eq!(health["schema_version"], 1);
    assert_eq!(health["data"]["status"], "ready");
    assert_eq!(health["data"]["pid"], service.0.id());
    let result = cli(home.path(), &["machine"]);
    assert!(result.status.success());
    let machine = json(&result);
    assert_eq!(machine["data"]["os"], "macos");
    assert!(machine["data"]["logical_cpus"].as_u64().unwrap() > 0);
    assert!(machine["data"]["memory"]["total_bytes"].as_u64().unwrap() > 0);
    assert!(
        machine["data"]["load_average"]["one_minute"]
            .as_f64()
            .unwrap()
            >= 0.0
    );
    let cores = &machine["data"]["cores"];
    assert_eq!(
        cores["performance"].as_u64().unwrap() + cores["efficiency"].as_u64().unwrap(),
        machine["data"]["logical_cpus"].as_u64().unwrap()
    );
    assert!(machine["data"]["memory_pressure"].as_str().is_some());
    assert!(machine["data"]["thermal_state"].as_str().is_some());
    assert!(machine["data"].get("cpu_utilization").is_some());
    assert_eq!(
        fs::metadata(&paths.data).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(paths.socket()).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let duplicate = cli(home.path(), &["serve"]);
    assert_eq!(json(&duplicate)["error"]["code"], "already_running");
    assert_eq!(
        json(&cli(home.path(), &["status"]))["data"]["pid"],
        service.0.id()
    );
    service.0.kill().unwrap();
    service.0.wait().unwrap();
    assert!(paths.socket().exists(), "crash leaves socket to recover");
    let replacement = start(home.path());
    assert_ne!(replacement.0.id(), service.0.id());
    // SIGTERM exercises graceful shutdown and removes the socket.
    assert!(
        crate::processes::command("/bin/kill")
            .args(["-TERM", &replacement.0.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    let deadline = Instant::now() + crate::processes::WAIT;
    while paths.socket().exists() {
        assert!(Instant::now() < deadline);
        sleep(Duration::from_millis(20));
    }
}

#[test]
fn refuses_unsafe_state_and_preserves_unrelated_socket_file() {
    let home = home();
    let paths = Paths::for_home(home.path()).unwrap();
    fs::create_dir_all(&paths.data).unwrap();
    fs::set_permissions(&paths.data, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
        json(&cli(home.path(), &["serve"]))["error"]["code"],
        "unsafe_directory"
    );
    fs::set_permissions(&paths.data, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(paths.configuration(), "[capacity]\nreserved_cores = -1\n").unwrap();
    assert_eq!(
        json(&cli(home.path(), &["serve"]))["error"]["code"],
        "invalid_configuration"
    );
    fs::remove_file(paths.configuration()).unwrap();
    fs::write(paths.socket(), "keep me").unwrap();
    assert_eq!(
        json(&cli(home.path(), &["serve"]))["error"]["code"],
        "socket_conflict"
    );
    assert_eq!(fs::read_to_string(paths.socket()).unwrap(), "keep me");
}

#[test]
fn usage_errors_are_json_and_do_not_echo_arguments() {
    let home = home();
    let result = cli(home.path(), &["unexpected-sensitive-value"]);
    assert_eq!(result.status.code(), Some(2));
    assert_eq!(json(&result)["error"]["code"], "invalid_arguments");
    assert!(!String::from_utf8_lossy(&result.stdout).contains("unexpected-sensitive-value"));
    assert!(result.stderr.is_empty());
    let doctor = cli(home.path(), &["doctor"]);
    assert_eq!(doctor.status.code(), Some(1));
    assert_eq!(json(&doctor)["data"]["healthy"], false);
}

#[test]
fn lifecycle_logs_rotate_while_running_and_doctor_reports_sizes_without_secrets() {
    let home = home();
    let paths = Paths::for_home(home.path()).unwrap();
    elsecall::private_files::directory(&paths.data).unwrap();
    fs::write(
        paths.configuration(),
        "[logs]\nmax_size = \"1024B\"\nkeep = 2\n",
    )
    .unwrap();
    let service = start(home.path());
    // Another service cannot acquire the live instance's lock. Each attempt
    // produces a real lifecycle failure through the CLI's production log sink.
    for _ in 0..50 {
        let result = crate::processes::command(env!("CARGO_BIN_EXE_elsecall"))
            .env("HOME", home.path())
            .env("SENSITIVE_ENV", "never-log-this-environment")
            .arg("serve")
            .output()
            .unwrap();
        assert!(!result.status.success());
        assert_eq!(json(&result)["error"]["code"], "already_running");
    }
    assert_eq!(
        json(&cli(home.path(), &["status"]))["data"]["pid"],
        service.0.id()
    );
    for number in 0..=2 {
        let path = if number == 0 {
            paths.log()
        } else {
            elsecall::service_log::Log::archive(&paths.log(), number)
        };
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.ends_with('\n'));
        assert!(text.len() <= 1024);
        for line in text.lines() {
            serde_json::from_str::<Value>(line).unwrap();
        }
        assert!(!text.contains("never-log-this-environment"));
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    assert!(!elsecall::service_log::Log::archive(&paths.log(), 3).exists());
    // A restart also opens the current pathname, rather than retaining an archive.
    drop(service);
    let _replacement = start(home.path());
    assert!(
        fs::read_to_string(paths.log())
            .unwrap()
            .contains("service_ready")
    );
    let doctor = json(&cli(home.path(), &["doctor"]));
    assert!(doctor["data"]["state"]["bytes"].as_u64().unwrap() > 0);
    assert!(
        doctor["data"]["state"]["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["name"] == "service.log")
    );
}

#[test]
fn unsafe_optional_metadata_does_not_stop_the_local_service_or_delete_external_scratch() {
    let home = home();
    let paths = Paths::for_home(home.path()).unwrap();
    elsecall::private_files::directory(&paths.data).unwrap();
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join(".write-123");
    let file = fs::File::create(&target).unwrap();
    file.set_times(
        fs::FileTimes::new()
            .set_modified(std::time::SystemTime::now() - Duration::from_secs(8 * 86_400)),
    )
    .unwrap();
    std::os::unix::fs::symlink(outside.path(), paths.data.join("upgrade")).unwrap();
    let _service = start(home.path());
    assert!(cli(home.path(), &["status"]).status.success());
    assert!(target.exists());
    assert!(
        fs::read_to_string(paths.log())
            .unwrap()
            .contains("state_scratch_cleanup_failed")
    );
}

#[test]
fn invalid_configuration_logs_only_its_code_and_doctor_never_echoes_values() {
    let home = home();
    let paths = Paths::for_home(home.path()).unwrap();
    elsecall::private_files::directory(&paths.data).unwrap();
    fs::write(
        paths.configuration(),
        "[logs]\nkeep = \"private-configuration-value\"\n",
    )
    .unwrap();
    let result = cli(home.path(), &["serve"]);
    assert_eq!(json(&result)["error"]["code"], "invalid_configuration");
    let log = fs::read_to_string(paths.log()).unwrap();
    assert!(log.contains("invalid_configuration"));
    assert!(!log.contains("private-configuration-value"));
    let doctor = cli(home.path(), &["doctor"]);
    assert!(!String::from_utf8_lossy(&doctor.stdout).contains("private-configuration-value"));
    assert_eq!(
        json(&doctor)["data"]["configuration_error"]["code"],
        "invalid_configuration"
    );
}
