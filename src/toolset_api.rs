//! Desired tool versions on the local socket.
//!
//! The same-user rule authorizes the caller. Writes are serialized, re-read the
//! file inside that lock, and run only on the master.
use crate::{
    api,
    failure::{Failure, Result},
    fleet::Fleet,
    paths::Paths,
    roster::{MemberName, Roster},
    tool_inventory::Manager,
    toolset::{
        document::{self, MemberWants, Toolset},
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
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{path::PathBuf, sync::Arc};

pub fn routes(fleet: Result<Arc<Fleet>>, paths: &Paths) -> Router {
    match fleet {
        Ok(fleet) => Router::new()
            .route("/v1/tools/desired", get(show).post(change))
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
    Router::new().route("/v1/tools/desired", any(respond))
}

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
    SetExecutable {
        manager: Manager,
        version: String,
    },
    Unset {
        manager: Manager,
        tool: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        version: Option<String>,
    },
    UnsetExecutable {
        manager: Manager,
    },
}

#[derive(Serialize)]
struct DesiredView {
    members: Vec<MemberView>,
    unknown_members: Vec<MemberName>,
}

#[derive(Serialize)]
struct MemberView {
    member: MemberName,
    wants: MemberWants,
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
            "Reading or updating desired tool versions failed unexpectedly.",
            "Run elsecall service restart, then retry.",
        ))
    })
}

impl Desired {
    fn view(&self, member: Option<MemberName>) -> Result<DesiredView> {
        let roster = self.fleet.roster()?;
        if let Some(name) = &member {
            known(&roster, name)?;
        }
        let toolset = Toolset::parse(&store::read(&self.path)?)?;
        Ok(present(&toolset, &roster, member.as_ref()))
    }

    fn edit(&self, request: DesiredEdit) -> Result<DesiredView> {
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
        format!("Only the master, {master}, changes the fleet's desired tool versions."),
        format!("Run this command on {master}."),
    )
}

fn present(toolset: &Toolset, roster: &Roster, only: Option<&MemberName>) -> DesiredView {
    let names = match only {
        Some(name) => vec![name.clone()],
        None => roster
            .members()
            .iter()
            .map(|member| member.name.clone())
            .collect(),
    };
    DesiredView {
        members: names
            .into_iter()
            .map(|member| MemberView {
                wants: toolset.resolve(&member),
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
        EditBody::SetDefault {
            manager,
            tool,
            version,
        } => Ok(Edit::SetDefault {
            version: exact(scope, manager, Some(&tool), "default", &tool, &version)?,
            tool: require(scope, manager, &tool)?,
            manager,
        }),
        EditBody::AddInstall {
            manager,
            tool,
            version,
        } => Ok(Edit::AddInstall {
            version: exact(scope, manager, Some(&tool), "install", &tool, &version)?,
            tool: require(scope, manager, &tool)?,
            manager,
        }),
        EditBody::SetExecutable { manager, version } => Ok(Edit::SetExecutable {
            version: exact(scope, manager, None, "version", manager.as_str(), &version)?,
            manager,
        }),
        EditBody::Unset {
            manager,
            tool,
            version,
        } => {
            let version = version
                .map(|version| exact(scope, manager, Some(&tool), "default", &tool, &version))
                .transpose()?;
            Ok(Edit::Unset {
                tool: require(scope, manager, &tool)?,
                manager,
                version,
            })
        }
        EditBody::UnsetExecutable { manager } => Ok(Edit::UnsetExecutable { manager }),
    }
}

fn require(scope: &Scope, manager: Manager, tool: &str) -> Result<crate::tool_inventory::ToolName> {
    document::require_tool(&tool_path(scope, manager, tool), manager, tool)
}

fn exact(
    scope: &Scope,
    manager: Manager,
    tool: Option<&str>,
    field: &str,
    label: &str,
    version: &str,
) -> Result<crate::tool_inventory::ToolVersion> {
    document::exact_version(&field_path(scope, manager, tool, field), label, version)
}

fn tool_path(scope: &Scope, manager: Manager, tool: &str) -> String {
    format!("{}.tools.{tool}", manager_path(scope, manager))
}

fn field_path(scope: &Scope, manager: Manager, tool: Option<&str>, field: &str) -> String {
    match tool {
        Some(tool) => format!("{}.{field}", tool_path(scope, manager, tool)),
        None => format!("{}.{field}", manager_path(scope, manager)),
    }
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
