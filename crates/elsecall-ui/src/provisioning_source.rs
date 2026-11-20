//! Where the window starts provisioning a member and reads its setup: the
//! port, and its adapter over the local service socket.
//!
//! Provisioning is `POST /v1/tools/provisioning`, which asks the master's
//! provisioning loop to plan and apply the member now (`elsecall tools
//! provision`). A member's setup is its plan (`GET /v1/tools/plan`, on the
//! master only) and its latest apply (`GET /v1/tools/apply`, on any member).
//! Both are read off the UI thread.
use serde::Deserialize;
use serde_json::Value;
use std::{path::PathBuf, sync::Arc, time::Duration};

/// The plan reads every member's workspaces and the member's inventory.
const PLAN_WAIT: Duration = Duration::from_secs(170);
const APPLY_WAIT: Duration = Duration::from_secs(30);
const ASK_WAIT: Duration = Duration::from_secs(10);

pub(crate) trait ProvisioningSource: Send + Sync {
    /// Asks the master to plan and apply `member` now.
    fn provision(&self, member: &str) -> Result<(), String>;
    /// `member`'s plan: slow, since the master reads every member's
    /// workspaces and the member's tools.
    fn plan(&self, member: &str) -> Result<Plan, String>;
    /// `member`'s latest apply, none when it never applied one.
    fn apply(&self, member: &str) -> Result<Option<Apply>, String>;
}

/// A member's plan and latest apply, each read on its own; none while it is
/// read.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Setup {
    pub plan: Option<Result<Plan, String>>,
    pub apply: Option<Result<Option<Apply>, String>>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Plan {
    pub steps: Vec<Tool>,
    pub unresolved: Vec<Unresolved>,
    /// Some pins or the member's tools could not be read, so the plan may
    /// lack steps.
    pub partial: bool,
}

/// One version of one tool, as a person reads it: `node 22.19.0`,
/// `Xcode 26.2`, `the iOS 26.2 runtime`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub(crate) struct Tool {
    pub manager: String,
    pub tool: String,
    #[serde(default)]
    pub version: Option<String>,
}

impl Tool {
    pub(crate) fn label(&self) -> String {
        let version = self.version.as_deref().unwrap_or("its default");
        match self.manager.as_str() {
            "xcodes" => format!("Xcode {version}"),
            "simulators" => format!("{} {version} simulator runtime", self.tool),
            _ => format!("{} {version}", self.tool),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Unresolved {
    pub tool: Tool,
    pub hint: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Apply {
    pub finished: bool,
    pub steps: Vec<Applied>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Applied {
    pub tool: Tool,
    pub state: AppliedState,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AppliedState {
    Queued,
    Running,
    Installed,
    Skipped,
    Failed { stage: String, code: String },
}

/// The local service, as the running window reaches it.
pub(crate) fn production() -> Arc<dyn ProvisioningSource> {
    Arc::new(SocketProvisioning {
        socket: elsecall_host::paths::Paths::current()
            .map(|paths| paths.socket())
            .map_err(|failure| format!("{} {}", failure.message, failure.hint)),
    })
}

pub(crate) struct SocketProvisioning {
    pub(crate) socket: Result<PathBuf, String>,
}

impl SocketProvisioning {
    fn runtime() -> Result<tokio::runtime::Runtime, String> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| error.to_string())
    }

    fn get(&self, route: &str, wait: Duration) -> Result<Value, String> {
        let socket = self.socket.clone()?;
        Self::runtime()?
            .block_on(elsecall_host::client::get_large_within::<Value>(
                &socket, route, wait,
            ))
            .map_err(|failure| format!("{} {}", failure.message, failure.hint))
    }
}

impl ProvisioningSource for SocketProvisioning {
    fn provision(&self, member: &str) -> Result<(), String> {
        let socket = self.socket.clone()?;
        Self::runtime()?
            .block_on(elsecall_host::client::post_within::<Value>(
                &socket,
                "/v1/tools/provisioning",
                serde_json::json!({ "member": member }),
                ASK_WAIT,
            ))
            .map(drop)
            .map_err(|failure| format!("{} {}", failure.message, failure.hint))
    }

    fn plan(&self, member: &str) -> Result<Plan, String> {
        self.get(
            &format!("/v1/tools/plan?member={}", escaped(member)),
            PLAN_WAIT,
        )
        .and_then(|value| plan(&value))
    }

    fn apply(&self, member: &str) -> Result<Option<Apply>, String> {
        self.get(
            &format!("/v1/tools/apply?member={}", escaped(member)),
            APPLY_WAIT,
        )
        .and_then(|value| apply(&value))
    }
}

/// A member name for a query string: anything outside a member name escaped.
fn escaped(member: &str) -> String {
    member
        .bytes()
        .map(|byte| match byte {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' => {
                (byte as char).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect()
}

/// The one member of a `tools plan --member` document.
pub(crate) fn plan(value: &Value) -> Result<Plan, String> {
    let member = &value["members"][0];
    if member["state"] != "planned" {
        return Err(member["error"]["message"]
            .as_str()
            .unwrap_or("This member's plan could not be read.")
            .to_owned());
    }
    #[derive(Deserialize)]
    struct WireUnresolved {
        #[serde(flatten)]
        tool: Tool,
        #[serde(default)]
        hint: String,
    }
    let list = |key: &str| member[key].as_array().cloned().unwrap_or_default();
    let unread = |value: &Value| value.as_array().is_some_and(|list| !list.is_empty());
    let partial = unread(&member["unread_managers"])
        || unread(&value["unread_members"])
        || value["workspaces"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|workspace| workspace["state"] == "failed")
        || value["global"]["state"] == "failed";
    Ok(Plan {
        partial,
        steps: list("steps")
            .into_iter()
            .filter_map(|step| serde_json::from_value(step).ok())
            .collect(),
        unresolved: list("unresolved")
            .into_iter()
            .filter_map(|entry| serde_json::from_value::<WireUnresolved>(entry).ok())
            .map(|entry| Unresolved {
                tool: entry.tool,
                hint: entry.hint,
            })
            .collect(),
    })
}

/// The one member of a `tools applied --member` document.
pub(crate) fn apply(value: &Value) -> Result<Option<Apply>, String> {
    let member = &value["members"][0];
    match member["state"].as_str() {
        Some("none") => return Ok(None),
        Some("running" | "finished") => {}
        _ => {
            return Err(member["error"]["message"]
                .as_str()
                .unwrap_or("This member's latest apply could not be read.")
                .to_owned());
        }
    }
    let apply = &member["apply"];
    let steps = apply["steps"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|step| {
            let tool: Tool = serde_json::from_value(step.clone()).ok()?;
            let text = |key: &str| step[key].as_str().unwrap_or_default().to_owned();
            let state = match step["state"].as_str()? {
                "queued" => AppliedState::Queued,
                "running" => AppliedState::Running,
                "installed" => AppliedState::Installed,
                "skipped" => AppliedState::Skipped,
                "failed" => AppliedState::Failed {
                    stage: text("stage"),
                    code: text("code"),
                },
                _ => return None,
            };
            Some(Applied { tool, state })
        })
        .collect();
    Ok(Some(Apply {
        finished: !apply["finished_at"].is_null(),
        steps,
    }))
}

#[cfg(test)]
mod tests {
    use crate::provisioning_source::*;
    use serde_json::json;

    #[test]
    fn provision_asks_the_master_over_the_socket() {
        use std::{
            io::{Read, Write},
            os::unix::{fs::PermissionsExt, net::UnixListener},
        };
        let root = tempfile::Builder::new()
            .prefix("ec-provision-ui-")
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
            assert!(header.starts_with("POST /v1/tools/provisioning HTTP/1.1\r\n"));
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
                json!({"member": "macbook-pro"})
            );
            let response = json!({"schema_version": 1, "data": {"master": "mac-mini",
                "following": true, "members": []}})
            .to_string();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response.len(),
                response
            )
            .unwrap();
        });
        let source = SocketProvisioning { socket: Ok(socket) };
        assert_eq!(source.provision("macbook-pro"), Ok(()));
        service.join().unwrap();
    }

    #[test]
    fn a_plan_and_an_apply_read_as_people_see_them() {
        let read = plan(
            &json!({"members": [{"member": "macbook-pro", "state": "planned",
            "steps": [{"manager": "mise", "tool": "node", "version": "22.19.0", "exact": true,
                       "reasons": [], "options": {}},
                      {"manager": "xcodes", "tool": "xcode", "version": "26.2"},
                      {"manager": "simulators", "tool": "iOS", "version": "26.2"}],
            "unresolved": [{"manager": "mise", "tool": "python", "version": "3.10.4",
                            "hint": "Install python 3.10.4 on the master.", "reasons": []}]}]}),
        )
        .unwrap();
        let labels: Vec<String> = read.steps.iter().map(Tool::label).collect();
        assert_eq!(
            labels,
            ["node 22.19.0", "Xcode 26.2", "iOS 26.2 simulator runtime"]
        );
        assert_eq!(
            read.unresolved[0].hint,
            "Install python 3.10.4 on the master."
        );
        assert!(!read.partial);
        let partial = plan(
            &json!({"unread_members": [{"member": "macbook", "error": {}}],
            "members": [{"state": "planned", "steps": [], "unresolved": []}]}),
        )
        .unwrap();
        assert!(partial.partial, "a member's pins went unread");
        assert_eq!(
            plan(&json!({"members": [{"state": "unavailable",
                "error": {"code": "member_unreachable", "message": "Gone.", "hint": "h"}}]})),
            Err("Gone.".into())
        );

        let applied = apply(&json!({"members": [{"member": "macbook-pro", "state": "finished",
            "apply": {"started_at": "2026-11-20T09:00:00.000Z", "finished_at": "2026-11-20T09:10:00.000Z",
              "steps": [{"manager": "mise", "tool": "node", "version": "22.19.0", "state": "installed", "at": "x"},
                        {"manager": "xcodes", "tool": "xcode", "version": "26.2", "state": "failed",
                         "stage": "privileges", "code": "xcode_privileges_missing", "at": "x"}]}}]}))
        .unwrap()
        .unwrap();
        assert!(applied.finished);
        assert_eq!(
            applied.steps[1].state,
            AppliedState::Failed {
                stage: "privileges".into(),
                code: "xcode_privileges_missing".into()
            }
        );
        assert_eq!(
            apply(&json!({"members": [{"member": "macbook-pro", "state": "none"}]})),
            Ok(None)
        );
        assert_eq!(escaped("mac mini"), "mac%20mini");
    }
}
