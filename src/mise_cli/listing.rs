//! Parse `mise ls --json` and `mise ls --global --json` from mise 2026.10.11.
//!
//! Both commands are run with the working directory set to the home directory,
//! so a project config is not loaded. Each document is an object keyed by the
//! tool's mise identifier. The value is an array of records.
//!
//! An installed version is a record with `installed: true`. `version` is the
//! exact version on disk, not `requested_version` (the request as written, such
//! as `22`). A record with `symlinked_to` is installed when `installed` is true;
//! mise 2026.10.11 reports the symlinked `rust` install that way. Records with
//! `installed: false` are configured but not present and are left out.
//! `install_path`, `source` and `active` on the full listing are not used.
//!
//! The default is the version the global config selects: the same tool and
//! `version` in `mise ls --global --json` with `installed: true` and
//! `active: true`. Two such versions of one tool are rejected.
use crate::tool_inventory::{InstalledTool, InventoryFailure, ToolName, ToolVersion};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Deserialize)]
struct Record {
    version: String,
    installed: bool,
    #[serde(default)]
    active: bool,
}

pub(super) fn installed_tools(
    listing: &[u8],
    global: &[u8],
) -> Result<Vec<InstalledTool>, InventoryFailure> {
    let installed = parse(listing, "mise ls --json")?;
    let global = parse(global, "mise ls --global --json")?;
    let defaults = defaults(&global)?;
    let mut tools = Vec::new();
    let mut seen = BTreeSet::new();
    for (name, records) in installed {
        for record in records {
            if !record.installed {
                continue;
            }
            let Some(name) = ToolName::new(&name) else {
                return Err(InventoryFailure::output_invalid(
                    "mise ls --json",
                    "a tool name was empty or contained whitespace",
                ));
            };
            let Some(version) = ToolVersion::new(&record.version) else {
                return Err(InventoryFailure::output_invalid(
                    "mise ls --json",
                    "a version was empty or contained whitespace",
                ));
            };
            if !seen.insert((name.as_str().to_owned(), version.as_str().to_owned())) {
                continue;
            }
            let default = defaults
                .get(name.as_str())
                .is_some_and(|selected| selected == version.as_str());
            tools.push(InstalledTool {
                name,
                version,
                default,
                detail: None,
            });
        }
    }
    Ok(tools)
}

fn parse(bytes: &[u8], command: &str) -> Result<BTreeMap<String, Vec<Record>>, InventoryFailure> {
    serde_json::from_slice(bytes).map_err(|_| {
        InventoryFailure::output_invalid(command, "the listing was not the expected JSON object")
    })
}

fn defaults(
    global: &BTreeMap<String, Vec<Record>>,
) -> Result<BTreeMap<String, String>, InventoryFailure> {
    let mut selected = BTreeMap::new();
    for (name, records) in global {
        for record in records {
            if !(record.installed && record.active) {
                continue;
            }
            if ToolName::new(name).is_none() || ToolVersion::new(&record.version).is_none() {
                return Err(InventoryFailure::output_invalid(
                    "mise ls --global --json",
                    "a tool name or version was empty or contained whitespace",
                ));
            }
            match selected.get(name.as_str()) {
                Some(existing) if existing != &record.version => {
                    return Err(InventoryFailure::output_invalid(
                        "mise ls --global --json",
                        &format!("more than one default version of {name}"),
                    ));
                }
                Some(_) => {}
                None => {
                    selected.insert(name.clone(), record.version.clone());
                }
            }
        }
    }
    Ok(selected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_inventory::{Executable, Manager, ManagerInventory};

    fn inventory(listing: &[u8], global: &[u8]) -> ManagerInventory {
        ManagerInventory::try_new(
            Manager::Mise,
            Executable::Missing,
            installed_tools(listing, global).unwrap(),
        )
        .unwrap()
    }

    fn versions(inventory: &ManagerInventory, name: &str) -> Vec<(String, bool)> {
        inventory
            .tools
            .iter()
            .filter(|tool| tool.name.as_str() == name)
            .map(|tool| (tool.version.as_str().to_owned(), tool.default))
            .collect()
    }

    #[test]
    fn parses_this_machines_mise_listings() {
        let listing = include_str!("../../tests/fixtures/mise/ls.json");
        let global = include_str!("../../tests/fixtures/mise/ls-global.json");
        assert!(listing.contains("/Users/me"));
        assert!(global.contains("/Users/me"));
        let inventory = inventory(listing.as_bytes(), global.as_bytes());
        assert_eq!(inventory.tools.len(), 88);
        let defaults: Vec<_> = inventory
            .tools
            .iter()
            .filter(|tool| tool.default)
            .map(|tool| format!("{} {}", tool.name.as_str(), tool.version.as_str()))
            .collect();
        assert_eq!(
            defaults,
            vec![
                "java zulu-17.54.21",
                "node 22.19.0",
                "packslip:example/acme 0.12.0",
                "packslip:github.com/example/acme-ios 0.2.0",
                "ruby 3.2.0",
            ]
        );
        assert_eq!(versions(&inventory, "rust"), vec![("1.99.0".into(), false)]);
        assert_eq!(
            versions(&inventory, "bun"),
            vec![
                ("1.2.13".into(), false),
                ("1.3.5".into(), false),
                ("1.3.14".into(), false),
            ]
        );
        let order: Vec<_> = inventory
            .tools
            .iter()
            .map(|tool| {
                (
                    tool.name.as_str().to_owned(),
                    tool.version.as_str().to_owned(),
                )
            })
            .collect();
        let mut sorted = order.clone();
        sorted.sort_by(|left, right| {
            left.0
                .cmp(&right.0)
                .then_with(|| crate::tool_inventory::compare_versions(&left.1, &right.1))
        });
        assert_eq!(order, sorted);
    }

    #[test]
    fn an_empty_object_and_uninstalled_entries_list_no_tools() {
        assert!(inventory(b"{}", b"{}").tools.is_empty());
        let listing = br#"{"node":[{"version":"22.19.0","installed":false,"active":true}],"tuist":[{"version":"4.206.0","installed":false}]}"#;
        assert!(inventory(listing, listing).tools.is_empty());
    }

    #[test]
    fn only_the_active_installed_global_version_is_the_default() {
        let listing = br#"{"node":[
            {"version":"20.19.5","installed":true,"active":false},
            {"version":"22.19.0","installed":true,"active":true},
            {"version":"99.0.0","installed":false,"active":true}
        ]}"#;
        let global = br#"{"node":[
            {"version":"20.19.5","installed":true,"active":false},
            {"version":"22.19.0","requested_version":"22","installed":true,"active":true},
            {"version":"99.0.0","installed":false,"active":true}
        ],"tuist":[{"version":"4.206.0","installed":false,"active":true}]}"#;
        assert_eq!(
            versions(&inventory(listing, global), "node"),
            vec![("20.19.5".into(), false), ("22.19.0".into(), true)]
        );
    }

    #[test]
    fn malformed_json_and_two_defaults_are_invalid_output() {
        let error = installed_tools(b"not-json SECRET-TOKEN", b"{}").unwrap_err();
        assert_eq!(error.code(), "tool_manager_output_invalid");
        assert!(!error.message().contains("SECRET-TOKEN"));
        assert!(installed_tools(b"[]", b"{}").is_err());
        assert!(installed_tools(br#"{"node":{}}"#, b"{}").is_err());
        let listing = br#"{"node":[
            {"version":"20.19.5","installed":true},
            {"version":"22.19.0","installed":true}
        ]}"#;
        let global = br#"{"node":[
            {"version":"20.19.5","installed":true,"active":true},
            {"version":"22.19.0","installed":true,"active":true}
        ]}"#;
        let error = installed_tools(listing, global).unwrap_err();
        assert_eq!(error.code(), "tool_manager_output_invalid");
        assert!(!error.message().contains("20.19.5"));
        let bad_version = br#"{"node":[{"version":"22 19","installed":true}]}"#;
        assert_eq!(
            installed_tools(bad_version, b"{}").unwrap_err().code(),
            "tool_manager_output_invalid"
        );
    }
}
