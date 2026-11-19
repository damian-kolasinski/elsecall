//! Adapter from the local service socket to [`FleetSnapshot`](crate::fleet_source::FleetSnapshot).
//!
//! `elsecall fleet` waits 15 seconds on this route, because the service asks every
//! member. `client::get` bounds other local routes at 3 seconds, which is too short
//! for a fleet read, so this uses `client::get_large_within` with the same 15
//! seconds and an 8 MiB bound for member diagnostics.

use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

use crate::fleet_source::{
    FleetSnapshot, FleetSource, Gate, LoadAverages, MemberRow, MemoryUse, Reachability, Release,
    Role, SourceFailure, Standing,
};

/// How long `elsecall fleet` waits for `GET /v1/fleet`.
const FLEET_WAIT: Duration = Duration::from_secs(15);

pub struct SocketSource {
    socket: Result<PathBuf, SourceFailure>,
    runtime: tokio::runtime::Runtime,
}

impl SocketSource {
    pub fn open() -> Self {
        let socket = elsecall_local_state::paths::Paths::current()
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
            .block_on(elsecall_local_client::get_large_within::<Value>(
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
        let paths =
            elsecall_local_state::paths::Paths::current().map_err(|error| error.message.clone())?;
        replace_window_at(&paths.socket())
    }

    fn start_release(&self) -> Result<elsecall::release_update::Job, String> {
        let paths = elsecall_local_state::paths::Paths::current().map_err(|error| error.message)?;
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
        .block_on(elsecall_local_client::post_within(
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
        .block_on(elsecall_local_client::post_within::<Launched>(
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

pub(crate) fn classify(failure: elsecall_failure::Failure) -> SourceFailure {
    let elsecall_failure::Failure {
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
    let master = fleet
        .members
        .iter()
        .find(|member| matches!(member.role, WireRole::Master))
        .and_then(|member| member.build.as_ref())
        .map(|build| build.version.clone());
    let members = fleet
        .members
        .into_iter()
        .map(|member| map_member(member, master.as_deref()))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(FleetSnapshot {
        enrolled: fleet.enrolled,
        members,
        standing: match fleet.roster_error {
            None => Standing::Rostered,
            Some(error) if error.code == "tailnet_unavailable" => {
                Standing::OffTailnet { hint: error.hint }
            }
            Some(error) => Standing::Unrostered {
                message: error.message,
                hint: error.hint,
            },
        },
    })
}

fn map_member(member: WireMember, master: Option<&str>) -> Result<MemberRow, SourceFailure> {
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
    let (cpu, logical_cpus, memory, load) = match machine {
        Some(machine) => (
            machine.cpu_utilization.filter(|value| value.is_finite()),
            machine
                .logical_cpus
                .as_ref()
                .and_then(Value::as_u64)
                .and_then(|count| u32::try_from(count).ok())
                .and_then(NonZeroU32::new),
            machine.memory.map(|memory| MemoryUse {
                used_bytes: memory.used_bytes,
                total_bytes: memory.total_bytes,
            }),
            machine
                .load_average
                .map(|load| LoadAverages {
                    one: load.one_minute,
                    five: load.five_minutes,
                    fifteen: load.fifteen_minutes,
                })
                .filter(|load| {
                    [load.one, load.five, load.fifteen]
                        .iter()
                        .all(|value| value.is_finite() && *value >= 0.0)
                }),
        ),
        None => (None, None, None, None),
    };
    let role = match member.role {
        WireRole::Master => Role::Master,
        WireRole::Member => Role::Member,
    };
    let release = Release::compare(
        member.build.map(|build| build.version),
        member.version_skew,
        master,
    );
    Ok(MemberRow {
        name: member.name,
        this_member: member.this_member,
        role,
        reach,
        gate,
        cpu,
        logical_cpus,
        memory,
        load,
        release,
    })
}

#[derive(Deserialize)]
struct WireFleet {
    enrolled: bool,
    members: Vec<WireMember>,
    /// Older services do not report it.
    #[serde(default)]
    roster_error: Option<WireRosterError>,
}

#[derive(Deserialize)]
struct WireRosterError {
    code: String,
    message: String,
    hint: String,
}

#[derive(Deserialize)]
struct WireMember {
    name: String,
    role: WireRole,
    #[serde(default)]
    this_member: bool,
    reachability: String,
    #[serde(default)]
    status: Option<WireStatus>,
    #[serde(default)]
    blocked: Option<WireBlocked>,
    /// Null when unreachable or when the member predates build IDs.
    #[serde(default)]
    build: Option<WireBuild>,
    /// Null when unknown; older services do not report it.
    #[serde(default)]
    version_skew: Option<bool>,
}

#[derive(Deserialize)]
struct WireBuild {
    version: String,
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
    logical_cpus: Option<Value>,
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
            assert_eq!(
                row.logical_cpus.map(|count| u64::from(count.get())),
                machine["logical_cpus"].as_u64()
            );
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
        assert!(snap.members[1].logical_cpus.is_none());
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
    fn capacity_is_independent_of_utilization_and_requires_a_positive_count() {
        let mut raw = data(include_str!("../tests/fixtures/fleet.json"));
        let members = raw["members"].as_array_mut().unwrap();
        members[0]["status"]["machine"]["cpu_utilization"] = Value::Null;
        members[1]["status"]["machine"]["logical_cpus"] = 0.into();
        members[2]["status"]["machine"]
            .as_object_mut()
            .unwrap()
            .remove("logical_cpus");
        let snapshot = snapshot_from_value(&raw).unwrap();
        assert_eq!(snapshot.members[0].logical_cpus, NonZeroU32::new(12));
        assert!(snapshot.members[0].cpu.is_none());
        for member in &snapshot.members[1..] {
            assert!(member.logical_cpus.is_none());
            assert!(member.cpu.is_some());
            assert!(member.load.is_some());
        }
        for invalid in [(-1).into(), (u64::from(u32::MAX) + 1).into(), "12".into()] {
            raw["members"][1]["status"]["machine"]["logical_cpus"] = invalid;
            let snapshot = snapshot_from_value(&raw).unwrap();
            assert!(snapshot.members[1].logical_cpus.is_none());
            assert!(snapshot.members[1].load.is_some());
        }
    }

    #[test]
    fn a_machine_off_the_tailnet_is_told_how_to_join_one() {
        let snap =
            snapshot_from_value(&data(include_str!("../tests/fixtures/no-tailnet.json"))).unwrap();
        assert!(snap.members.is_empty());
        match snap.standing {
            Standing::OffTailnet { hint } => {
                assert!(hint.contains("Tailscale is running"), "{hint}")
            }
            other => panic!("expected a machine off the tailnet, got {other:?}"),
        }
        // Services from before the field existed report a tailnet as present.
        let older =
            snapshot_from_value(&data(include_str!("../tests/fixtures/fleet.json"))).unwrap();
        assert_eq!(older.standing, Standing::Rostered);
    }

    #[test]
    fn a_listener_without_its_port_is_not_mistaken_for_a_missing_tailnet() {
        let mut document = data(include_str!("../tests/fixtures/no-tailnet.json"));
        document["roster_error"] = serde_json::json!({
            "code": "member_listener_unavailable",
            "message": "Tailscale is running, but the member listener could not bind its port on any tailnet address.",
            "hint": "Another process may hold the port.",
        });
        match snapshot_from_value(&document).unwrap().standing {
            Standing::Unrostered { message, hint } => {
                assert!(message.starts_with("Tailscale is running"), "{message}");
                assert_eq!(hint, "Another process may hold the port.");
            }
            other => panic!("expected a machine without a roster, got {other:?}"),
        }
    }

    #[test]
    fn reads_each_members_release_against_the_masters() {
        let fleet = serde_json::json!({
            "enrolled": true,
            "members": [
                {"name": "mac-mini", "role": "master", "reachability": "reachable",
                 "build": {"version": "0.2.2", "build": "1111111111111111"}, "version_skew": false},
                {"name": "macbook-pro", "role": "member", "reachability": "reachable",
                 "build": {"version": "0.2.1", "build": "2222222222222222"}, "version_skew": true},
                {"name": "mac-studio", "role": "member", "reachability": "unreachable",
                 "build": null, "version_skew": null}
            ]
        });
        let releases: Vec<_> = snapshot_from_value(&fleet)
            .unwrap()
            .members
            .into_iter()
            .map(|member| member.release)
            .collect();
        assert_eq!(
            releases,
            vec![
                Release::Matching {
                    version: "0.2.2".into()
                },
                Release::Behind {
                    version: "0.2.1".into()
                },
                Release::Unknown { version: None },
            ]
        );
    }

    #[test]
    fn classifies_the_service_unavailable_envelope() {
        let fixture = include_str!("../tests/fixtures/error.json");
        let response: elsecall_wire::api::ErrorResponse = serde_json::from_str(fixture).unwrap();
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
            (
                "no-tailnet.json",
                include_str!("../tests/fixtures/no-tailnet.json"),
            ),
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
