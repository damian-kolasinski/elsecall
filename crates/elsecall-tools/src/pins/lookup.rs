//! Whether a workspace or the master's global mise config already pins a tool,
//! and which file a new pin of it belongs in: what the fleet view needs to clear
//! a "not pinned, pin it here" item once someone pins the tool.
//!
//! It reads the same files as `elsecall tools pins`, through the same adapters,
//! and nothing else: no mise, no Xcode scan, no conflict check, since a conflict
//! still pins the tool.
use crate::pins::{
    PinSource, SourceKind, elsecall_tools_file::ElsecallToolsFile, mise_global::MiseGlobal,
    mise_project::MiseProject, xcode_version::XcodeVersionFile,
};
use elsecall_contract::failure::Result;
use std::path::Path;

/// Where a pin of an Xcode version goes, at the workspace's root.
const XCODE_FILE: &str = ".xcode-version";
/// Where a mise pin goes when the workspace has no mise config yet.
const MISE_FILE: &str = "mise.toml";
/// The master's global mise config, for runs outside a workspace.
const GLOBAL_FILE: &str = "~/.config/mise/config.toml";

/// The pins one workspace (or, without one, the global config) holds.
#[derive(Debug)]
pub struct PinLookup {
    /// `(manager, tool)`, as inventory names them.
    pinned: Vec<(&'static str, String)>,
    /// The workspace's first mise project file, if it has one.
    mise_file: Option<String>,
    in_workspace: bool,
}

/// Reads the pins `workspace` and the global config under `home` hold. Without a
/// workspace, only the global config.
pub fn read(workspace: Option<&Path>, home: &Path) -> Result<PinLookup> {
    let global = MiseGlobal::new(home.to_owned());
    let mut sources: Vec<&dyn PinSource> = vec![&global];
    if workspace.is_some() {
        sources.extend([
            &MiseProject as &dyn PinSource,
            &XcodeVersionFile,
            &ElsecallToolsFile,
        ]);
    }
    let mut lookup = PinLookup {
        pinned: Vec::new(),
        mise_file: None,
        in_workspace: workspace.is_some(),
    };
    for source in sources {
        let reading = source.read(workspace.unwrap_or(home))?;
        if lookup.mise_file.is_none() {
            let files = reading
                .files
                .iter()
                .filter(|file| file.kind == SourceKind::MiseProject);
            let files: Vec<_> = files.map(|file| file.file.clone()).collect();
            lookup.mise_file = files
                .iter()
                .find(|file| file.as_str() == MISE_FILE)
                .or_else(|| files.iter().find(|file| file.ends_with(".toml")))
                .cloned();
        }
        lookup.pinned.extend(
            reading
                .pins
                .into_iter()
                .map(|pin| (pin.target.manager(), pin.target.tool().to_owned())),
        );
    }
    Ok(lookup)
}

impl PinLookup {
    /// Whether a `manager` tool called `name` is pinned. A mise pin may name a
    /// backend (`aqua:realm/SwiftLint`, `npm:@scope/pkg`), whose program is its
    /// last part, compared ignoring case.
    pub fn pins(&self, manager: &str, name: &str) -> bool {
        self.pinned.iter().any(|(pinned_manager, tool)| {
            *pinned_manager == manager
                && (tool == name
                    || tool
                        .rsplit([':', '/'])
                        .next()
                        .is_some_and(|last| last.eq_ignore_ascii_case(name)))
        })
    }

    /// The file a new pin of a `manager` tool belongs in: workspace-relative,
    /// or the global config for runs outside a workspace.
    pub fn file_for(&self, manager: &str) -> String {
        match (manager, self.in_workspace) {
            ("xcodes", _) => XCODE_FILE.into(),
            (_, false) => GLOBAL_FILE.into(),
            _ => self.mise_file.clone().unwrap_or_else(|| MISE_FILE.into()),
        }
    }
}

/// Where a pin would go when the workspace cannot be read.
pub fn default_file(manager: &str, in_workspace: bool) -> String {
    PinLookup {
        pinned: Vec::new(),
        mise_file: None,
        in_workspace,
    }
    .file_for(manager)
}

#[cfg(test)]
mod tests {
    use crate::pins::lookup::*;
    use std::fs;

    #[test]
    fn pins_clear_by_name_or_backend_and_name_the_file_to_pin_in() {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path().canonicalize().unwrap();
        fs::write(
            root.join(".mise.toml"),
            "[tools]\nnode = \"22.19.0\"\n\"aqua:realm/SwiftLint\" = \"0.59.1\"\n",
        )
        .unwrap();
        fs::write(root.join(".xcode-version"), "26.2\n").unwrap();
        fs::create_dir_all(home.path().join(".config/mise")).unwrap();
        fs::write(
            home.path().join(".config/mise/config.toml"),
            "[tools]\nbun = \"1.3.14\"\n",
        )
        .unwrap();

        let lookup = read(Some(&root), home.path()).unwrap();
        assert!(lookup.pins("mise", "node"));
        assert!(lookup.pins("mise", "swiftlint"), "the backend's program");
        assert!(lookup.pins("mise", "bun"), "the global layer counts");
        assert!(lookup.pins("xcodes", "xcode"));
        assert!(!lookup.pins("mise", "tuist"));
        assert_eq!(lookup.file_for("mise"), ".mise.toml");
        assert_eq!(lookup.file_for("xcodes"), ".xcode-version");

        let outside = read(None, home.path()).unwrap();
        assert!(outside.pins("mise", "bun") && !outside.pins("mise", "node"));
        assert_eq!(outside.file_for("mise"), "~/.config/mise/config.toml");

        let bare = tempfile::tempdir().unwrap();
        let lookup = read(Some(&bare.path().canonicalize().unwrap()), home.path()).unwrap();
        assert_eq!(lookup.file_for("mise"), "mise.toml");
        assert_eq!(default_file("mise", true), "mise.toml");
    }
}
