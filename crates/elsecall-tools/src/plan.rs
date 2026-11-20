//! The plan per member: what each member lacks of what the fleet's workspaces
//! pin. See `docs/fleet-provisioning.md`.
//!
//! The planner joins the two ports without either knowing the other: pins
//! (`PinSource`), read by each member from the workspaces its runs started in
//! ([`reported`]) and from the master's global mise config, are judged on the
//! master ([`wants`]); then, for each member, its exceptions in
//! `fleet-tools.toml` are applied and what its tool managers
//! (`ToolManager`) report as installed is taken away ([`member`]). Nothing here
//! installs anything.
pub(crate) mod document;
pub(crate) mod member;
pub(crate) mod reported;
pub(crate) mod wants;

use crate::{
    fleet_tool_inventory::{FleetInventory, MemberInventory},
    pins::Reading,
    tool_inventory::{InventoryFailure, Manager, ManagerReport},
    toolset::document::Toolset,
};
use document::{Document, WorkspaceRead};
use elsecall_contract::failure::Failure;
use elsecall_peers::roster::MemberName;
use reported::WorkspaceReport;
use wants::{MasterTools, Origin, Want};

/// Everything the plan is made of, already read.
pub(crate) struct Inputs {
    pub(crate) master: MemberName,
    pub(crate) global: Result<Reading, Failure>,
    pub(crate) reports: Vec<(MemberName, Result<WorkspaceReport, Failure>)>,
    pub(crate) inventories: FleetInventory,
    pub(crate) toolset: Toolset,
    /// Plan only these members; every member's workspaces still count.
    pub(crate) only: Option<Vec<MemberName>>,
}

pub(crate) fn build(inputs: Inputs) -> Document {
    let mut warnings = Vec::new();
    let master = master_tools(&inputs.master, &inputs.inventories, &mut warnings);
    let mut wants: Vec<Want> = Vec::new();
    if let Ok(reading) = &inputs.global {
        let origin = Origin {
            member: inputs.master.clone(),
            workspace: None,
        };
        wants.extend(wants::wants(&master, &origin, &reading.pins));
    }
    let mut workspaces = Vec::new();
    let mut unread_members = Vec::new();
    for (member, report) in &inputs.reports {
        let report = match report {
            Ok(report) => report,
            Err(error) => {
                unread_members.push((member.clone(), error.clone()));
                continue;
            }
        };
        for reported in &report.workspaces {
            if let Ok(reading) = &reported.pins {
                let origin = Origin {
                    member: member.clone(),
                    workspace: Some(reported.workspace.clone()),
                };
                wants.extend(wants::wants(&master, &origin, &reading.pins));
            }
            workspaces.push(WorkspaceRead {
                member: member.clone(),
                workspace: reported.workspace.clone(),
                last_run_at: reported.last_run_at,
                files: reported
                    .pins
                    .as_ref()
                    .map(|reading| reading.files.clone())
                    .map_err(Clone::clone),
            });
        }
    }
    let members = inputs
        .inventories
        .members
        .iter()
        .filter(|entry| {
            inputs
                .only
                .as_ref()
                .is_none_or(|only| only.contains(member_of(entry)))
        })
        .map(|entry| match entry {
            MemberInventory::Read { member, inventory } => (
                member.clone(),
                Ok(member::plan_member(
                    &wants,
                    &inputs.toolset.exceptions(member),
                    inventory,
                )),
            ),
            MemberInventory::Unavailable { member, failure } => {
                (member.clone(), Err(failure.clone()))
            }
        })
        .collect();
    Document {
        master_xcodes: master.xcodes.clone(),
        master: inputs.master,
        global: inputs.global.map(|reading| reading.files),
        workspaces,
        unread_members,
        warnings,
        members,
    }
}

fn member_of(entry: &MemberInventory) -> &MemberName {
    match entry {
        MemberInventory::Read { member, .. } | MemberInventory::Unavailable { member, .. } => {
            member
        }
    }
}

/// What the master has installed. A manager it could not read is a warning, and
/// pins that need it stay unresolved.
fn master_tools(
    master: &MemberName,
    inventories: &FleetInventory,
    warnings: &mut Vec<(Manager, InventoryFailure)>,
) -> MasterTools {
    let mut tools = MasterTools {
        mise: None,
        xcodes: None,
    };
    let inventory = inventories.members.iter().find_map(|entry| match entry {
        MemberInventory::Read { member, inventory } if member == master => Some(inventory),
        _ => None,
    });
    let Some(inventory) = inventory else {
        for manager in [Manager::Mise, Manager::Xcodes] {
            warnings.push((
                manager,
                InventoryFailure::failed("the master's own inventory could not be read"),
            ));
        }
        return tools;
    };
    for report in &inventory.managers {
        match report {
            ManagerReport::Read(read) if read.manager == Manager::Mise => {
                tools.mise = Some(
                    read.tools
                        .iter()
                        .map(|tool| (tool.name.clone(), tool.version.clone()))
                        .collect(),
                );
            }
            ManagerReport::Read(read) if read.manager == Manager::Xcodes => {
                tools.xcodes = Some(read.tools.iter().map(|tool| tool.version.clone()).collect());
            }
            ManagerReport::Read(_) => {}
            ManagerReport::Failed { manager, failure } => {
                if *manager != Manager::Simulators {
                    warnings.push((*manager, failure.clone()));
                }
            }
        }
    }
    tools
}
