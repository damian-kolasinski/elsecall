//! Installing a mise tool: `mise install '<tool>[<options>]@<version>'`.
//!
//! The version is named explicitly, never `mise install` in a workspace, so no
//! repository's config is read and no repository code runs. Mise runs as the
//! inventory runs it: the executable the inventory finds (`ELSECALL_MISE` or
//! the usual places), in the home folder, with `MISE` and `MISE_*` removed from
//! its environment and `MISE_YES=1` added, since nobody is there to answer. The
//! carried install options go in mise's bracket syntax; their values were
//! checked when the pins were read so they cannot end it. A step may take 30
//! minutes, enough for a tool mise compiles.
//!
//! A member without mise, as a bare Mac is, first copies the master's: the
//! executable the master's inventory runs, over the members' channel
//! ([`Source`]). It is written as `mise` in a private folder beside where the
//! inventory looks first (`ELSECALL_MISE`, or `~/.local/bin/mise`), since mise
//! behaves as a shim under any other name, and moved there only once the copy
//! arrived whole and `mise --version` runs, so a member never keeps a mise that
//! does not start; the folder is removed however it ends. That is the step's
//! `bootstrap` stage; the steps after it find mise in place, and when it fails
//! the applier fails them with it rather than copying again.
use crate::{
    artifact::Artifact,
    install::{Installer, Ran, StepFailure, Work, request::InstallStep, source::Source},
    mise_cli,
    tool_inventory::Manager,
};
use futures_util::future::BoxFuture;
use std::{
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

const LIMIT: Duration = Duration::from_secs(30 * 60);
/// How long the copied mise has to say its version.
const CHECK: Duration = Duration::from_secs(30);
/// Far more than a mise executable (Homebrew's mise 2026.11 is 237 MB); it is
/// held in memory.
const MAXIMUM_MISE: u64 = 1024 * 1024 * 1024;
/// How long copying mise from the master may take as a whole.
const COPY: Duration = Duration::from_secs(10 * 60);

pub(crate) struct MiseInstaller {
    pub(crate) source: Arc<dyn Source>,
}

impl Installer for MiseInstaller {
    fn manager(&self) -> Manager {
        Manager::Mise
    }

    fn install<'a>(
        &'a self,
        step: &'a InstallStep,
        work: &'a dyn Work,
    ) -> BoxFuture<'a, Result<(), StepFailure>> {
        Box::pin(async move {
            let failed = |code| StepFailure {
                stage: "install",
                code,
            };
            let (home, program) = mise_cli::located().map_err(|_| failed("mise_unusable"))?;
            let program = match program {
                Some(program) => program,
                None => self.bootstrap(work).await?,
            };
            let mut command = tokio::process::Command::new(program);
            command.args(["install", &spec(step)]).current_dir(&home);
            for name in mise_cli::variables_to_remove() {
                command.env_remove(name);
            }
            command.env("MISE_YES", "1");
            match work.run(command, LIMIT).await {
                Ran::Ended(status) if status.success() => Ok(()),
                Ran::Ended(_) => Err(failed("tool_install_failed")),
                Ran::TimedOut => Err(failed("tool_install_timeout")),
                Ran::NotStarted => Err(failed("mise_unusable")),
            }
        })
    }
}

impl MiseInstaller {
    /// Copies the master's mise into place; its path once it runs.
    async fn bootstrap(&self, work: &dyn Work) -> Result<PathBuf, StepFailure> {
        let failed = |code| StepFailure {
            stage: "bootstrap",
            code,
        };
        let target = mise_cli::install_target().ok_or(failed("mise_unusable"))?;
        let mut bytes = Vec::new();
        tokio::time::timeout(
            COPY,
            self.source.fetch(&Artifact::Mise, &mut bytes, MAXIMUM_MISE),
        )
        .await
        .map_err(|_| failed("artifact_cut"))?
        .map_err(failed)?;
        let folder = target.clone();
        let staged = tokio::task::spawn_blocking(move || Staged::write(&folder, &bytes))
            .await
            .map_err(|_| failed("mise_copy_failed"))?
            .map_err(|_| failed("mise_copy_failed"))?;
        // mise reads the name it runs under, and runs as a shim under any
        // other: the copy is tried as `mise`, in a folder of its own.
        let mut check = tokio::process::Command::new(staged.executable());
        check.arg("--version");
        for name in mise_cli::variables_to_remove() {
            check.env_remove(name);
        }
        match work.run(check, CHECK).await {
            Ran::Ended(status) if status.success() => {
                let placed = target.clone();
                tokio::task::spawn_blocking(move || staged.place(&placed))
                    .await
                    .map_err(|_| failed("mise_copy_failed"))?
                    .map_err(|_| failed("mise_copy_failed"))?;
                Ok(target)
            }
            _ => Err(failed("mise_copy_unusable")),
        }
    }
}

/// A copied mise beside where it goes, in a folder of its own that is removed
/// however the bootstrap ends.
struct Staged {
    folder: PathBuf,
}

impl Staged {
    /// Writes `bytes` as `mise` in a new private folder beside `target`.
    fn write(target: &Path, bytes: &[u8]) -> std::io::Result<Self> {
        let parent = target
            .parent()
            .ok_or_else(|| std::io::Error::other("the target has no folder"))?;
        std::fs::create_dir_all(parent)?;
        let folder = parent.join(format!(
            ".elsecall-mise-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_nanos())
        ));
        std::fs::DirBuilder::new().mode(0o700).create(&folder)?;
        let staged = Self { folder };
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o755)
            .open(staged.executable())?;
        std::io::Write::write_all(&mut file, bytes)?;
        file.sync_all()?;
        Ok(staged)
    }

    fn executable(&self) -> PathBuf {
        self.folder.join("mise")
    }

    /// Moves the copy to `target`, replacing nothing but a link or file there.
    fn place(self, target: &Path) -> std::io::Result<()> {
        std::fs::set_permissions(self.executable(), std::fs::Permissions::from_mode(0o755))?;
        std::fs::rename(self.executable(), target)
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(self.executable());
        let _ = std::fs::remove_dir(&self.folder);
    }
}

/// `tool[option=value,…]@version`, as mise reads a tool on its command line.
pub(crate) fn spec(step: &InstallStep) -> String {
    let options: Vec<String> = step
        .options
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect();
    if options.is_empty() {
        format!("{}@{}", step.tool.as_str(), step.version.as_str())
    } else {
        format!(
            "{}[{}]@{}",
            step.tool.as_str(),
            options.join(","),
            step.version.as_str()
        )
    }
}

#[cfg(test)]
mod tests {
    use crate::install::{mise::spec, request::WireStep};
    use serde_json::json;

    #[test]
    fn a_step_names_its_tool_options_and_version() {
        let step = |value| {
            serde_json::from_value::<WireStep>(value)
                .unwrap()
                .accept()
                .unwrap()
        };
        assert_eq!(
            spec(&step(
                json!({"manager": "mise", "tool": "node", "version": "22.19.0"})
            )),
            "node@22.19.0"
        );
        assert_eq!(
            spec(&step(
                json!({"manager": "mise", "tool": "ubi:cli/cli", "version": "2.0.0",
                              "options": {"exe": "gh", "bin_path": "bin"}})
            )),
            "ubi:cli/cli[bin_path=bin,exe=gh]@2.0.0"
        );
    }
}
