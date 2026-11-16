//! Adapter from the local service socket to [`FleetSnapshot`](crate::fleet_source::FleetSnapshot).
//!
//! `elsecall fleet` waits 15 seconds on this route, because the service asks every
//! member. `client::get` bounds other local routes at 3 seconds, which is too short
//! for a fleet read, so this uses `client::get_large_within` with the same 15
//! seconds and an 8 MiB bound for member diagnostics.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

use crate::fleet_source::{
    FleetSnapshot, FleetSource, Gate, LoadAverages, MemberRow, MemoryUse, Reachability, Role,
    SourceFailure,
};

/// How long `elsecall fleet` waits for `GET /v1/fleet`.
const FLEET_WAIT: Duration = Duration::from_secs(15);

pub struct SocketSource {
    socket: Result<PathBuf, SourceFailure>,
    runtime: tokio::runtime::Runtime,
}

impl SocketSource {
    pub fn open() -> Self {
        let socket = elsecall::paths::Paths::current()
            .map(|paths| paths.socket())
            .map_err(classify);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime for the fleet socket");
        Self { socket, runtime }
    }

    fn fetch(&self) -> Result<FleetSnapshot, SourceFailure> {
        let socket = match &self.socket {
            Ok(path) => path,
            Err(failure) => return Err(failure.clone()),
        };
        let value = self
            .runtime
            .block_on(elsecall::client::get_large_within::<Value>(
                socket,
                "/v1/fleet",
                FLEET_WAIT,
            ))
            .map_err(classify)?;
        snapshot_from_value(&value)
    }
}

impl FleetSource for SocketSource {
    fn fleet(&self) -> Result<FleetSnapshot, SourceFailure> {
        self.fetch()
    }
}

pub struct SocketWindowUpgrade;

impl crate::window_upgrade::WindowUpgrade for SocketWindowUpgrade {
    fn replace_and_reopen(&self) -> Result<(), String> {
        let paths = elsecall::paths::Paths::current().map_err(|error| error.message.clone())?;
        replace_window_at(&paths.socket())
    }

    fn start_release(&self) -> Result<elsecall::release_update::Job, String> {
        let paths = elsecall::paths::Paths::current().map_err(|error| error.message)?;
        start_release_at(&paths.socket())
    }
    fn close_old_window(&self) {
        std::process::exit(0);
    }
}

fn start_release_at(socket: &std::path::Path) -> Result<elsecall::release_update::Job, String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    runtime
        .block_on(elsecall::client::post_within(
            socket,
            "/v1/update/install",
            serde_json::json!({"reopen":true,"wait_seconds":300}),
            Duration::from_secs(30),
        ))
        .map_err(|error| format!("{} {}", error.message, error.hint))
}

fn replace_window_at(socket: &std::path::Path) -> Result<(), String> {
    #[derive(Deserialize)]
    struct Launched {
        launched: bool,
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    let result = runtime
        .block_on(elsecall::client::post_within::<Launched>(
            socket,
            "/v1/ui/update",
            serde_json::json!({}),
            Duration::from_secs(30),
        ))
        .map_err(|error| format!("{} {}", error.message, error.hint))?;
    if result.launched {
        Ok(())
    } else {
        Err("The updated window did not launch. Retry the upgrade.".into())
    }
}

/// Opens the socket on the first read, which the refresh thread performs.
pub fn shared() -> Arc<dyn FleetSource> {
    Arc::new(LazySocket {
        source: OnceLock::new(),
    })
}

struct LazySocket {
    source: OnceLock<SocketSource>,
}

impl FleetSource for LazySocket {
    fn fleet(&self) -> Result<FleetSnapshot, SourceFailure> {
        self.source.get_or_init(SocketSource::open).fleet()
    }
}

pub(crate) fn classify(failure: elsecall::failure::Failure) -> SourceFailure {
    let elsecall::failure::Failure {
        code,
        message,
        hint,
        ..
    } = failure;
    if code == "service_unavailable" {
        SourceFailure::ServiceUnavailable { message, hint }
    } else {
        SourceFailure::Rejected { message, hint }
    }
}

pub(crate) fn snapshot_from_value(data: &Value) -> Result<FleetSnapshot, SourceFailure> {
    let fleet: WireFleet =
        serde_json::from_value(data.clone()).map_err(|error| SourceFailure::Rejected {
            message: format!("The fleet response could not be read ({error})."),
            hint: "Reinstall matching CLI and service versions with elsecall install.".into(),
        })?;
    let members = fleet
        .members
        .into_iter()
        .map(map_member)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(FleetSnapshot {
        enrolled: fleet.enrolled,
        members,
    })
}

fn map_member(member: WireMember) -> Result<MemberRow, SourceFailure> {
    let reach = match member.reachability.as_str() {
        "reachable" => Reachability::Reachable,
        "unreachable" => Reachability::Unreachable,
        other => {
            return Err(SourceFailure::Rejected {
                message: format!(
                    "The fleet response could not be read (reachability {other:?} on {}).",
                    member.name
                ),
                hint: "Reinstall matching CLI and service versions with elsecall install.".into(),
            });
        }
    };
    let gate = match member.blocked {
        Some(blocked) => Gate::Blocked {
            leading_process: blocked
                .outside
                .into_iter()
                .map(|process| process.name)
                .find(|name| !name.is_empty()),
        },
        None => Gate::Open,
    };
    let machine = match reach {
        Reachability::Reachable => member.status.and_then(|status| status.machine),
        Reachability::Unreachable => None,
    };
    let (cpu, memory, load) = match machine {
        Some(machine) => (
            machine.cpu_utilization.filter(|value| value.is_finite()),
            machine.memory.map(|memory| MemoryUse {
                used_bytes: memory.used_bytes,
                total_bytes: memory.total_bytes,
            }),
            machine.load_average.map(|load| LoadAverages {
                one: load.one_minute,
                five: load.five_minutes,
                fifteen: load.fifteen_minutes,
            }),
        ),
        None => (None, None, None),
    };
    let role = match member.role {
        WireRole::Master => Role::Master,
        WireRole::Member => Role::Member,
    };
    Ok(MemberRow {
        name: member.name,
        role,
        reach,
        gate,
        cpu,
        memory,
        load,
    })
}

#[derive(Deserialize)]
struct WireFleet {
    enrolled: bool,
    members: Vec<WireMember>,
}

#[derive(Deserialize)]
struct WireMember {
    name: String,
    role: WireRole,
    reachability: String,
    #[serde(default)]
    status: Option<WireStatus>,
    #[serde(default)]
    blocked: Option<WireBlocked>,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum WireRole {
    Master,
    Member,
}

#[derive(Deserialize)]
struct WireStatus {
    #[serde(default)]
    machine: Option<WireMachine>,
}

#[derive(Deserialize)]
struct WireMachine {
    #[serde(default)]
    cpu_utilization: Option<f64>,
    #[serde(default)]
    memory: Option<WireMemory>,
    #[serde(default)]
    load_average: Option<WireLoad>,
}

#[derive(Deserialize)]
struct WireMemory {
    used_bytes: u64,
    total_bytes: u64,
}

#[derive(Deserialize)]
struct WireLoad {
    one_minute: f64,
    five_minutes: f64,
    fifteen_minutes: f64,
}

#[derive(Deserialize)]
struct WireBlocked {
    #[serde(default)]
    outside: Vec<WireProcess>,
}

#[derive(Deserialize)]
struct WireProcess {
    #[serde(default)]
    name: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_release_button_requests_installation_and_reopening_over_the_socket() {
        use std::{
            io::{Read, Write},
            os::unix::{fs::PermissionsExt, net::UnixListener},
        };
        let root = tempfile::Builder::new()
            .prefix("ec-release-ui-")
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir_in("/tmp")
            .unwrap();
        let socket = root.path().join("service.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let service = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut header = Vec::new();
            while !header.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                stream.read_exact(&mut byte).unwrap();
                header.push(byte[0]);
            }
            let header = String::from_utf8(header).unwrap();
            assert!(header.starts_with("POST /v1/update/install HTTP/1.1\r\n"));
            let length = header
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length: ")
                        .map(|length| length.parse::<usize>().unwrap())
                })
                .unwrap();
            let mut body = vec![0; length];
            stream.read_exact(&mut body).unwrap();
            assert_eq!(
                serde_json::from_slice::<Value>(&body).unwrap(),
                serde_json::json!({"reopen":true,"wait_seconds":300})
            );
            let response=serde_json::json!({"schema_version":1,"data":{"id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","release":{"version":"0.1.1"},"from_build":"1111111111111111","started_at_ms":20,"reopen":true,"wait_seconds":300,"state":"starting","updated_at":1700000000}}).to_string();
            write!(stream,"HTTP/1.1 202 Accepted\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",response.len(),response).unwrap();
        });
        let job = start_release_at(&socket).unwrap();
        assert!(job.reopen);
        assert_eq!(job.release.version.to_string(), "0.1.1");
        service.join().unwrap();
    }
    #[test]
    fn reads_a_large_fleet_response_over_the_real_socket() {
        use std::{
            io::{Read, Write},
            os::unix::{fs::PermissionsExt, net::UnixListener},
        };
        let root = tempfile::Builder::new()
            .prefix("ec-fleet-")
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir_in("/tmp")
            .unwrap();
        let socket = root.path().join("service.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let mut fleet: Value =
            serde_json::from_str(include_str!("../tests/fixtures/fleet.json")).unwrap();
        let expected = snapshot_from_value(&fleet["data"]).unwrap();
        fleet["data"]["members"][0]["status"]["processes"] = serde_json::json!(
            (1..=4096)
                .map(|pid| serde_json::json!({"pid":pid,"name":"compiler","cpu_seconds":1.5}))
                .collect::<Vec<_>>()
        );
        let body = serde_json::to_vec(&fleet).unwrap();
        assert!(body.len() > 64 * 1024);
        let service = std::thread::spawn(move || {
            let (mut connection, _) = listener.accept().unwrap();
            connection
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                connection.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            assert!(request.starts_with(b"GET /v1/fleet HTTP/1.1\r\n"));
            write!(connection, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
            connection.write_all(&body).unwrap();
        });
        let source = SocketSource {
            socket: Ok(socket),
            runtime: tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap(),
        };
        assert_eq!(source.fleet().unwrap(), expected);
        service.join().unwrap();
    }

    #[test]
    fn replacement_requires_a_successful_launch_confirmation_from_the_socket() {
        use std::{
            io::{Read, Write},
            os::unix::net::UnixListener,
        };
        for (status, body, success) in [
            (
                "200 OK",
                r#"{"schema_version":1,"data":{"launched":true}}"#,
                true,
            ),
            (
                "200 OK",
                r#"{"schema_version":1,"data":{"launched":false}}"#,
                false,
            ),
            ("200 OK", r#"{"schema_version":1,"data":{}}"#, false),
            ("200 OK", "not-json", false),
            (
                "500 Internal Server Error",
                r#"{"schema_version":1,"error":{"code":"window_open_failed","message":"Could not open.","hint":"Retry the upgrade."}}"#,
                false,
            ),
        ] {
            let root = tempfile::Builder::new()
                .prefix("ec-ui-sock-")
                .permissions({
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::Permissions::from_mode(0o700)
                })
                .tempdir_in("/tmp")
                .unwrap();
            let socket = root.path().join("service.sock");
            let listener = UnixListener::bind(&socket).unwrap();
            let service = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut bytes = [0; 2048];
                let count = stream.read(&mut bytes).unwrap();
                assert!(
                    std::str::from_utf8(&bytes[..count])
                        .unwrap()
                        .starts_with("POST /v1/ui/update HTTP/1.1\r\n")
                );
                write!(stream, "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            });
            let result = replace_window_at(&socket);
            assert_eq!(result.is_ok(), success, "{body}: {result:?}");
            service.join().unwrap();
        }
    }

    fn data(fixture: &str) -> Value {
        let document: Value = serde_json::from_str(fixture).unwrap();
        document["data"].clone()
    }

    #[test]
    fn maps_the_fleet_fixture() {
        let fixture = include_str!("../tests/fixtures/fleet.json");
        let raw: Value = serde_json::from_str(fixture).unwrap();
        let snap = snapshot_from_value(&raw["data"]).unwrap();
        assert!(snap.enrolled);
        let members = raw["data"]["members"].as_array().unwrap();
        assert_eq!(snap.members.len(), members.len());
        assert_eq!(
            snap.members
                .iter()
                .map(|member| member.name.as_str())
                .collect::<Vec<_>>(),
            ["alpha", "beta", "gamma"]
        );
        assert_eq!(snap.members[0].role, Role::Master);
        for (row, member) in snap.members.iter().zip(members) {
            assert_eq!(row.name, member["name"].as_str().unwrap());
            match member["role"].as_str().unwrap() {
                "master" => assert_eq!(row.role, Role::Master),
                "member" => assert_eq!(row.role, Role::Member),
                other => panic!("unexpected role {other}"),
            }
            assert!(matches!(row.reach, Reachability::Reachable));
            assert!(matches!(row.gate, Gate::Open));
            assert_eq!(member["key"].as_str().unwrap().len(), 43);
            let endpoints = member["endpoints"].as_array().unwrap();
            assert!(
                endpoints
                    .iter()
                    .any(|endpoint| endpoint.as_str().unwrap().contains("192.0.2."))
            );
            assert!(
                endpoints
                    .iter()
                    .any(|endpoint| endpoint.as_str().unwrap().contains("2001:db8::"))
            );
            let machine = &member["status"]["machine"];
            assert_eq!(
                machine["hostname"].as_str().unwrap(),
                format!("{}.example", row.name)
            );
            assert_eq!(row.cpu, machine["cpu_utilization"].as_f64());
            let memory = row.memory.unwrap();
            assert_eq!(
                memory.used_bytes,
                machine["memory"]["used_bytes"].as_u64().unwrap()
            );
            assert_eq!(
                memory.total_bytes,
                machine["memory"]["total_bytes"].as_u64().unwrap()
            );
            let load = row.load.unwrap();
            assert_eq!(
                load.one,
                machine["load_average"]["one_minute"].as_f64().unwrap()
            );
            assert_eq!(
                load.five,
                machine["load_average"]["five_minutes"].as_f64().unwrap()
            );
            assert_eq!(
                load.fifteen,
                machine["load_average"]["fifteen_minutes"].as_f64().unwrap()
            );
        }
    }

    #[test]
    fn maps_an_unreachable_member() {
        let snap =
            snapshot_from_value(&data(include_str!("../tests/fixtures/unreachable.json"))).unwrap();
        assert!(matches!(snap.members[0].reach, Reachability::Reachable));
        assert!(snap.members[0].cpu.is_some());
        assert!(matches!(snap.members[1].reach, Reachability::Unreachable));
        assert!(snap.members[1].cpu.is_none());
        assert!(snap.members[1].memory.is_none());
        assert!(snap.members[1].load.is_none());
        assert!(matches!(snap.members[1].gate, Gate::Open));
    }

    #[test]
    fn maps_a_blocked_member() {
        let snap =
            snapshot_from_value(&data(include_str!("../tests/fixtures/blocked.json"))).unwrap();
        assert!(matches!(snap.members[1].reach, Reachability::Reachable));
        assert!(snap.members[1].cpu.is_some());
        match &snap.members[1].gate {
            Gate::Blocked {
                leading_process: Some(name),
            } => assert_eq!(name, "Xcode"),
            other => panic!("expected Xcode blocking the member, got {other:?}"),
        }
    }

    #[test]
    fn classifies_the_service_unavailable_envelope() {
        let fixture = include_str!("../tests/fixtures/error.json");
        let response: elsecall::api::ErrorResponse = serde_json::from_str(fixture).unwrap();
        let failure = classify(response.error);
        match failure {
            SourceFailure::ServiceUnavailable { hint, .. } => {
                assert!(hint.contains("elsecall install"), "{hint}");
                assert!(hint.contains("elsecall service restart"), "{hint}");
            }
            other => panic!("expected service unavailable, got {other:?}"),
        }
        let document: Value = serde_json::from_str(fixture).unwrap();
        assert!(snapshot_from_value(&document).is_err());
    }

    #[test]
    fn fixtures_omit_operator_identifiers() {
        for (name, fixture) in [
            ("fleet.json", include_str!("../tests/fixtures/fleet.json")),
            (
                "unreachable.json",
                include_str!("../tests/fixtures/unreachable.json"),
            ),
            (
                "blocked.json",
                include_str!("../tests/fixtures/blocked.json"),
            ),
            ("error.json", include_str!("../tests/fixtures/error.json")),
        ] {
            for needle in ["/Users/", ".home", "100.", "fd7a:"] {
                assert!(!fixture.contains(needle), "{name} contains {needle}");
            }
        }
    }

    #[test]
    fn rejects_an_unknown_reachability() {
        let value = serde_json::json!({
            "enrolled": true,
            "members": [{
                "name": "studio",
                "role": "member",
                "reachability": "maybe"
            }]
        });
        let error = snapshot_from_value(&value).unwrap_err();
        assert!(matches!(error, SourceFailure::Rejected { .. }));
        assert!(error.message().contains("maybe"));
    }
}
