//! The master's global mise config, `~/.config/mise/config.toml`: the tools and
//! defaults every member's shell gets outside a project.
//!
//! Only that file is read, under the home folder of whoever runs the read, so the
//! path means the same on every member whatever its home folder is called. It
//! is the operator's own file, so it may link anywhere.
use crate::pins::{
    PinSource, Reading, SourceFile, SourceKind, mise_config,
    mise_project::push_pins,
    source_file::{Confinement, read_text},
};
use elsecall_contract::failure::Result;
use std::path::{Path, PathBuf};

const FILE: &str = ".config/mise/config.toml";

pub(super) struct MiseGlobal {
    home: PathBuf,
}

impl MiseGlobal {
    pub(super) fn new(home: PathBuf) -> Self {
        Self { home }
    }
}

impl PinSource for MiseGlobal {
    fn read(&self, _workspace: &Path) -> Result<Reading> {
        let display = format!("~/{FILE}");
        let mut reading = Reading::default();
        let Some(text) = read_text(&self.home.join(FILE), &display, Confinement::Anywhere)? else {
            return Ok(reading);
        };
        let source = SourceFile {
            kind: SourceKind::MiseGlobal,
            file: display,
        };
        push_pins(
            mise_config::tools(&text, &source.file)?,
            None,
            &source,
            &mut reading,
        );
        reading.files.push(source);
        Ok(reading)
    }
}

#[cfg(test)]
mod tests {
    use crate::pins::mise_global::*;
    use std::fs;

    #[test]
    fn the_global_config_is_read_home_relative() {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let source = MiseGlobal::new(home.path().to_owned());
        assert_eq!(source.read(workspace.path()).unwrap(), Reading::default());

        let path = home.path().join(FILE);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "[tools]\nnode = \"22\"\njava = \"zulu-17\"\n").unwrap();
        let reading = source.read(workspace.path()).unwrap();
        assert_eq!(reading.files.len(), 1);
        assert_eq!(reading.files[0].file, "~/.config/mise/config.toml");
        assert_eq!(reading.files[0].kind, SourceKind::MiseGlobal);
        let pins: Vec<(&str, &str)> = reading
            .pins
            .iter()
            .map(|pin| (pin.target.tool(), pin.version.as_str()))
            .collect();
        assert_eq!(pins, [("java", "zulu-17"), ("node", "22")]);
        assert!(reading.pins.iter().all(|pin| pin.environment.is_none()));

        fs::write(&path, "[tools]\nnode = \"{{ env.HOME }}\"\n").unwrap();
        let error = source.read(workspace.path()).unwrap_err();
        assert!(
            error
                .message
                .starts_with("~/.config/mise/config.toml: tools.node:")
        );
    }
}
