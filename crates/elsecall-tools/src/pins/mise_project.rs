//! The mise project config a repository keeps at its root.
//!
//! These are the files mise reads for a directory, in its order from lowest
//! precedence (as `mise config ls` lists them, checked with mise 2026.10):
//!
//! 1. `.tool-versions`
//! 2. `.config/mise/conf.d/*.toml`
//! 3. `.config/mise/config.toml`, `.config/mise/mise.toml`, `.config/mise.toml`,
//!    `.mise/config.toml`, `mise/config.toml`, `mise.toml`, `.mise.toml`
//! 4. Environment variants, read only with `MISE_ENV=<env>`:
//!    `.config/mise/config.<env>.toml`, `.config/mise.<env>.toml`,
//!    `mise/config.<env>.toml`, `mise.<env>.toml`, `.mise/config.<env>.toml`,
//!    `.mise.<env>.toml`. Their pins carry that environment.
//!
//! `*.local.toml` files, including in `conf.d`, are one machine's overrides,
//! not the repository's, and are not read. Parent directories are the
//! member's, not the repository's, and are not read either.
use crate::pins::{
    Environment, PinSource, Reading, SourceFile, SourceKind,
    mise_config::{self, push_pins},
    source_file::{list_workspace_directory, read_workspace_file},
    tool_versions,
};
use elsecall_contract::failure::Result;
use std::path::Path;

/// conf.d files, and environment files per name pattern, a workspace may have.
const MAX_LISTED: usize = 64;

const BASE: [&str; 7] = [
    ".config/mise/config.toml",
    ".config/mise/mise.toml",
    ".config/mise.toml",
    ".mise/config.toml",
    "mise/config.toml",
    "mise.toml",
    ".mise.toml",
];

/// `(directory, stem)`: environment files are `<directory>/<stem>.<env>.toml`.
const ENVIRONMENT_FILES: [(&str, &str); 6] = [
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
        let conf_d = ".config/mise/conf.d";
        let names = list_workspace_directory(
            workspace,
            conf_d,
            |name| name.ends_with(".toml") && !name.ends_with(".local.toml"),
            MAX_LISTED,
        )?;
        for name in names {
            read_file(workspace, &format!("{conf_d}/{name}"), None, &mut reading)?;
        }
        for file in BASE {
            read_file(workspace, file, None, &mut reading)?;
        }
        for (directory, stem) in ENVIRONMENT_FILES {
            let names = list_workspace_directory(
                workspace,
                directory,
                |name| environment(name, stem).is_some(),
                MAX_LISTED,
            )?;
            for name in names {
                let file = if directory.is_empty() {
                    name.clone()
                } else {
                    format!("{directory}/{name}")
                };
                read_file(workspace, &file, environment(&name, stem), &mut reading)?;
            }
        }
        Ok(reading)
    }
}

/// The environment of `<stem>.<env>.toml`; `local` and dotted names are not one.
fn environment(name: &str, stem: &str) -> Option<Environment> {
    name.strip_prefix(stem)?
        .strip_prefix('.')?
        .strip_suffix(".toml")
        .and_then(Environment::new)
}

fn read_file(
    workspace: &Path,
    file: &str,
    environment: Option<Environment>,
    reading: &mut Reading,
) -> Result<()> {
    let Some(text) = read_workspace_file(workspace, file)? else {
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
                    pin.target.environment().map(|env| env.as_str().to_owned()),
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
    fn every_file_mise_reads_is_read_in_its_order() {
        let (_dir, root) = workspace();
        let files = [
            ".tool-versions",
            ".config/mise/conf.d/a.toml",
            ".config/mise/conf.d/b.toml",
            ".config/mise/config.toml",
            ".config/mise/mise.toml",
            ".config/mise.toml",
            ".mise/config.toml",
            "mise/config.toml",
            "mise.toml",
            ".mise.toml",
            ".config/mise/config.ci.toml",
            ".config/mise.ci.toml",
            "mise/config.ci.toml",
            "mise.ci.toml",
            "mise.signing.toml",
            ".mise/config.ci.toml",
            ".mise.ci.toml",
        ];
        for file in files {
            let text = if file == ".tool-versions" {
                ""
            } else {
                "[tools]\n"
            };
            write(&root, file, text);
        }
        let ignored = [
            ".config/mise/mise.ci.toml",
            ".config/mise/conf.d/me.local.toml",
            ".config/mise/config.local.toml",
            "mise.local.toml",
            "mise.ci.local.toml",
            ".mise.local.toml",
            "mise.a.b.toml",
            "mise..toml",
            "notmise.ci.toml",
        ];
        for file in ignored {
            write(&root, file, "[tools]\nignored = \"1.0.0\"\n");
        }
        let reading = MiseProject.read(&root).unwrap();
        let read: Vec<&str> = reading
            .files
            .iter()
            .map(|file| file.file.as_str())
            .collect();
        assert_eq!(read, files);
        assert!(reading.pins.is_empty());
        assert!(
            reading
                .files
                .iter()
                .all(|file| file.kind == SourceKind::MiseProject)
        );
    }

    #[test]
    fn pins_carry_their_file_and_environment() {
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
        write(&root, ".tool-versions", "nodejs 22.19.0\n");
        let reading = MiseProject.read(&root).unwrap();
        assert_eq!(
            summary(&reading),
            [
                (
                    "node".into(),
                    "22.19.0".into(),
                    None,
                    ".tool-versions".into()
                ),
                ("bun".into(), "latest".into(), None, "mise.toml".into()),
                ("python".into(), "3.10.4".into(), None, "mise.toml".into()),
                (
                    "packslip:example/crem".into(),
                    "0.2.0".into(),
                    Some("signing".into()),
                    "mise.signing.toml".into()
                ),
            ]
        );
    }

    #[test]
    fn too_many_environment_files_are_refused() {
        let (_dir, root) = workspace();
        for index in 0..=MAX_LISTED {
            write(&root, &format!("mise.e{index}.toml"), "");
        }
        let error = MiseProject.read(&root).unwrap_err();
        assert_eq!(error.message, ".: at most 64 mise config files.");
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
