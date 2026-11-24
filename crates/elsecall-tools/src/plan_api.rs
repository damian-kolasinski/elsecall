//! The plan per member over HTTP, and the member route that feeds it.
//!
//! `GET /v1/tools/plan` runs on the master, which reads its own global mise
//! config and judges pins against what it has installed. It asks every member
//! at once for its inventory (the fleet inventory's adapters) and for the pins
//! of the workspaces its runs started in (`GET /peer/v1/tools/workspace-pins`),
//! reads `fleet-tools.toml`, and plans each member. `?member=NAME` plans one
//! member; every member's workspaces still count. On another member the route
//! is `not_master`. A machine that has not joined a fleet is the master of a
//! fleet of one.
//!
//! `GET /peer/v1/tools/workspace-pins` is this member's [`WorkspaceReport`]:
//! its recent workspaces ([`RecentWorkspaces`], the port the service composes
//! with the runs' record of them) and what each one's files pin, read as data
//! off the async worker. Any enrolled member may ask, as for inventory.
use crate::{
    fleet_tool_inventory::{self, MemberInventories, OwnInventories, PeerInventories},
    pins::{self, Reading},
    plan::{
        self, Inputs,
        document::Document,
        reported::{self, ReportedWorkspace, WorkspaceReport},
    },
    toolset::{document::Toolset, store},
};
use axum::{
    Json, Router,
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use elsecall_contract::{
    api,
    failure::{Failure, Result},
    scheduling_model::timestamp::Timestamp,
};
use elsecall_fleet::fleet::Fleet;
use elsecall_host::paths::Paths;
use elsecall_peers::{
    membership_store::Membership,
    peer_client,
    roster::{Member, MemberName},
};
use futures_util::future::BoxFuture;
use hyper::Method;
use serde::Deserialize;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

pub const ROUTE: &str = "/v1/tools/plan";
pub const PEER_ROUTE: &str = "/peer/v1/tools/workspace-pins";
/// Reading pins is reading files; a member that takes longer is unavailable.
/// Shorter than the member listener's 30-second exchange, so this side decides.
const PEER_TIMEOUT: Duration = Duration::from_secs(20);

/// The port: the workspaces this member's runs started in.
pub trait RecentWorkspaces: Send + Sync {
    /// Roots with a run in the retention window, the most recent first.
    fn recent(&self) -> Vec<RecentWorkspace>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecentWorkspace {
    pub root: PathBuf,
    pub last_run_at: Timestamp,
}

/// What planning reads: the fleet, `fleet-tools.toml` and this member's
/// recent workspaces.
pub struct Planning {
    fleet: std::result::Result<Arc<Fleet>, Failure>,
    exceptions: PathBuf,
    recent: Arc<dyn RecentWorkspaces>,
}

impl Planning {
    pub fn new(
        fleet: std::result::Result<Arc<Fleet>, Failure>,
        paths: &Paths,
        recent: Arc<dyn RecentWorkspaces>,
    ) -> Self {
        Self {
            fleet,
            exceptions: store::path(&paths.data),
            recent,
        }
    }

    pub(crate) fn fleet(&self) -> Result<Arc<Fleet>> {
        self.fleet.clone()
    }
}

pub fn routes(planning: Planning) -> Router {
    Router::new()
        .route(ROUTE, get(plan_route))
        .with_state(Arc::new(planning))
}

/// The member route, behind the member API's authentication.
pub fn peer_routes(recent: Arc<dyn RecentWorkspaces>) -> Router<Arc<Fleet>> {
    Router::new().route(
        PEER_ROUTE,
        get(move || {
            let recent = Arc::clone(&recent);
            async move {
                match own_report(recent).await {
                    Ok(report) => match bounded(&report) {
                        Ok(()) => Json(api::Response::new(report)).into_response(),
                        Err(error) => {
                            elsecall_api_response::failure(StatusCode::PAYLOAD_TOO_LARGE, error)
                        }
                    },
                    Err(error) => {
                        elsecall_api_response::failure(StatusCode::INTERNAL_SERVER_ERROR, error)
                    }
                }
            }
        }),
    )
}

fn bounded(report: &WorkspaceReport) -> Result<()> {
    let bytes = serde_json::to_vec(&api::Response::new(report)).expect("report serializes");
    if bytes.len() > fleet_tool_inventory::PER_MEMBER_LIMIT {
        return Err(Failure::new(
            "pins_too_large",
            "This member's workspace pins are larger than 512 KiB.",
            "Remove worktrees you no longer use; a workspace drops out 30 days after its last run.",
        ));
    }
    Ok(())
}

/// This member's recent workspaces and their pins, read off the async worker.
async fn own_report(recent: Arc<dyn RecentWorkspaces>) -> Result<WorkspaceReport> {
    tokio::task::spawn_blocking(move || {
        let home = home()?;
        Ok(read_report(&recent.recent(), &home))
    })
    .await
    .map_err(|_| stopped())?
}

pub(crate) fn read_report(recent: &[RecentWorkspace], home: &Path) -> WorkspaceReport {
    let workspaces = recent
        .iter()
        .filter(|workspace| workspace.root.is_dir())
        .filter(|workspace| pins::workspace_root(&workspace.root, home).is_ok())
        .take(reported::MAXIMUM_WORKSPACES)
        .map(|workspace| {
            let name = reported::workspace_name(&workspace.root, home);
            let pins = pins::workspace_pins(&workspace.root, home)
                .and_then(|reading| within_bounds(reading, &name))
                .map_err(|failure| as_pin_failure(failure, &name));
            ReportedWorkspace {
                workspace: name,
                last_run_at: workspace.last_run_at,
                pins,
            }
        })
        .collect();
    WorkspaceReport {
        workspaces,
        signed_pins: pins::packslip::fingerprints(home).ok(),
    }
}

fn within_bounds(reading: Reading, name: &str) -> Result<Reading> {
    if reading.pins.len() > reported::MAXIMUM_PINS {
        return Err(Failure::new(
            "pin_source_invalid",
            format!("{name}: more than {} pins.", reported::MAXIMUM_PINS),
            "Pin fewer tools in the workspace.",
        ));
    }
    Ok(reading)
}

/// What the master accepts: a workspace that vanished between the listing and
/// the read, say, is unreadable rather than some other code.
fn as_pin_failure(failure: Failure, name: &str) -> Failure {
    match failure.code.as_str() {
        "pin_source_unreadable" | "pin_source_invalid" | "pins_conflict" => failure,
        _ => Failure::new(
            "pin_source_unreadable",
            format!("{name}: the workspace could not be read."),
            format!("Run elsecall tools pins {name} on this member to see why."),
        ),
    }
}

fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| {
            Failure::new(
                "pin_source_unreadable",
                "HOME is not set, so workspaces cannot be named or the global mise config found.",
                "Run elsecall service restart from a login session.",
            )
        })
}

fn stopped() -> Failure {
    Failure::new(
        "tools_failed",
        "Planning stopped unexpectedly.",
        "Run elsecall service restart, then retry.",
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PlanQuery {
    #[serde(default)]
    member: Option<String>,
}

async fn plan_route(
    State(planning): State<Arc<Planning>>,
    Query(query): Query<PlanQuery>,
) -> Response {
    match plan(&planning, query.member.as_deref()).await {
        Ok(value) => Json(api::Response::new(value)).into_response(),
        Err(failure) => {
            let status = match failure.code.as_str() {
                "invalid_request" | "invalid_fleet_tools" => StatusCode::BAD_REQUEST,
                "not_master" => StatusCode::CONFLICT,
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            };
            (status, Json(api::ErrorResponse::new(failure))).into_response()
        }
    }
}

/// One member, and how to reach it: none for this member.
pub(crate) struct Reach {
    pub(crate) name: MemberName,
    pub(crate) peer: Option<Member>,
}

async fn plan(planning: &Planning, only: Option<&str>) -> Result<serde_json::Value> {
    gather(planning, only)
        .await
        .map(|(document, _)| document.to_json())
}

/// The plan, and how to reach each member it names.
pub(crate) async fn gather(
    planning: &Planning,
    only: Option<&str>,
) -> Result<(Document, Vec<Reach>)> {
    let fleet = planning.fleet.clone()?;
    let (_, members) = fleet_members(&fleet)?;
    let only = match only {
        None => None,
        Some(wanted) => Some(vec![
            members
                .iter()
                .find(|member| member.name.as_str() == wanted)
                .map(|member| member.name.clone())
                .ok_or_else(|| unknown_member(wanted, &members))?,
        ]),
    };
    gather_for(planning, only).await
}

/// The plan of the members `only` names, or every member's; every member's
/// workspaces count either way, and only their inventories and the master's
/// are read.
pub(crate) async fn gather_for(
    planning: &Planning,
    only: Option<Vec<MemberName>>,
) -> Result<(Document, Vec<Reach>)> {
    let fleet = planning.fleet.clone()?;
    let (master, members) = fleet_members(&fleet)?;
    let path = planning.exceptions.clone();
    let toolset = tokio::task::spawn_blocking(move || Toolset::parse(&store::read(&path)?))
        .await
        .map_err(|_| stopped())??;
    // The master judges every pin, so its inventory is always read; with
    // --member, no other member's is waited for.
    let inventory_sources: Vec<fleet_tool_inventory::Source> = members
        .iter()
        .filter(|member| {
            member.peer.is_none() || only.as_ref().is_none_or(|only| only.contains(&member.name))
        })
        .map(|member| fleet_tool_inventory::Source {
            member: member.name.clone(),
            inventories: match &member.peer {
                None => Arc::new(OwnInventories) as Arc<dyn MemberInventories>,
                Some(peer) => Arc::new(PeerInventories {
                    fleet: Arc::clone(&fleet),
                    member: peer.clone(),
                }),
            },
        })
        .collect();
    let reports = futures_util::future::join_all(members.iter().map(|member| {
        let name = member.name.clone();
        let read = report_of(&fleet, member, Arc::clone(&planning.recent));
        async move { (name, read.await) }
    }));
    let (inventories, reports) =
        tokio::join!(fleet_tool_inventory::gather(&inventory_sources), reports);
    let selected = toolset.managed_packslip.clone();
    let global = tokio::task::spawn_blocking(move || {
        home().and_then(|home| pins::mise_global::read_managed(&home, &selected))
    })
    .await
    .map_err(|_| stopped())?;
    if !toolset.managed_packslip.is_empty()
        && let Err(error) = &global
    {
        return Err(error.clone());
    }
    let document = plan::build(Inputs {
        master,
        global,
        reports,
        inventories,
        toolset,
        only,
    });
    Ok((document, members))
}

fn report_of(
    fleet: &Arc<Fleet>,
    member: &Reach,
    recent: Arc<dyn RecentWorkspaces>,
) -> BoxFuture<'static, Result<WorkspaceReport>> {
    match &member.peer {
        None => Box::pin(own_report(recent)),
        Some(peer) => {
            let fleet = Arc::clone(fleet);
            let peer = peer.clone();
            Box::pin(async move {
                peer_client::call::<WorkspaceReport>(
                    fleet.key(),
                    &peer,
                    Method::GET,
                    PEER_ROUTE,
                    None::<&()>,
                    PEER_TIMEOUT,
                )
                .await
                .map_err(|error| unreported(&peer.name, &error))
            })
        }
    }
}

/// Every member, this one first, or the one `only` names: on any member, for
/// reading what each installed.
pub(crate) fn members_to_read(fleet: &Arc<Fleet>, only: Option<&str>) -> Result<Vec<Reach>> {
    let (_, members) = roster_members(fleet)?;
    match only {
        None => Ok(members),
        Some(wanted) => {
            let found = members
                .iter()
                .position(|member| member.name.as_str() == wanted)
                .ok_or_else(|| unknown_member(wanted, &members))?;
            Ok(members.into_iter().skip(found).take(1).collect())
        }
    }
}

/// The master, whether it is this member, and every member, this one first. A
/// machine that has not joined a fleet is the master of a fleet of one.
pub(crate) fn roster_members(fleet: &Arc<Fleet>) -> Result<((MemberName, bool), Vec<Reach>)> {
    let roster = match fleet.roster() {
        Ok(roster) => roster,
        Err(_) if matches!(fleet.membership(), Membership::Solo) => {
            let name = fleet.presence().map(|(name, _)| name).unwrap_or_else(|| {
                crate::tools_api::name_from_host(sysinfo::System::host_name().as_deref())
            });
            return Ok(((name.clone(), true), vec![Reach { name, peer: None }]));
        }
        Err(error) => return Err(error),
    };
    let mut members: Vec<Reach> = roster
        .members()
        .iter()
        .map(|member| Reach {
            name: member.name.clone(),
            peer: (member.key != fleet.public_key()).then(|| member.clone()),
        })
        .collect();
    members.sort_by_key(|member| member.peer.is_some());
    let master = roster.master();
    Ok((
        (master.name.clone(), master.key == fleet.public_key()),
        members,
    ))
}

/// The master and every member, this one first, when this member is the master.
fn fleet_members(fleet: &Arc<Fleet>) -> Result<(MemberName, Vec<Reach>)> {
    let ((master, this_member), members) = roster_members(fleet)?;
    if !this_member {
        return Err(Failure::new(
            "not_master",
            format!(
                "Only the master, {master}, plans the fleet: pins are judged against what it has installed."
            ),
            format!(
                "Run elsecall tools plan on {master}, or elsecall run --member {master} -- elsecall tools plan."
            ),
        ));
    }
    Ok((master, members))
}

/// A member that could not report its workspaces, with the fleet inventory's
/// codes for the same transport failure. The text is written here.
fn unreported(member: &MemberName, failure: &Failure) -> Failure {
    let code = fleet_tool_inventory::unavailable_member(member, failure).code;
    let hint = match code.as_str() {
        "version_skew" | "not_found" => {
            "Run elsecall fleet upgrade on the master so every member runs its build, then retry."
                .to_owned()
        }
        _ => format!("Check {member} with elsecall fleet (elsecall doctor there), then retry."),
    };
    Failure::new(
        &code,
        format!("{member} could not report the workspaces its runs started in."),
        hint,
    )
}

fn unknown_member(member: &str, members: &[Reach]) -> Failure {
    let listed = members
        .iter()
        .map(|member| member.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    Failure::new(
        "invalid_request",
        format!("--member {member:?} is not a member of this fleet; its members are {listed}."),
        "Run elsecall fleet to list members, then retry.",
    )
}
