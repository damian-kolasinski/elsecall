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
use crate::{
    install::{Installer, Ran, StepFailure, Work, request::InstallStep},
    mise_cli,
    tool_inventory::Manager,
};
use futures_util::future::BoxFuture;
use std::time::Duration;

const LIMIT: Duration = Duration::from_secs(30 * 60);

pub(crate) struct MiseInstaller;

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
            let program = program.ok_or(failed("mise_missing"))?;
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
