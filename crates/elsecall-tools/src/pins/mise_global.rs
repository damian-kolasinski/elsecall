//! The master's global mise config, `~/.config/mise/config.toml`: the tools and
//! defaults every member's shell gets outside a project.
//!
//! Only that file is read, under the home folder of whoever runs the read, so the
//! path means the same on every member whatever its home folder is called. It
//! is the operator's own file, so it may link anywhere. mise's other global
//! files (`~/.config/mise/conf.d`, `config.local.toml`, environment variants)
//! are not read.
use super::packslip::{self, CONFIG as FILE, DISPLAY};
use crate::pins::{
    PinSource, Reading, SourceFile, SourceKind,
    mise_config::{self, push_pins},
    source_file::read_home_file,
};
use elsecall_contract::failure::Result;
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

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
        read_managed(&self.home, &BTreeSet::new())
    }
}

pub(crate) fn read_managed(home: &Path, selected: &BTreeSet<String>) -> Result<Reading> {
    let mut reading = Reading::default();
    let text = read_home_file(&home.join(FILE), DISPLAY)?;
    if text.is_none() && selected.is_empty() {
        return Ok(reading);
    }
    let text = text.unwrap_or_default();
    let source = SourceFile {
        kind: SourceKind::MiseGlobal,
        file: DISPLAY.to_owned(),
    };
    push_pins(
        mise_config::tools(&text, &source.file)?,
        None,
        &source,
        &mut reading,
    );
    packslip::approve(&text, &mut reading, selected)?;
    reading.files.push(source);
    Ok(reading)
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
        assert!(
            reading
                .pins
                .iter()
                .all(|pin| pin.target.environment().is_none())
        );

        fs::write(&path, "[tools]\nnode = \"{{ env.HOME }}\"\n").unwrap();
        let error = source.read(workspace.path()).unwrap_err();
        assert!(
            error
                .message
                .starts_with("~/.config/mise/config.toml: tools.node:")
        );
    }
}
