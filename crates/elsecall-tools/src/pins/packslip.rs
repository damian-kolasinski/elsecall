//! Packslip's exact GitHub Actions signing identity for one tagged release.
//! Only the master's global config can supply a managed pin. Members report
//! fingerprints of their local declarations so signer drift is visible even
//! when the release is already installed.
use super::{InstallOptions, invalid, source_file::read_home_file};
use elsecall_contract::failure::Result;
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, path::Path};

pub(crate) const CONFIG: &str = ".config/mise/config.toml";
pub(crate) const DISPLAY: &str = "~/.config/mise/config.toml";
const ISSUER: &str = "https://token.actions.githubusercontent.com";

fn component(s: &str) -> bool {
    !s.is_empty()
        && !matches!(s, "." | "..")
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.'))
}

pub(crate) fn repo(tool: &str) -> Option<&str> {
    let repo = tool.strip_prefix("packslip:github.com/")?;
    let (owner, name) = repo.split_once('/')?;
    (component(owner) && component(name)).then_some(repo)
}

pub(crate) fn valid(tool: &str, version: &str, identity: &str, issuer: &str) -> bool {
    let Some(repo) = repo(tool) else { return false };
    let Some(workflow) = identity
        .strip_prefix(&format!("https://github.com/{repo}/.github/workflows/"))
        .and_then(|path| path.strip_suffix(&format!("@refs/tags/v{version}")))
    else {
        return false;
    };
    issuer == ISSUER
        && version.split('.').count() == 3
        && version
            .split('.')
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
        && component(workflow)
        && (workflow.ends_with(".yml") || workflow.ends_with(".yaml"))
        && identity.len() <= 256
}

pub(crate) fn fingerprint(tool: &str, version: &str, identity: &str) -> String {
    format!(
        "{:x}",
        Sha256::digest(format!("{tool}\0{version}\0{identity}"))
    )
}

/// A separate fingerprint for a tool whose local options mise would retain
/// during a repair. Never send the contents of these options to the master.
pub(crate) fn local_options(tool: &str) -> String {
    format!("{:x}", Sha256::digest(format!("local-options\0{tool}")))
}

fn declaration<'a>(root: &'a toml::Table, tool: &str) -> Option<(&'a str, &'a str, &'a str)> {
    let entry = root.get("tools")?.get(tool)?.as_table()?;
    if entry.len() != 3 {
        return None;
    }
    let fields = (
        entry.get("version")?.as_str()?,
        entry.get("identity")?.as_str()?,
        entry.get("issuer")?.as_str()?,
    );
    valid(tool, fields.0, fields.1, fields.2).then_some(fields)
}

pub(super) fn approve(
    text: &str,
    reading: &mut super::Reading,
    selected: &BTreeSet<String>,
) -> Result<()> {
    if selected.is_empty() {
        return Ok(());
    }
    let root: toml::Table =
        toml::from_str(text).map_err(|_| invalid(DISPLAY, "tools", "expected TOML"))?;
    for tool in selected {
        let error = || {
            invalid(
                DISPLAY,
                tool,
                "a managed Packslip pin requires exactly version, identity for its own tagged release workflow, and the GitHub Actions issuer",
            )
        };
        let (version, identity, issuer) = declaration(&root, tool).ok_or_else(error)?;
        let pin = reading
            .pins
            .iter_mut()
            .find(|pin| pin.target.tool() == tool && pin.version.as_str() == version)
            .ok_or_else(error)?;
        pin.options = InstallOptions::signed(identity, issuer);
    }
    Ok(())
}

pub(crate) fn fingerprints(home: &Path) -> Result<BTreeSet<String>> {
    let Some(text) = read_home_file(&home.join(CONFIG), DISPLAY)? else {
        return Ok(BTreeSet::new());
    };
    let root: toml::Table =
        toml::from_str(&text).map_err(|_| invalid(DISPLAY, "tools", "expected TOML"))?;
    let mut pins = BTreeSet::new();
    if let Some(tools) = root.get("tools") {
        let tools = tools
            .as_table()
            .ok_or_else(|| invalid(DISPLAY, "tools", "expected a table"))?;
        for (key, entry) in tools {
            let tool = key.split_once('[').map_or(key.as_str(), |(tool, _)| tool);
            if repo(tool).is_none() {
                continue;
            }
            let simple = entry.is_str()
                || entry.as_table().is_some_and(|table| {
                    table
                        .keys()
                        .all(|key| matches!(key.as_str(), "version" | "identity" | "issuer"))
                });
            if !simple || tool != key {
                pins.insert(local_options(tool));
            } else if let Some((version, identity, _)) = declaration(&root, tool) {
                pins.insert(fingerprint(tool, version, identity));
            }
        }
    }
    Ok(pins)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn only_an_exact_release_of_the_named_repository_can_select_a_global_pin() {
        let tool = "packslip:github.com/acme/tool";
        let signer = "https://github.com/acme/tool/.github/workflows/release.yml@refs/tags/v2.0.0";
        let options = |identity: &str, issuer: &str| {
            BTreeMap::from([
                ("identity".to_owned(), identity.to_owned()),
                ("issuer".to_owned(), issuer.to_owned()),
            ])
        };
        assert!(InstallOptions::from_step(tool, "2.0.0", options(signer, ISSUER)).is_ok());
        assert!(InstallOptions::from_wire(options(signer, ISSUER)).is_err());
        for (name, version, identity, issuer) in [
            ("github:acme/tool", "2.0.0", signer, ISSUER),
            ("packslip:github.com/acme/other", "2.0.0", signer, ISSUER),
            (tool, "1.0.0", signer, ISSUER),
            (tool, "latest", signer, ISSUER),
            (tool, "2.0.0", signer, "https://example.com"),
            (
                tool,
                "2.0.0",
                "https://github.com/acme/tool/.github/workflows/*.yml@refs/tags/v2.0.0",
                ISSUER,
            ),
        ] {
            assert!(InstallOptions::from_step(name, version, options(identity, issuer)).is_err());
        }
        let mut extra = options(signer, ISSUER);
        extra.insert("postinstall".into(), "ignored".into());
        assert!(InstallOptions::from_step(tool, "2.0.0", extra).is_err());
    }
}
