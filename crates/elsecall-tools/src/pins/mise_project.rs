//! The mise project config a repository keeps at its root.
//!
//! These are the files mise reads for a directory, in its order from lowest
//! precedence: `.tool-versions`, `.config/mise/conf.d/*.toml`,
//! `.config/mise/config.toml`, `.config/mise.toml`, `mise/config.toml`,
//! `mise.toml`, `.mise/config.toml` and `.mise.toml`. Each config name also has
//! environment variants, `mise.<env>.toml`, which mise reads only with
//! `MISE_ENV=<env>`; their pins carry that environment. `*.local.toml` files
//! are one machine's overrides, not the repository's, and are not read. Parent
//! directories are the member's, not the repository's, and are not read either.
use crate::pins::{
    Environment, Pin, PinSource, Reading, SourceFile, SourceKind, Target, invalid,
    mise_config::{self, ToolRequests},
    source_file::{Confinement, list_names, read_text},
    tool_versions,
};
use elsecall_contract::failure::Result;
use std::path::Path;

const MAX_CONF_D: usize = 64;

/// Config files named `<stem>.toml` in `directory`, with their `<stem>.<env>.toml`
/// variants.
const LAYOUTS: [(&str, &str); 6] = [
    (".config/mise", "config"),
    (".config", "mise"),
    ("mise", "config"),
    ("", "mise"),
    (".mise", "config"),
    ("", ".mise"),
];

pub(super) struct MiseProject;

impl PinSource for MiseProject {
    fn read(&self, workspace: &Path) -> Result<Reading> {
        let mut reading = Reading::default();
        read_file(workspace, ".tool-versions", None, &mut reading)?;
        for name in conf_d(workspace)? {
            read_file(workspace, &name, None, &mut reading)?;
        }
        for (directory, stem) in LAYOUTS {
            for (file, environment) in config_files(workspace, directory, stem)? {
                read_file(workspace, &file, environment, &mut reading)?;
            }
        }
        Ok(reading)
    }
}

fn conf_d(workspace: &Path) -> Result<Vec<String>> {
    let directory = ".config/mise/conf.d";
    let names: Vec<String> = list_names(&workspace.join(directory), directory)?
        .into_iter()
        .filter(|name| name.ends_with(".toml") && printable(name))
        .map(|name| format!("{directory}/{name}"))
        .collect();
    if names.len() > MAX_CONF_D {
        return Err(invalid(directory, "", "at most 64 config files"));
    }
    Ok(names)
}

/// `<directory>/<stem>.toml`, then its environment variants by name.
fn config_files(
    workspace: &Path,
    directory: &str,
    stem: &str,
) -> Result<Vec<(String, Option<Environment>)>> {
    let display_directory = if directory.is_empty() { "." } else { directory };
    let prefix = |name: &str| {
        if directory.is_empty() {
            name.to_owned()
        } else {
            format!("{directory}/{name}")
        }
    };
    let mut files = vec![(prefix(&format!("{stem}.toml")), None)];
    for name in list_names(&workspace.join(directory), display_directory)? {
        let environment = name
            .strip_prefix(stem)
            .and_then(|rest| rest.strip_prefix('.'))
            .and_then(|rest| rest.strip_suffix(".toml"))
            .and_then(Environment::new);
        if let Some(environment) = environment {
            files.push((prefix(&name), Some(environment)));
        }
    }
    Ok(files)
}

fn read_file(
    workspace: &Path,
    file: &str,
    environment: Option<Environment>,
    reading: &mut Reading,
) -> Result<()> {
    let Some(text) = read_text(&workspace.join(file), file, Confinement::Within(workspace))? else {
        return Ok(());
    };
    let requests = if file == ".tool-versions" {
        tool_versions::tools(&text, file)?
    } else {
        mise_config::tools(&text, file)?
    };
    let source = SourceFile {
        kind: SourceKind::MiseProject,
        file: file.to_owned(),
    };
    push_pins(requests, environment, &source, reading);
    reading.files.push(source);
    Ok(())
}

/// One pin per requested version, in the order written.
pub(super) fn push_pins(
    requests: Vec<ToolRequests>,
    environment: Option<Environment>,
    source: &SourceFile,
    reading: &mut Reading,
) {
    for request in requests {
        for version in request.versions {
            reading.pins.push(Pin {
                target: Target::Mise(request.tool.clone()),
                version,
                environment: environment.clone(),
                source: source.clone(),
            });
        }
    }
}

fn printable(name: &str) -> bool {
    name.chars().all(|c| !c.is_control())
}

#[cfg(test)]
mod tests {
    use crate::pins::mise_project::*;
    use std::fs;

    fn workspace() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        (dir, root)
    }

    fn write(root: &Path, file: &str, text: &str) {
        let path = root.join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn summary(reading: &Reading) -> Vec<(String, String, Option<String>, String)> {
        reading
            .pins
            .iter()
            .map(|pin| {
                (
                    pin.target.tool().to_owned(),
                    pin.version.as_str().to_owned(),
                    pin.environment.as_ref().map(|env| env.as_str().to_owned()),
                    pin.source.file.clone(),
                )
            })
            .collect()
    }

    #[test]
    fn an_empty_workspace_pins_nothing() {
        let (_dir, root) = workspace();
        assert_eq!(MiseProject.read(&root).unwrap(), Reading::default());
    }

    #[test]
    fn every_project_file_is_read_in_mise_order_with_environments() {
        let (_dir, root) = workspace();
        write(
            &root,
            "mise.toml",
            "[tools]\npython = \"3.10.4\"\nbun = \"latest\"\n",
        );
        write(
            &root,
            "mise.signing.toml",
            "[tools]\n\"packslip:example/crem\" = \"0.2.0\"\n",
        );
        write(&root, "mise.local.toml", "[tools]\nlocal = \"1.0.0\"\n");
        write(
            &root,
            "mise.signing.local.toml",
            "[tools]\nlocal = \"1.0.0\"\n",
        );
        write(&root, ".tool-versions", "swiftgen 6.6.3\n");
        write(
            &root,
            ".config/mise/conf.d/extra.toml",
            "[tools]\njq = \"1.7.1\"\n",
        );
        write(&root, ".config/mise/conf.d/notes.txt", "not a config");
        write(&root, ".mise/config.toml", "[tools]\ntuist = \"4.205.0\"\n");
        write(&root, ".mise.toml", "[settings]\njobs = 1\n");
        let reading = MiseProject.read(&root).unwrap();
        assert_eq!(
            summary(&reading),
            [
                (
                    "swiftgen".into(),
                    "6.6.3".into(),
                    None,
                    ".tool-versions".into()
                ),
                (
                    "jq".into(),
                    "1.7.1".into(),
                    None,
                    ".config/mise/conf.d/extra.toml".into()
                ),
                ("bun".into(), "latest".into(), None, "mise.toml".into()),
                ("python".into(), "3.10.4".into(), None, "mise.toml".into()),
                (
                    "packslip:example/crem".into(),
                    "0.2.0".into(),
                    Some("signing".into()),
                    "mise.signing.toml".into()
                ),
                (
                    "tuist".into(),
                    "4.205.0".into(),
                    None,
                    ".mise/config.toml".into()
                ),
            ]
        );
        let files: Vec<&str> = reading
            .files
            .iter()
            .map(|file| file.file.as_str())
            .collect();
        assert_eq!(
            files,
            [
                ".tool-versions",
                ".config/mise/conf.d/extra.toml",
                "mise.toml",
                "mise.signing.toml",
                ".mise/config.toml",
                ".mise.toml",
            ]
        );
        assert!(
            reading
                .files
                .iter()
                .all(|file| file.kind == SourceKind::MiseProject)
        );
    }

    #[test]
    fn a_malformed_file_fails_the_read() {
        let (_dir, root) = workspace();
        write(&root, ".config/mise.toml", "[tools]\nnode = 22\n");
        let error = MiseProject.read(&root).unwrap_err();
        assert_eq!(
            error.message,
            ".config/mise.toml: tools.node: expected a version, a list of versions, or a table with a version."
        );
    }
}
