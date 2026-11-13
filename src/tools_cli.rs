//! `elsecall tools`: inventory and the fleet's desired versions, through the local service.
//!
//! Without flags this is this machine. `--fleet` is every member. `--member`
//! is one member's entry from that same fleet document.
use crate::{
    client,
    failure::{Failure, Result},
    paths::Paths,
    roster::MemberName,
    tool_inventory::{Manager, ToolName},
    tools_api,
    toolset_api::{DesiredEdit, EditBody},
};
use clap::{Args, Subcommand};
use hyper::Method;
use serde_json::Value;
use std::time::Duration;

/// Three mise commands and `xcodes version` each get 30 seconds, and
/// `xcode-select` gets 10. This waits longer than that, so a manager timeout is
/// part of the inventory instead of a failed service call.
const LOCAL_TIMEOUT: Duration = Duration::from_secs(150);
/// Longer than [`crate::fleet_tool_inventory::MEMBER_TIMEOUT`], which bounds
/// each other member. The members are asked together, so the wait is the
/// slowest of them plus a margin for this service.
const FLEET_TIMEOUT: Duration = Duration::from_secs(170);

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
    /// Show desired tool versions for every member, or one
    Desired {
        /// Resolve only this member
        #[arg(long)]
        member: Option<MemberName>,
    },
    /// On the master, set a desired version in fleet-tools.toml
    Set {
        /// `mise`, `xcodes`, `mise:<name>`, or `xcode`
        tool: String,
        /// Exact version, such as 22.19.0
        version: String,
        /// Write this member's overlay instead of the fleet default
        #[arg(long)]
        member: Option<MemberName>,
        /// Add the version beside the default instead of making it the default
        #[arg(long)]
        install_only: bool,
    },
    /// On the master, remove a desired version or a whole entry
    Unset {
        /// `mise`, `xcodes`, `mise:<name>`, or `xcode`
        tool: String,
        /// Remove only this version from default and install
        #[arg(long)]
        version: Option<String>,
        /// Change this member's overlay instead of the fleet default
        #[arg(long)]
        member: Option<MemberName>,
    },
}

enum Target {
    Executable(Manager),
    Tool { manager: Manager, name: String },
}

pub async fn run(paths: &Paths, args: ToolsArgs) -> Result<Value> {
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
        ToolsCommand::Desired { member } => {
            let route = match &member {
                Some(name) => format!("/v1/tools/desired?member={name}"),
                None => "/v1/tools/desired".to_owned(),
            };
            client::get_large(&paths.socket(), &route).await
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
                    member,
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
    let mut route = String::from(tools_api::FLEET_ROUTE);
    route.push_str("?member=");
    for byte in member.bytes() {
        match byte {
            b'a'..=b'z' | b'0'..=b'9' | b'-' => route.push(byte as char),
            _ => route.push_str(&format!("%{byte:02X}")),
        }
    }
    route
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
        client::post_large(socket, "/v1/tools/desired", body),
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
    match target(spec)? {
        Target::Executable(manager) => {
            if install_only {
                return Err(Failure::new(
                    "invalid_request",
                    "The manager executable has one version; --install-only applies to a tool.",
                    "Drop --install-only to set mise or xcodes, or name a tool such as mise:node or xcode.",
                ));
            }
            Ok(EditBody::SetExecutable {
                manager,
                version: version.to_owned(),
            })
        }
        Target::Tool { manager, name } => {
            let tool = tool_name(&name)?;
            if install_only {
                Ok(EditBody::AddInstall {
                    manager,
                    tool,
                    version: version.to_owned(),
                })
            } else {
                Ok(EditBody::SetDefault {
                    manager,
                    tool,
                    version: version.to_owned(),
                })
            }
        }
    }
}

fn unset_edit(spec: &str, version: Option<String>) -> Result<EditBody> {
    match target(spec)? {
        Target::Executable(manager) => {
            if version.is_some() {
                return Err(Failure::new(
                    "invalid_request",
                    "The manager executable has one version; --version applies to a tool.",
                    "Drop --version to clear mise or xcodes, or name a tool such as mise:node or xcode.",
                ));
            }
            Ok(EditBody::UnsetExecutable { manager })
        }
        Target::Tool { manager, name } => Ok(EditBody::Unset {
            manager,
            tool: tool_name(&name)?,
            version,
        }),
    }
}

fn target(spec: &str) -> Result<Target> {
    match spec {
        "mise" => Ok(Target::Executable(Manager::Mise)),
        "xcodes" => Ok(Target::Executable(Manager::Xcodes)),
        "xcode" => Ok(Target::Tool {
            manager: Manager::Xcodes,
            name: "xcode".into(),
        }),
        other => {
            if let Some(name) = other.strip_prefix("mise:") {
                return named(Manager::Mise, name, spec);
            }
            if let Some(name) = other.strip_prefix("xcodes:") {
                return named(Manager::Xcodes, name, spec);
            }
            Err(bad_tool(spec))
        }
    }
}

fn named(manager: Manager, name: &str, spec: &str) -> Result<Target> {
    if name.is_empty() {
        return Err(bad_tool(spec));
    }
    Ok(Target::Tool {
        manager,
        name: name.to_owned(),
    })
}

fn tool_name(name: &str) -> Result<String> {
    ToolName::new(name)
        .map(|name| name.as_str().to_owned())
        .ok_or_else(|| {
            Failure::new(
                "invalid_request",
                "Tool names cannot be empty or contain whitespace or control characters.",
                "Name a tool such as mise:node or xcode.",
            )
        })
}

fn bad_tool(spec: &str) -> Failure {
    Failure::new(
        "invalid_request",
        format!("Unknown tool {spec}."),
        "Use mise or xcodes for the manager, mise:<name> for a mise tool, or xcode.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

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
    }
}
