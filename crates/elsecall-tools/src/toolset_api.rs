//! The exceptions to the pins, `fleet-tools.toml`, on the local socket.
//!
//! The same-user rule authorizes the caller. Writes are serialized, re-read the
//! file inside that lock, and run only on the master. Only a member's exception
//! names versions; `[defaults]` only leaves tools out.
use crate::{
    tool_inventory::Manager,
    toolset::{
        document::{self, MemberException, Toolset},
        edit::{self, Edit, Scope},
        store,
    },
};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{any, get},
};
use elsecall_contract::api;
use elsecall_contract::failure::{Failure, Result};
use elsecall_fleet::fleet::Fleet;
use elsecall_host::paths::Paths;
use elsecall_peers::roster::{MemberName, Roster};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{path::PathBuf, sync::Arc};

pub fn routes(fleet: Result<Arc<Fleet>>, paths: &Paths) -> Router {
    match fleet {
        Ok(fleet) => Router::new()
            .route(ROUTE, get(show).post(change))
            .with_state(Arc::new(Desired {
                fleet,
                path: store::path(&paths.data),
                writes: tokio::sync::Mutex::new(()),
            })),
        Err(error) => unavailable(error),
    }
}

fn unavailable(error: Failure) -> Router {
    let respond = move || {
        let error = error.clone();
        async move {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(api::ErrorResponse::new(error)),
            )
        }
    };
    Router::new().route(ROUTE, any(respond))
}

pub const ROUTE: &str = "/v1/tools/exceptions";

struct Desired {
    fleet: Arc<Fleet>,
    path: PathBuf,
    writes: tokio::sync::Mutex<()>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DesiredQuery {
    #[serde(default)]
    member: Option<MemberName>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DesiredEdit {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub member: Option<MemberName>,
    pub edit: EditBody,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum EditBody {
    LeaveOut {
        manager: Manager,
        tool: String,
    },
    SetDefault {
        manager: Manager,
        tool: String,
        version: String,
    },
    AddInstall {
        manager: Manager,
        tool: String,
        version: String,
    },
    Unset {
        manager: Manager,
        tool: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        version: Option<String>,
    },
}

#[derive(Serialize)]
struct ExceptionsView {
    members: Vec<MemberView>,
    unknown_members: Vec<MemberName>,
}

#[derive(Serialize)]
struct MemberView {
    member: MemberName,
    exceptions: Vec<MemberException>,
}

async fn show(State(desired): State<Arc<Desired>>, Query(query): Query<DesiredQuery>) -> Response {
    let desired = Arc::clone(&desired);
    respond(joined(
        tokio::task::spawn_blocking(move || desired.view(query.member)).await,
    ))
}

async fn change(State(desired): State<Arc<Desired>>, body: Bytes) -> Response {
    let request = match parse::<DesiredEdit>(&body) {
        Ok(request) => request,
        Err(error) => return respond::<()>(Err(error)),
    };
    let _guard = desired.writes.lock().await;
    let desired = Arc::clone(&desired);
    respond(joined(
        tokio::task::spawn_blocking(move || desired.edit(request)).await,
    ))
}

fn joined<T>(result: std::result::Result<Result<T>, tokio::task::JoinError>) -> Result<T> {
    result.unwrap_or_else(|_| {
        Err(Failure::new(
            "tools_failed",
            "Reading or updating the fleet's tool exceptions failed unexpectedly.",
            "Run elsecall service restart, then retry.",
        ))
    })
}

impl Desired {
    fn view(&self, member: Option<MemberName>) -> Result<ExceptionsView> {
        let roster = self.fleet.roster()?;
        if let Some(name) = &member {
            known(&roster, name)?;
        }
        let toolset = Toolset::parse(&store::read(&self.path)?)?;
        Ok(present(&toolset, &roster, member.as_ref()))
    }

    fn edit(&self, request: DesiredEdit) -> Result<ExceptionsView> {
        let roster = self.fleet.roster()?;
        if roster.master().key != self.fleet.public_key() {
            return Err(not_master(&roster.master().name));
        }
        if let Some(name) = &request.member {
            known(&roster, name)?;
        }
        let scope = match &request.member {
            Some(name) => Scope::Member(name.clone()),
            None => Scope::Defaults,
        };
        let edit = to_edit(&scope, request.edit)?;
        let bytes = store::read(&self.path)?;
        let text = std::str::from_utf8(&bytes).map_err(|_| {
            document::invalid("fleet-tools.toml", "the document must be UTF-8 TOML")
        })?;
        let updated = edit::apply(text, &scope, &edit)?;
        let toolset = Toolset::parse(updated.as_bytes())?;
        if updated.as_bytes() != bytes {
            store::write(&self.path, updated.as_bytes())?;
        }
        Ok(present(&toolset, &roster, request.member.as_ref()))
    }
}

fn known(roster: &Roster, name: &MemberName) -> Result<()> {
    if roster.member(name).is_none() {
        Err(Failure::new(
            "unknown_member",
            format!("{name} is not a member of this fleet."),
            "Run elsecall fleet to list members, then retry with one of those names.",
        ))
    } else {
        Ok(())
    }
}

fn not_master(master: &MemberName) -> Failure {
    Failure::new(
        "not_master",
        format!("Only the master, {master}, changes the fleet's tool exceptions."),
        format!("Run this command on {master}."),
    )
}

fn present(toolset: &Toolset, roster: &Roster, only: Option<&MemberName>) -> ExceptionsView {
    let names = match only {
        Some(name) => vec![name.clone()],
        None => roster
            .members()
            .iter()
            .map(|member| member.name.clone())
            .collect(),
    };
    ExceptionsView {
        members: names
            .into_iter()
            .map(|member| MemberView {
                exceptions: toolset.exceptions(&member),
                member,
            })
            .collect(),
        unknown_members: toolset
            .member_names()
            .filter(|name| roster.member(name).is_none())
            .cloned()
            .collect(),
    }
}

fn to_edit(scope: &Scope, body: EditBody) -> Result<Edit> {
    match body {
        EditBody::LeaveOut { manager, tool } => Ok(Edit::LeaveOut {
            tool: require(scope, manager, &tool)?,
            manager,
        }),
        EditBody::SetDefault {
            manager,
            tool,
            version,
        } => {
            member_only(scope)?;
            Ok(Edit::SetDefault {
                version: exact(scope, manager, &tool, "default", &version)?,
                tool: require(scope, manager, &tool)?,
                manager,
            })
        }
        EditBody::AddInstall {
            manager,
            tool,
            version,
        } => {
            member_only(scope)?;
            Ok(Edit::AddInstall {
                version: exact(scope, manager, &tool, "install", &version)?,
                tool: require(scope, manager, &tool)?,
                manager,
            })
        }
        EditBody::Unset {
            manager,
            tool,
            version,
        } => {
            let version = version
                .map(|version| exact(scope, manager, &tool, "default", &version))
                .transpose()?;
            Ok(Edit::Unset {
                tool: require(scope, manager, &tool)?,
                manager,
                version,
            })
        }
    }
}

/// Versions are a member's own; the defaults only leave tools out.
fn member_only(scope: &Scope) -> Result<()> {
    match scope {
        Scope::Member(_) => Ok(()),
        Scope::Defaults => Err(Failure::new(
            "invalid_request",
            "Only one member's exception names versions; for every member, pin the version in the workspace instead.",
            "Pass --member NAME, or change the pin in the workspace's mise config or .xcode-version.",
        )),
    }
}

fn require(scope: &Scope, manager: Manager, tool: &str) -> Result<crate::tool_inventory::ToolName> {
    document::require_tool(&tool_path(scope, manager, tool), manager, tool)
}

fn exact(
    scope: &Scope,
    manager: Manager,
    tool: &str,
    field: &str,
    version: &str,
) -> Result<crate::tool_inventory::ToolVersion> {
    document::exact_version(&field_path(scope, manager, tool, field), tool, version)
}

fn tool_path(scope: &Scope, manager: Manager, tool: &str) -> String {
    format!("{}.tools.{tool}", manager_path(scope, manager))
}

fn field_path(scope: &Scope, manager: Manager, tool: &str, field: &str) -> String {
    format!("{}.{field}", tool_path(scope, manager, tool))
}

fn manager_path(scope: &Scope, manager: Manager) -> String {
    match scope {
        Scope::Defaults => format!("defaults.{}", manager.as_str()),
        Scope::Member(name) => format!("members.{}.{}", name.as_str(), manager.as_str()),
    }
}

fn parse<T: DeserializeOwned>(body: &Bytes) -> Result<T> {
    serde_json::from_slice(body).map_err(|_| {
        Failure::new(
            "invalid_request",
            "The tools request is invalid.",
            "Run elsecall tools --help for the expected arguments.",
        )
    })
}

fn respond<T: Serialize>(result: Result<T>) -> Response {
    match result {
        Ok(data) => Json(api::Response::new(data)).into_response(),
        Err(failure) => {
            let status = match failure.code.as_str() {
                "invalid_request" | "invalid_fleet_tools" => StatusCode::BAD_REQUEST,
                "not_master" | "unknown_member" => StatusCode::CONFLICT,
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            };
            (status, Json(api::ErrorResponse::new(failure))).into_response()
        }
    }
}
