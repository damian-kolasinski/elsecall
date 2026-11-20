//! The plan as `elsecall tools plan` prints it.
use crate::{
    fleet_tool_inventory::MemberUnavailable,
    pins::SourceFile,
    plan::{
        member::{Applied, Installed, MemberPlan, Pending, Step},
        wants::{Reason, ToolKey},
    },
    tool_inventory::{InventoryFailure, Manager},
    toolset::document::Exception,
};
use elsecall_contract::{failure::Failure, scheduling_model::timestamp::Timestamp};
use elsecall_peers::roster::MemberName;
use serde_json::{Value, json};

/// The master's global mise config: read (and the file it read, if any), or failed.
pub(crate) type GlobalRead = Result<Vec<SourceFile>, Failure>;

pub(crate) struct WorkspaceRead {
    pub(crate) member: MemberName,
    pub(crate) workspace: String,
    pub(crate) last_run_at: Timestamp,
    pub(crate) files: Result<Vec<SourceFile>, Failure>,
}

pub(crate) struct Document {
    pub(crate) master: MemberName,
    pub(crate) global: GlobalRead,
    pub(crate) workspaces: Vec<WorkspaceRead>,
    pub(crate) unread_members: Vec<(MemberName, Failure)>,
    pub(crate) warnings: Vec<(Manager, InventoryFailure)>,
    pub(crate) members: Vec<(MemberName, Result<MemberPlan, MemberUnavailable>)>,
}

impl Document {
    pub(crate) fn to_json(&self) -> Value {
        json!({
            "master": self.master,
            "global": match &self.global {
                Ok(files) => json!({
                    "file": "~/.config/mise/config.toml",
                    "state": if files.is_empty() { "missing" } else { "read" },
                }),
                Err(error) => json!({
                    "file": "~/.config/mise/config.toml",
                    "state": "failed",
                    "error": error,
                }),
            },
            "workspaces": self.workspaces.iter().map(workspace).collect::<Vec<_>>(),
            "unread_members": self
                .unread_members
                .iter()
                .map(|(member, error)| json!({"member": member, "error": error}))
                .collect::<Vec<_>>(),
            "warnings": self
                .warnings
                .iter()
                .map(|(manager, error)| json!({"manager": manager, "error": error}))
                .collect::<Vec<_>>(),
            "members": self
                .members
                .iter()
                .map(|(member, plan)| match plan {
                    Ok(plan) => member_plan(&self.master, member, plan),
                    Err(error) => json!({"member": member, "state": "unavailable", "error": error}),
                })
                .collect::<Vec<_>>(),
        })
    }
}

fn workspace(read: &WorkspaceRead) -> Value {
    let mut value = json!({
        "member": read.member,
        "workspace": read.workspace,
        "last_run_at": read.last_run_at,
    });
    match &read.files {
        Ok(files) => {
            value["state"] = json!("read");
            value["files"] = json!(files.iter().map(|f| f.file.as_str()).collect::<Vec<_>>());
        }
        Err(error) => {
            value["state"] = json!("failed");
            value["error"] = json!(error);
        }
    }
    value
}

fn member_plan(master: &MemberName, member: &MemberName, plan: &MemberPlan) -> Value {
    json!({
        "member": member,
        "state": "planned",
        "steps": plan.steps.iter().map(|s| step(master, s)).collect::<Vec<_>>(),
        "unresolved": plan.unresolved.iter().map(|p| pending(master, p)).collect::<Vec<_>>(),
        "exceptions": plan.exceptions.iter().map(applied).collect::<Vec<_>>(),
        "installed": plan.installed.iter().map(installed).collect::<Vec<_>>(),
        "unread_managers": plan
            .unread
            .iter()
            .map(|(manager, error)| json!({"manager": manager, "error": error}))
            .collect::<Vec<_>>(),
    })
}

fn tool(key: &ToolKey) -> (Value, Value) {
    (json!(key.manager), json!(key.tool))
}

fn step(master: &MemberName, step: &Step) -> Value {
    let (manager, tool) = tool(&step.key);
    let mut value = json!({
        "manager": manager,
        "tool": tool,
        "version": step.version,
        "exact": step.exact,
        "reasons": step.reasons.iter().map(reason).collect::<Vec<_>>(),
        "options": step.options,
    });
    if !step.exact {
        value["hint"] = json!(format!(
            "Planned at the version an inexact pin resolves to on the master, {master}; pin it exactly so every member installs the same build."
        ));
    }
    value
}

fn pending(master: &MemberName, pending: &Pending) -> Value {
    let (manager, tool) = tool(&pending.key);
    json!({
        "manager": manager,
        "tool": tool,
        "version": pending.written,
        "hint": pending.why.hint(master),
        "reasons": pending.reasons.iter().map(reason).collect::<Vec<_>>(),
    })
}

fn applied(applied: &Applied) -> Value {
    let exception = &applied.exception;
    let mut value = json!({
        "manager": exception.manager,
        "tool": exception.tool,
        "key": exception.key,
        "pinned": applied.pinned,
    });
    match &exception.exception {
        Exception::LeaveOut => value["kind"] = json!("leave_out"),
        Exception::Versions(versions) => {
            value["kind"] = json!("versions");
            value["default"] = json!(versions.default());
            value["install"] = json!(versions.install().collect::<Vec<_>>());
        }
    }
    value
}

fn installed(installed: &Installed) -> Value {
    let (manager, tool) = tool(&installed.key);
    json!({"manager": manager, "tool": tool, "version": installed.version})
}

fn reason(reason: &Reason) -> Value {
    match reason {
        Reason::Pin {
            member,
            workspace,
            source,
            version,
            environment,
        } => json!({
            "kind": "pin",
            "member": member,
            "workspace": workspace,
            "source": source.kind.as_str(),
            "file": source.file,
            "version": version,
            "environment": environment.as_ref().map(|e| e.as_str()),
        }),
        Reason::DefaultRuntime {
            member,
            workspace,
            xcode,
        } => json!({
            "kind": "default_runtime",
            "member": member,
            "workspace": workspace,
            "xcode": xcode,
        }),
        Reason::Exception { key } => json!({
            "kind": "exception",
            "file": "fleet-tools.toml",
            "key": key,
        }),
        Reason::ExceptionRuntime { key, xcode } => json!({
            "kind": "exception_runtime",
            "file": "fleet-tools.toml",
            "key": key,
            "xcode": xcode,
        }),
    }
}
