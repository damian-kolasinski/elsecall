//! `elsecall tools`: inventory and the exceptions to the pins, through the local
//! service, and a workspace's pins, read here without the service.
//!
//! Without flags inventory is this machine. `--fleet` is every member. `--member`
//! is one member's entry from that same fleet document.
use crate::{
    tool_inventory::{Manager, ToolName},
    tools_api,
    toolset_api::{self, DesiredEdit, EditBody},
};
use clap::{Args, Subcommand};
use elsecall_contract::failure::{Failure, Result};
use elsecall_host::client;
use elsecall_host::paths::Paths;
use elsecall_peers::roster::MemberName;
use hyper::Method;
use serde_json::Value;
use std::{path::PathBuf, time::Duration};

/// Three mise commands and `xcodes version` each get 30 seconds, and
/// `xcode-select` gets 10. This waits longer than that, so a manager timeout is
/// part of the inventory instead of a failed service call.
const LOCAL_TIMEOUT: Duration = Duration::from_secs(150);
/// Longer than [`crate::fleet_tool_inventory::MEMBER_TIMEOUT`], which bounds
/// each other member. The members are asked together, so the wait is the
/// slowest of them plus a margin for this service.
pub(crate) const FLEET_TIMEOUT: Duration = Duration::from_secs(170);

#[derive(Args)]
pub struct ToolsArgs {
    #[command(subcommand)]
    command: ToolsCommand,
}

#[derive(Subcommand)]
enum ToolsCommand {
    /// List installed tools and which version is the default
    Inventory {
        /// Every member of the fleet. A machine that has not joined is a fleet of one
        #[arg(long)]
        fleet: bool,
        /// One member's inventory, by its fleet name
        #[arg(long, value_name = "NAME")]
        member: Option<String>,
    },
    /// List the tool versions a workspace pins, read without running its code
    Pins {
        /// The workspace, usually a repository's root. Defaults to the current directory
        path: Option<PathBuf>,
    },
    /// On the master, plan each member: what it lacks of what the fleet's workspaces pin.
    /// Read-only; nothing is installed
    Plan {
        /// Plan only this member; every member's workspaces still count
        #[arg(long, value_name = "NAME")]
        member: Option<String>,
    },
    /// On the master, install each member's plan: what `tools plan` lists, as admitted
    /// work on each member. Nothing is ever uninstalled
    Apply {
        /// Apply only this member's plan; every member's workspaces still count
        #[arg(long, value_name = "NAME")]
        member: Option<String>,
        /// Print the plan and install nothing
        #[arg(long)]
        dry_run: bool,
        /// Wait until the started members finish, then print what each installed
        #[arg(long, conflicts_with = "dry_run")]
        wait: bool,
    },
    /// On the master, what the provisioning loop knows of each member: whether it
    /// is in step with the pins, applying, waiting or needs a person
    Provisioning {
        /// Only this member
        #[arg(long, value_name = "NAME")]
        member: Option<String>,
    },
    /// On the master, plan and apply one member's tools now instead of at the
    /// loop's next look; it is marked ready once nothing is left to install
    Provision {
        /// The member, by its fleet name
        member: String,
    },
    /// Show what each member's latest apply installed, or is installing
    Applied {
        /// Only this member
        #[arg(long, value_name = "NAME")]
        member: Option<String>,
        /// Wait until no member is still installing
        #[arg(long)]
        wait: bool,
    },
    /// Whether this account may place, select and license Xcode through Elsecall's
    /// helper, and the one-time step a person takes to let it
    Privileges {
        #[command(subcommand)]
        command: Option<crate::privileges_cli::PrivilegesCommand>,
    },
    /// Show the exceptions to the pins in fleet-tools.toml for every member, or one
    Exceptions {
        /// Only this member
        #[arg(long)]
        member: Option<MemberName>,
    },
    /// On the master, leave a tool's pins out on every member, or one
    LeaveOut {
        /// `mise:<name>`, `xcode`, or `simulators:<platform>` such as `simulators:iOS`
        tool: String,
        /// Leave it out on this member only
        #[arg(long)]
        member: Option<MemberName>,
    },
    /// On the master, keep versions on one member instead of what the pins ask for
    Set {
        /// `mise:<name>`, `xcode`, or `simulators:<platform>`
        tool: String,
        /// Exact version, such as 22.19.0
        version: String,
        /// The member that differs
        #[arg(long)]
        member: MemberName,
        /// Add the version beside the default instead of making it the default
        #[arg(long)]
        install_only: bool,
    },
    /// On the master, remove an exception, or one version of a member's exception
    Unset {
        /// `mise:<name>`, `xcode`, or `simulators:<platform>`
        tool: String,
        /// Remove only this version from default and install
        #[arg(long)]
        version: Option<String>,
        /// Change this member's exception instead of the one for every member
        #[arg(long)]
        member: Option<MemberName>,
    },
}

struct Target {
    manager: Manager,
    name: String,
}

pub async fn run(
    paths: &Paths,
    args: ToolsArgs,
    root: &dyn crate::privileges_cli::RootShell,
) -> Result<Value> {
    match args.command {
        ToolsCommand::Inventory { fleet, member } => {
            if let Some(member) = member {
                let data = client::send_bounded(
                    &paths.socket(),
                    Method::GET,
                    &member_route(&member),
                    FLEET_TIMEOUT,
                    crate::fleet_tool_inventory::FLEET_RESPONSE_LIMIT,
                )
                .await?;
                return one_member(data);
            }
            if fleet {
                return client::send_bounded(
                    &paths.socket(),
                    Method::GET,
                    tools_api::FLEET_ROUTE,
                    FLEET_TIMEOUT,
                    crate::fleet_tool_inventory::FLEET_RESPONSE_LIMIT,
                )
                .await;
            }
            client::send_large(
                &paths.socket(),
                Method::GET,
                tools_api::LOCAL_ROUTE,
                LOCAL_TIMEOUT,
            )
            .await
        }
        ToolsCommand::Pins { path } => {
            let path = path.unwrap_or_else(|| PathBuf::from("."));
            let report = tokio::task::spawn_blocking(move || crate::pins::read_workspace(&path))
                .await
                .map_err(|_| {
                    Failure::new(
                        "internal_error",
                        "Reading the workspace's pins stopped unexpectedly.",
                        "Run the command again.",
                    )
                })??;
            Ok(serde_json::to_value(report).expect("serializable pins"))
        }
        ToolsCommand::Plan { member } => {
            let route = match &member {
                Some(name) => format!("{}?member={}", crate::plan_api::ROUTE, escaped(name)),
                None => crate::plan_api::ROUTE.to_owned(),
            };
            client::send_bounded(
                &paths.socket(),
                Method::GET,
                &route,
                FLEET_TIMEOUT,
                crate::fleet_tool_inventory::FLEET_RESPONSE_LIMIT,
            )
            .await
        }
        ToolsCommand::Apply {
            member,
            dry_run,
            wait,
        } => crate::apply_cli::apply(paths, member, dry_run, wait).await,
        ToolsCommand::Provisioning { member } => {
            let route = match &member {
                Some(name) => format!(
                    "{}?member={}",
                    crate::provisioning_api::ROUTE,
                    escaped(name)
                ),
                None => crate::provisioning_api::ROUTE.to_owned(),
            };
            client::get_large(&paths.socket(), &route).await
        }
        ToolsCommand::Provision { member } => {
            client::post(
                &paths.socket(),
                crate::provisioning_api::ROUTE,
                &crate::provisioning_api::ProvisionBody { member },
                client::Wait::Bounded,
            )
            .await
        }
        ToolsCommand::Applied { member, wait } => {
            crate::apply_cli::applied(paths, member.as_deref(), wait).await
        }
        ToolsCommand::Privileges { command } => crate::privileges_cli::run(command, root).await,
        ToolsCommand::Exceptions { member } => {
            let route = match &member {
                Some(name) => format!("{}?member={name}", toolset_api::ROUTE),
                None => toolset_api::ROUTE.to_owned(),
            };
            client::get_large(&paths.socket(), &route).await
        }
        ToolsCommand::LeaveOut { tool, member } => {
            let Target { manager, name } = target(&tool)?;
            post(
                &paths.socket(),
                &DesiredEdit {
                    member,
                    edit: EditBody::LeaveOut {
                        manager,
                        tool: tool_name(&name)?,
                    },
                },
            )
            .await
        }
        ToolsCommand::Set {
            tool,
            version,
            member,
            install_only,
        } => {
            post(
                &paths.socket(),
                &DesiredEdit {
                    member: Some(member),
                    edit: set_edit(&tool, &version, install_only)?,
                },
            )
            .await
        }
        ToolsCommand::Unset {
            tool,
            version,
            member,
        } => {
            post(
                &paths.socket(),
                &DesiredEdit {
                    member,
                    edit: unset_edit(&tool, version)?,
                },
            )
            .await
        }
    }
}

fn member_route(member: &str) -> String {
    format!("{}?member={}", tools_api::FLEET_ROUTE, escaped(member))
}

/// A member name for a query: anything outside a member name is escaped.
pub(crate) fn escaped(member: &str) -> String {
    let mut escaped = String::new();
    for byte in member.bytes() {
        match byte {
            b'a'..=b'z' | b'0'..=b'9' | b'-' => escaped.push(byte as char),
            _ => escaped.push_str(&format!("%{byte:02X}")),
        }
    }
    escaped
}

fn one_member(data: Value) -> Result<Value> {
    data.get("members")
        .and_then(Value::as_array)
        .and_then(|members| members.first())
        .cloned()
        .ok_or_else(|| {
            Failure::new(
                "protocol_error",
                "The local service returned an invalid or incompatible response.",
                "Reinstall matching CLI and service versions with elsecall install.",
            )
        })
}

async fn post(socket: &std::path::Path, body: &DesiredEdit) -> Result<Value> {
    match tokio::time::timeout(
        client::TIMEOUT,
        client::post_large(socket, toolset_api::ROUTE, body),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => Err(Failure::new(
            "service_timeout",
            "The local service did not respond within three seconds.",
            "Run elsecall doctor, then elsecall service restart.",
        )),
    }
}

fn set_edit(spec: &str, version: &str, install_only: bool) -> Result<EditBody> {
    let Target { manager, name } = target(spec)?;
    let tool = tool_name(&name)?;
    let version = version.to_owned();
    Ok(if install_only {
        EditBody::AddInstall {
            manager,
            tool,
            version,
        }
    } else {
        EditBody::SetDefault {
            manager,
            tool,
            version,
        }
    })
}

fn unset_edit(spec: &str, version: Option<String>) -> Result<EditBody> {
    let Target { manager, name } = target(spec)?;
    Ok(EditBody::Unset {
        manager,
        tool: tool_name(&name)?,
        version,
    })
}

fn target(spec: &str) -> Result<Target> {
    let named = |manager, name: &str| {
        if name.is_empty() {
            Err(bad_tool(spec))
        } else {
            Ok(Target {
                manager,
                name: name.to_owned(),
            })
        }
    };
    match spec {
        "mise" | "xcodes" => Err(Failure::new(
            "invalid_request",
            format!(
                "fleet-tools.toml holds only exceptions to the pins and no longer sets the {spec} executable."
            ),
            "Name a tool such as mise:node, xcode or simulators:iOS.",
        )),
        "xcode" => named(Manager::Xcodes, "xcode"),
        other => {
            for (prefix, manager) in [
                ("mise:", Manager::Mise),
                ("xcodes:", Manager::Xcodes),
                ("simulators:", Manager::Simulators),
            ] {
                if let Some(name) = other.strip_prefix(prefix) {
                    return named(manager, name);
                }
            }
            Err(bad_tool(spec))
        }
    }
}

fn tool_name(name: &str) -> Result<String> {
    ToolName::new(name)
        .map(|name| name.as_str().to_owned())
        .ok_or_else(|| {
            Failure::new(
                "invalid_request",
                "Tool names cannot be empty or contain whitespace or control characters.",
                "Name a tool such as mise:node, xcode or simulators:iOS.",
            )
        })
}

fn bad_tool(spec: &str) -> Failure {
    Failure::new(
        "invalid_request",
        format!("Unknown tool {spec}."),
        "Use mise:<name> for a mise tool, xcode, or simulators:<platform> such as simulators:iOS.",
    )
}

#[cfg(test)]
mod tests {
    use crate::tools_cli::*;

    #[test]
    fn the_fleet_command_waits_longer_than_one_member() {
        assert!(FLEET_TIMEOUT > crate::fleet_tool_inventory::MEMBER_TIMEOUT);
    }

    #[test]
    fn a_member_query_escapes_anything_outside_a_member_name() {
        assert_eq!(
            member_route("bravo"),
            "/v1/tools/fleet-inventory?member=bravo"
        );
        assert_eq!(
            member_route("Nope"),
            "/v1/tools/fleet-inventory?member=%4Eope"
        );
        assert_eq!(
            member_route("a b"),
            "/v1/tools/fleet-inventory?member=a%20b"
        );
    }

    #[test]
    fn a_malformed_tool_name_is_an_invalid_request() {
        for spec in ["mise:foo bar", "xcodes:foo bar", "mise:\u{0001}node"] {
            let set = set_edit(spec, "22.19.0", false).unwrap_err();
            assert_eq!(set.code, "invalid_request", "{spec}");
            let unset = unset_edit(spec, None).unwrap_err();
            assert_eq!(unset.code, "invalid_request", "{spec}");
        }
        assert!(set_edit("xcodes:node", "16.4", false).is_ok());
        assert!(unset_edit("xcodes:node", None).is_ok());
        assert!(set_edit("mise:node", "22", true).is_ok());
        assert!(set_edit("mise:java", "zulu-17", false).is_ok());
        assert!(set_edit("simulators:iOS", "26.2", false).is_ok());
    }

    #[test]
    fn manager_executables_are_no_longer_exceptions() {
        for spec in ["mise", "xcodes"] {
            let error = set_edit(spec, "1.0.0", false).unwrap_err();
            assert_eq!(error.code, "invalid_request");
            assert!(error.message.contains("only exceptions"), "{error}");
            assert!(unset_edit(spec, None).is_err());
        }
    }
}
