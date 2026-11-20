//! Edits to the text of `fleet-tools.toml`, which holds exceptions to the pins.
//!
//! `toml_edit` changes one key and leaves every other comment and table alone.
//! The result is parsed again and returned only when the whole document validates.
use crate::{
    tool_inventory::{Manager, ToolName, ToolVersion},
    toolset::document::{Toolset, invalid},
};
use elsecall_contract::failure::Result;
use elsecall_peers::roster::MemberName;
use toml_edit::{DocumentMut, Entry, Item, Table, Value, value};

/// Where an edit is written. A member scope replaces defaults for that member only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Scope {
    Defaults,
    Member(MemberName),
}

/// One change to the exceptions. Only a member scope names versions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Edit {
    /// An empty entry: nothing the pins ask for this tool is planned.
    LeaveOut { manager: Manager, tool: ToolName },
    SetDefault {
        manager: Manager,
        tool: ToolName,
        version: ToolVersion,
    },
    AddInstall {
        manager: Manager,
        tool: ToolName,
        version: ToolVersion,
    },
    Unset {
        manager: Manager,
        tool: ToolName,
        version: Option<ToolVersion>,
    },
}

struct ToolState {
    default: Option<String>,
    install: Vec<String>,
}

pub(crate) fn apply(text: &str, scope: &Scope, edit: &Edit) -> Result<String> {
    let mut doc: DocumentMut = text
        .parse()
        .map_err(|_| invalid("fleet-tools.toml", "the document must be TOML"))?;
    let changed = match edit {
        Edit::LeaveOut { manager, tool } => leave_out(&mut doc, scope, *manager, tool)?,
        Edit::SetDefault {
            manager,
            tool,
            version,
        } => set_default(&mut doc, scope, *manager, tool, version)?,
        Edit::AddInstall {
            manager,
            tool,
            version,
        } => add_install(&mut doc, scope, *manager, tool, version)?,
        Edit::Unset {
            manager,
            tool,
            version,
        } => unset(&mut doc, scope, *manager, tool, version.as_ref())?,
    };
    let updated = if changed {
        doc.to_string()
    } else {
        text.to_owned()
    };
    Toolset::parse(updated.as_bytes())?;
    Ok(updated)
}

fn leave_out(
    doc: &mut DocumentMut,
    scope: &Scope,
    manager: Manager,
    tool: &ToolName,
) -> Result<bool> {
    if let Some(state) = tool_state(doc, scope, manager, tool)?
        && state.default.is_none()
        && state.install.is_empty()
    {
        return Ok(false);
    }
    with_tool(doc, scope, manager.as_str(), tool.as_str(), |table| {
        table.set_implicit(false);
        let keys: Vec<String> = table.iter().map(|(key, _)| key.to_owned()).collect();
        for key in keys {
            table.remove(&key);
        }
        Ok(true)
    })
}

fn set_default(
    doc: &mut DocumentMut,
    scope: &Scope,
    manager: Manager,
    tool: &ToolName,
    version: &ToolVersion,
) -> Result<bool> {
    if let Some(state) = tool_state(doc, scope, manager, tool)?
        && state.default.as_deref() == Some(version.as_str())
        && !state.install.iter().any(|item| item == version.as_str())
    {
        return Ok(false);
    }
    let tool_name = tool.as_str();
    let version = version.as_str();
    with_tool(doc, scope, manager.as_str(), tool_name, |table| {
        let changed = set_string(table, "default", version);
        let removed = remove_install(table, version);
        Ok(changed || removed)
    })
}

fn add_install(
    doc: &mut DocumentMut,
    scope: &Scope,
    manager: Manager,
    tool: &ToolName,
    version: &ToolVersion,
) -> Result<bool> {
    let version = version.as_str();
    if let Some(state) = tool_state(doc, scope, manager, tool)?
        && holds(&state, version)
    {
        return Ok(false);
    }
    push_install(doc, scope, manager, tool, version)
}

fn holds(state: &ToolState, version: &str) -> bool {
    state.default.as_deref() == Some(version) || state.install.iter().any(|item| item == version)
}

fn push_install(
    doc: &mut DocumentMut,
    scope: &Scope,
    manager: Manager,
    tool: &ToolName,
    version: &str,
) -> Result<bool> {
    let tool_name = tool.as_str();
    let field = format!(
        "{}.tools.{tool_name}.install",
        manager_path(scope, manager.as_str())
    );
    with_tool(doc, scope, manager.as_str(), tool_name, |table| {
        match table.entry("install") {
            Entry::Vacant(slot) => {
                let mut array = toml_edit::Array::new();
                array.push(version);
                slot.insert(Item::Value(Value::Array(array)));
            }
            Entry::Occupied(mut slot) => {
                let Some(array) = slot.get_mut().as_array_mut() else {
                    return Err(invalid(&field, "expected an array of strings"));
                };
                array.push(version);
            }
        }
        Ok(true)
    })
}

fn unset(
    doc: &mut DocumentMut,
    scope: &Scope,
    manager: Manager,
    tool: &ToolName,
    version: Option<&ToolVersion>,
) -> Result<bool> {
    let Some(state) = tool_state(doc, scope, manager, tool)? else {
        return Ok(false);
    };
    if let Some(version) = version
        && state.default.as_deref() != Some(version.as_str())
        && !state.install.iter().any(|item| item == version.as_str())
    {
        return Ok(false);
    }
    let tool_name = tool.as_str().to_owned();
    let manager_name = manager.as_str();
    let mut orphans = Vec::new();
    with_tools(doc, scope, manager_name, |tools| {
        match version {
            None => remove_key(tools, &tool_name, &mut orphans),
            Some(version) => {
                let remove_entry = {
                    let Some(table) = tools.get_mut(&tool_name).and_then(Item::as_table_mut) else {
                        return Ok(());
                    };
                    if table.get("default").and_then(Item::as_str) == Some(version.as_str()) {
                        table.remove("default");
                    }
                    remove_install(table, version.as_str());
                    versionless(table)
                };
                if remove_entry {
                    remove_key(tools, &tool_name, &mut orphans);
                }
            }
        }
        Ok(())
    })?;
    prune(doc, scope, manager_name, &mut orphans);
    Ok(true)
}

fn with_tool<T>(
    doc: &mut DocumentMut,
    scope: &Scope,
    manager: &str,
    tool: &str,
    f: impl FnOnce(&mut Table) -> Result<T>,
) -> Result<T> {
    let path = manager_path(scope, manager);
    with_tools(doc, scope, manager, |tools| {
        let table = descend(tools, tool, &format!("{path}.tools.{tool}"))?;
        f(table)
    })
}

fn with_tools<T>(
    doc: &mut DocumentMut,
    scope: &Scope,
    manager: &str,
    f: impl FnOnce(&mut Table) -> Result<T>,
) -> Result<T> {
    let path = manager_path(scope, manager);
    let manager_table = manager_table(doc, scope, manager, &path)?;
    let tools = descend(manager_table, "tools", &format!("{path}.tools"))?;
    f(tools)
}

fn manager_table<'a>(
    doc: &'a mut DocumentMut,
    scope: &Scope,
    manager: &str,
    path: &str,
) -> Result<&'a mut Table> {
    let root = doc.as_table_mut();
    match scope {
        Scope::Defaults => {
            let defaults = descend(root, "defaults", "defaults")?;
            descend(defaults, manager, path)
        }
        Scope::Member(name) => {
            let members = descend(root, "members", "members")?;
            let member_path = format!("members.{}", name.as_str());
            let member = descend(members, name.as_str(), &member_path)?;
            descend(member, manager, path)
        }
    }
}

fn descend<'a>(table: &'a mut Table, key: &str, path: &str) -> Result<&'a mut Table> {
    if !table.contains_key(key) {
        let mut child = Table::new();
        child.set_implicit(true);
        table.insert(key, Item::Table(child));
    }
    table
        .get_mut(key)
        .and_then(Item::as_table_mut)
        .ok_or_else(|| invalid(path, "expected a table"))
}

fn prune(doc: &mut DocumentMut, scope: &Scope, manager: &str, orphans: &mut Vec<Orphan>) {
    if let Some(manager_table) = manager_mut(doc, scope, manager)
        && manager_table
            .get("tools")
            .and_then(Item::as_table)
            .is_some_and(Table::is_empty)
    {
        remove_key(manager_table, "tools", orphans);
    }
    if let Some(parent) = scope_parent_mut(doc, scope)
        && parent
            .get(manager)
            .and_then(Item::as_table)
            .is_some_and(Table::is_empty)
    {
        remove_key(parent, manager, orphans);
    }
    match scope {
        Scope::Defaults => {
            if doc
                .as_table()
                .get("defaults")
                .and_then(Item::as_table)
                .is_some_and(Table::is_empty)
            {
                remove_key(doc.as_table_mut(), "defaults", orphans);
            }
        }
        Scope::Member(name) => {
            let member = name.as_str().to_owned();
            if let Some(members) = doc
                .as_table_mut()
                .get_mut("members")
                .and_then(Item::as_table_mut)
                && members
                    .get(&member)
                    .and_then(Item::as_table)
                    .is_some_and(Table::is_empty)
            {
                remove_key(members, &member, orphans);
            }
            if doc
                .as_table()
                .get("members")
                .and_then(Item::as_table)
                .is_some_and(Table::is_empty)
            {
                remove_key(doc.as_table_mut(), "members", orphans);
            }
        }
    }
    restore_orphans(doc, orphans);
}

struct Orphan {
    position: isize,
    prefix: String,
}

struct Header {
    position: isize,
    path: Vec<String>,
}

fn remove_key(table: &mut Table, key: &str, orphans: &mut Vec<Orphan>) {
    let orphan = {
        let child = table.get(key).and_then(Item::as_table);
        child.and_then(|child| {
            let prefix = child.decor().prefix().and_then(|raw| raw.as_str())?;
            prefix.contains('#').then(|| Orphan {
                position: child.position().unwrap_or(isize::MAX),
                prefix: prefix.to_owned(),
            })
        })
    };
    if let Some(orphan) = orphan {
        orphans.push(orphan);
    }
    table.remove(key);
}

fn restore_orphans(doc: &mut DocumentMut, orphans: &mut Vec<Orphan>) {
    if orphans.is_empty() {
        return;
    }
    let mut headers = Vec::new();
    collect_headers(doc.as_table(), &mut Vec::new(), &mut headers);
    orphans.sort_by_key(|orphan| std::cmp::Reverse(orphan.position));
    for orphan in orphans.drain(..) {
        let next = headers
            .iter()
            .find(|header| header.position > orphan.position);
        if let Some(header) = next {
            prepend_table_prefix(doc, &header.path, &orphan.prefix);
        } else {
            let existing = doc.trailing().as_str().unwrap_or("").to_owned();
            let mut combined = existing;
            if !combined.is_empty() && !combined.ends_with('\n') {
                combined.push('\n');
            }
            combined.push_str(&orphan.prefix);
            doc.set_trailing(combined);
        }
    }
}

fn collect_headers(table: &Table, path: &mut Vec<String>, headers: &mut Vec<Header>) {
    let mut keys: Vec<(isize, String)> = table
        .iter()
        .filter_map(|(key, item)| {
            item.as_table()
                .map(|child| (child.position().unwrap_or(isize::MAX), key.to_owned()))
        })
        .collect();
    keys.sort_by_key(|(position, _)| *position);
    for (position, key) in keys {
        path.push(key);
        let child = table
            .get(path.last().unwrap())
            .and_then(Item::as_table)
            .unwrap();
        if !child.is_implicit() && !child.is_dotted() {
            headers.push(Header {
                position,
                path: path.clone(),
            });
        }
        collect_headers(child, path, headers);
        path.pop();
    }
}

fn prepend_table_prefix(doc: &mut DocumentMut, path: &[String], prefix: &str) {
    let mut table = doc.as_table_mut();
    for key in path {
        table = table
            .get_mut(key)
            .and_then(Item::as_table_mut)
            .expect("header path");
    }
    let existing = table
        .decor()
        .prefix()
        .and_then(|raw| raw.as_str())
        .unwrap_or("")
        .to_owned();
    let mut combined = prefix.to_owned();
    if !combined.ends_with('\n') {
        combined.push('\n');
    }
    combined.push_str(&existing);
    table.decor_mut().set_prefix(combined);
}

fn scope_parent_mut<'a>(doc: &'a mut DocumentMut, scope: &Scope) -> Option<&'a mut Table> {
    match scope {
        Scope::Defaults => doc.as_table_mut().get_mut("defaults")?.as_table_mut(),
        Scope::Member(name) => {
            let name = name.as_str();
            doc.as_table_mut()
                .get_mut("members")?
                .as_table_mut()?
                .get_mut(name)?
                .as_table_mut()
        }
    }
}

fn manager_mut<'a>(
    doc: &'a mut DocumentMut,
    scope: &Scope,
    manager: &str,
) -> Option<&'a mut Table> {
    scope_parent_mut(doc, scope)?
        .get_mut(manager)?
        .as_table_mut()
}

fn set_string(table: &mut Table, key: &str, text: &str) -> bool {
    match table.entry(key) {
        Entry::Occupied(mut occupied) => {
            if occupied.get().as_str() == Some(text) {
                return false;
            }
            let decor = occupied.get().as_value().map(|item| item.decor().clone());
            let mut item = value(text);
            if let Some(decor) = decor
                && let Some(parsed) = item.as_value_mut()
            {
                *parsed.decor_mut() = decor;
            }
            occupied.insert(item);
            true
        }
        Entry::Vacant(slot) => {
            slot.insert(value(text));
            true
        }
    }
}

fn remove_install(table: &mut Table, version: &str) -> bool {
    let (changed, empty) = {
        let Some(array) = table.get_mut("install").and_then(Item::as_array_mut) else {
            return false;
        };
        let before = array.len();
        array.retain(|value| value.as_str() != Some(version));
        (array.len() != before, array.is_empty())
    };
    if empty {
        table.remove("install");
    }
    changed || empty
}

fn versionless(table: &Table) -> bool {
    for (key, item) in table.iter() {
        match key {
            "default" => return false,
            "install" => {
                if item.as_array().is_some_and(|array| !array.is_empty()) {
                    return false;
                }
            }
            _ => return false,
        }
    }
    true
}

fn tool_state(
    doc: &DocumentMut,
    scope: &Scope,
    manager: Manager,
    tool: &ToolName,
) -> Result<Option<ToolState>> {
    let Some(manager_table) = find_manager(doc, scope, manager.as_str())? else {
        return Ok(None);
    };
    let path = manager_path(scope, manager.as_str());
    let Some(tools) = child_table(manager_table, "tools", &format!("{path}.tools"))? else {
        return Ok(None);
    };
    let tool_path = format!("{path}.tools.{}", tool.as_str());
    let Some(table) = child_table(tools, tool.as_str(), &tool_path)? else {
        return Ok(None);
    };
    let default = match table.get("default") {
        None => None,
        Some(item) => Some(
            item.as_str()
                .ok_or_else(|| invalid(&format!("{tool_path}.default"), "expected a string"))?
                .to_owned(),
        ),
    };
    let install = match table.get("install") {
        None => Vec::new(),
        Some(item) => {
            let Some(array) = item.as_array() else {
                return Err(invalid(
                    &format!("{tool_path}.install"),
                    "expected an array of strings",
                ));
            };
            let mut values = Vec::new();
            for value in array {
                let Some(text) = value.as_str() else {
                    return Err(invalid(
                        &format!("{tool_path}.install"),
                        "expected a string",
                    ));
                };
                values.push(text.to_owned());
            }
            values
        }
    };
    Ok(Some(ToolState { default, install }))
}

fn find_manager<'a>(
    doc: &'a DocumentMut,
    scope: &Scope,
    manager: &str,
) -> Result<Option<&'a Table>> {
    let root = doc.as_table();
    match scope {
        Scope::Defaults => {
            let Some(defaults) = child_table(root, "defaults", "defaults")? else {
                return Ok(None);
            };
            child_table(defaults, manager, &format!("defaults.{manager}"))
        }
        Scope::Member(name) => {
            let Some(members) = child_table(root, "members", "members")? else {
                return Ok(None);
            };
            let member_path = format!("members.{}", name.as_str());
            let Some(member) = child_table(members, name.as_str(), &member_path)? else {
                return Ok(None);
            };
            child_table(member, manager, &format!("{member_path}.{manager}"))
        }
    }
}

fn child_table<'a>(table: &'a Table, key: &str, path: &str) -> Result<Option<&'a Table>> {
    match table.get(key) {
        None => Ok(None),
        Some(item) => match item.as_table() {
            Some(child) => Ok(Some(child)),
            None => Err(invalid(path, "expected a table")),
        },
    }
}

fn manager_path(scope: &Scope, manager: &str) -> String {
    match scope {
        Scope::Defaults => format!("defaults.{manager}"),
        Scope::Member(name) => format!("members.{}.{manager}", name.as_str()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        tool_inventory::Manager::{Mise, Simulators, Xcodes},
        toolset::document::Exception,
    };

    const NOTED: &str = "# fleet note\n\
[defaults.mise.tools.sentry-cli] # everywhere\n\
\n\
[members.alpha.xcodes.tools.xcode]\n\
default = \"26.3\" # on purpose\n\
\n\
[members.alpha.mise.tools.tuist]\n\
install = [\"4.205.0\"] # alongside\n";

    fn version(text: &str) -> ToolVersion {
        ToolVersion::new(text).unwrap()
    }

    fn tool(name: &str) -> ToolName {
        ToolName::new(name).unwrap()
    }

    fn alpha() -> Scope {
        Scope::Member("alpha".parse().unwrap())
    }

    fn member(text: &str, edit: &Edit) -> String {
        apply(text, &alpha(), edit).unwrap()
    }

    fn exceptions(text: &str) -> Vec<(String, Exception)> {
        Toolset::parse(text.as_bytes())
            .unwrap()
            .exceptions(&"alpha".parse().unwrap())
            .into_iter()
            .map(|exception| (exception.key, exception.exception))
            .collect()
    }

    #[test]
    fn comments_survive_each_edit() {
        let add = member(
            NOTED,
            &Edit::AddInstall {
                manager: Mise,
                tool: tool("bun"),
                version: version("1.2.13"),
            },
        );
        for kept in [
            "# fleet note",
            "# everywhere",
            "# on purpose",
            "# alongside",
            "1.2.13",
        ] {
            assert!(add.contains(kept), "{kept}: {add}");
        }
        let set_default = member(
            NOTED,
            &Edit::SetDefault {
                manager: Xcodes,
                tool: tool("xcode"),
                version: version("26.4"),
            },
        );
        assert!(set_default.contains("# on purpose"), "{set_default}");
        assert!(set_default.contains("26.4") && !set_default.contains("26.3"));
        let unset = member(
            NOTED,
            &Edit::Unset {
                manager: Mise,
                tool: tool("tuist"),
                version: None,
            },
        );
        assert!(unset.contains("# fleet note") && unset.contains("# on purpose"));
        assert!(!unset.contains("tuist"), "{unset}");
    }

    #[test]
    fn leaving_out_writes_an_empty_entry_and_replaces_versions() {
        let everywhere = apply(
            "# keep\n",
            &Scope::Defaults,
            &Edit::LeaveOut {
                manager: Mise,
                tool: tool("bun"),
            },
        )
        .unwrap();
        assert!(
            everywhere.contains("[defaults.mise.tools.bun]\n"),
            "{everywhere}"
        );
        assert!(everywhere.contains("# keep"), "{everywhere}");
        let replaced = member(
            NOTED,
            &Edit::LeaveOut {
                manager: Xcodes,
                tool: tool("xcode"),
            },
        );
        assert!(
            replaced.contains("[members.alpha.xcodes.tools.xcode]"),
            "{replaced}"
        );
        assert!(!replaced.contains("26.3"), "{replaced}");
        assert!(exceptions(&replaced).contains(&(
            "members.alpha.xcodes.tools.xcode".to_owned(),
            Exception::LeaveOut
        )));
        let runtimes = member(
            "",
            &Edit::LeaveOut {
                manager: Simulators,
                tool: tool("iOS"),
            },
        );
        assert_eq!(runtimes, "[members.alpha.simulators.tools.iOS]\n");
        assert_eq!(
            member(
                NOTED,
                &Edit::LeaveOut {
                    manager: Mise,
                    tool: tool("tuist")
                }
            ),
            NOTED.replace("install = [\"4.205.0\"] # alongside\n", "")
        );
    }

    #[test]
    fn set_on_an_empty_file_creates_tables() {
        let node = member(
            "",
            &Edit::SetDefault {
                manager: Mise,
                tool: tool("node"),
                version: version("22.19.0"),
            },
        );
        assert_eq!(
            node,
            "[members.alpha.mise.tools.node]\ndefault = \"22.19.0\"\n"
        );
        let install = member(
            "",
            &Edit::AddInstall {
                manager: Mise,
                tool: tool("tuist"),
                version: version("4.205.0"),
            },
        );
        assert_eq!(
            install,
            "[members.alpha.mise.tools.tuist]\ninstall = [\"4.205.0\"]\n"
        );
    }

    #[test]
    fn versions_under_defaults_are_refused() {
        let error = apply(
            "",
            &Scope::Defaults,
            &Edit::SetDefault {
                manager: Mise,
                tool: tool("node"),
                version: version("22.19.0"),
            },
        )
        .unwrap_err();
        assert_eq!(error.code, "invalid_fleet_tools");
        assert!(error.message.contains("only leaves a tool out"), "{error}");
    }

    #[test]
    fn unset_removes_emptied_tables() {
        let original = "# keep\n\
[members.alpha.mise.tools.node]\n\
default = \"22.19.0\"\n\
install = [\"22.16.0\"]\n\
\n\
[defaults.mise.tools.tuist] # stays\n";
        let without_default = member(
            original,
            &Edit::Unset {
                manager: Mise,
                tool: tool("node"),
                version: Some(version("22.19.0")),
            },
        );
        assert!(without_default.contains("22.16.0") && !without_default.contains("22.19.0"));
        let cleared = member(
            &without_default,
            &Edit::Unset {
                manager: Mise,
                tool: tool("node"),
                version: Some(version("22.16.0")),
            },
        );
        assert!(!cleared.contains("alpha"), "{cleared}");
        assert!(
            cleared.contains("# keep") && cleared.contains("# stays"),
            "{cleared}"
        );
        let left_out = apply(
            &cleared,
            &Scope::Defaults,
            &Edit::Unset {
                manager: Mise,
                tool: tool("tuist"),
                version: None,
            },
        )
        .unwrap();
        assert!(!left_out.contains("tuist"), "{left_out}");
        assert!(left_out.contains("# keep"), "{left_out}");
    }

    #[test]
    fn an_edit_that_does_not_validate_is_refused() {
        let broken = "[members.alpha.mise.tools.bun]\ndefault = \"1.0.0\"\nextra = \"SENTINEL\"\n";
        let error = apply(
            broken,
            &alpha(),
            &Edit::AddInstall {
                manager: Mise,
                tool: tool("node"),
                version: version("22.16.0"),
            },
        )
        .unwrap_err();
        assert!(
            error.message.contains("members.alpha.mise.tools.bun.extra"),
            "{error}"
        );
        assert!(!error.message.contains("SENTINEL"), "{error}");
        let error = apply(
            "",
            &alpha(),
            &Edit::SetDefault {
                manager: Mise,
                tool: tool("node"),
                version: version("latest"),
            },
        )
        .unwrap_err();
        assert!(
            error
                .message
                .contains("versions must be exact, e.g. 22.19.0"),
            "{error}"
        );
        assert!(!error.message.contains("latest"), "{error}");
        let error = apply(
            "",
            &alpha(),
            &Edit::LeaveOut {
                manager: Xcodes,
                tool: tool("node"),
            },
        )
        .unwrap_err();
        assert!(
            error.message.contains("xcodes manages only the tool xcode"),
            "{error}"
        );
    }

    #[test]
    fn tool_names_with_colon_and_slash_are_quoted() {
        let text = apply(
            "# keep\n",
            &Scope::Defaults,
            &Edit::LeaveOut {
                manager: Mise,
                tool: tool("packslip:example/acme"),
            },
        )
        .unwrap();
        assert!(
            text.contains("[defaults.mise.tools.\"packslip:example/acme\"]"),
            "{text}"
        );
        assert_eq!(
            exceptions(&text),
            [(
                "defaults.mise.tools.\"packslip:example/acme\"".to_owned(),
                Exception::LeaveOut
            )]
        );
    }

    #[test]
    fn a_noop_keeps_the_original_text() {
        let original = "[members.alpha.mise.tools.node]\ndefault = \"22.19.0\" # keep\n";
        for edit in [
            Edit::SetDefault {
                manager: Mise,
                tool: tool("node"),
                version: version("22.19.0"),
            },
            Edit::AddInstall {
                manager: Mise,
                tool: tool("node"),
                version: version("22.19.0"),
            },
            Edit::Unset {
                manager: Mise,
                tool: tool("node"),
                version: Some(version("1.0.0")),
            },
        ] {
            assert_eq!(member(original, &edit), original, "{edit:?}");
        }
        let left_out = "[defaults.mise.tools.bun] # keep\n";
        let same = apply(
            left_out,
            &Scope::Defaults,
            &Edit::LeaveOut {
                manager: Mise,
                tool: tool("bun"),
            },
        )
        .unwrap();
        assert_eq!(same, left_out);
    }

    #[test]
    fn an_existing_entry_is_edited_in_place() {
        let original = "[members.alpha.mise.tools.node]\n\
default = \"20.0.0\"\n\
install = [\"20.11.0\"]\n";
        let edited = member(
            original,
            &Edit::AddInstall {
                manager: Mise,
                tool: tool("node"),
                version: version("18.0.0"),
            },
        );
        assert_eq!(
            edited,
            "[members.alpha.mise.tools.node]\n\
default = \"20.0.0\"\n\
install = [\"20.11.0\", \"18.0.0\"]\n"
        );
    }
}
