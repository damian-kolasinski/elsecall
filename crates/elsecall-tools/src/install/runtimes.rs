//! Installing a simulator runtime: `xcodebuild -downloadPlatform <platform>
//! -buildVersion <version> -architectureVariant arm64`, as Xcode's own
//! Components settings do, with no Apple ID. The variant is named because a
//! version alone looks for a universal image, which Apple no longer offers for
//! every version (`iOS 26.2 is not available for download`); every member runs
//! Elsecall's Apple silicon build.
//!
//! The runtime comes from Apple through an installed Xcode's `xcodebuild`: from
//! 26 on, the one whose major and minor version match the runtime's (iOS 26.2
//! from Xcode 26.2), else the oldest later one; an older runtime through the
//! selected Xcode, else the newest. It runs with that Xcode as
//! `DEVELOPER_DIR`, in `/`, for up to an hour; a runtime is several gigabytes.
//! A plan installs Xcode before runtimes, so a runtime derived from a pinned
//! Xcode finds it.
use crate::{
    install::{Installer, Ran, StepFailure, Work, request::InstallStep},
    tool_inventory::{Detail, InstalledTool, Manager, compare_versions},
};
use futures_util::future::BoxFuture;
use std::{path::PathBuf, time::Duration};

const LIMIT: Duration = Duration::from_secs(60 * 60);

pub(crate) struct RuntimeInstaller;

impl Installer for RuntimeInstaller {
    fn manager(&self) -> Manager {
        Manager::Simulators
    }

    fn install<'a>(
        &'a self,
        step: &'a InstallStep,
        work: &'a dyn Work,
    ) -> BoxFuture<'a, Result<(), StepFailure>> {
        Box::pin(async move {
            let failed = |code| StepFailure {
                stage: "download",
                code,
            };
            let xcodes = tokio::task::spawn_blocking(crate::xcodes::installed)
                .await
                .map_err(|_| failed("xcode_unreadable"))?
                .map_err(|_| failed("xcode_unreadable"))?;
            let bundle =
                xcode_for(&xcodes, step.version.as_str()).ok_or(failed("xcode_missing"))?;
            let developer = bundle.join("Contents/Developer");
            let mut command = tokio::process::Command::new(developer.join("usr/bin/xcodebuild"));
            command
                .args([
                    "-downloadPlatform",
                    step.tool.as_str(),
                    "-buildVersion",
                    step.version.as_str(),
                    "-architectureVariant",
                    "arm64",
                ])
                .env("DEVELOPER_DIR", &developer)
                .current_dir("/");
            match work.run(command, LIMIT).await {
                Ran::Ended(status) if status.success() => Ok(()),
                Ran::Ended(_) | Ran::NotStarted => Err(failed("runtime_download_failed")),
                Ran::TimedOut => Err(failed("runtime_download_timeout")),
            }
        })
    }
}

/// The Xcode to download `runtime` with. From Xcode 26 on, Xcode and its
/// runtimes share their numbering: the Xcode of the runtime's major and minor
/// version, else the oldest later one, since an earlier Xcode does not know the
/// runtime; none is `xcode_missing` rather than a download bound to fail. An
/// older runtime is numbered apart from Xcode, so the selected Xcode, else the
/// newest.
fn xcode_for(xcodes: &[InstalledTool], runtime: &str) -> Option<PathBuf> {
    let path = |tool: &InstalledTool| match &tool.detail {
        Some(Detail::Xcode { path, .. }) => Some(path.clone()),
        _ => None,
    };
    let release = |version: &str| version.split('.').take(2).collect::<Vec<_>>().join(".");
    let major = |version: &str| {
        version
            .split('.')
            .next()
            .and_then(|major| major.parse::<u32>().ok())
            .unwrap_or(0)
    };
    let newest = |a: &&InstalledTool, b: &&InstalledTool| {
        compare_versions(a.version.as_str(), b.version.as_str())
    };
    if major(runtime) >= 26 {
        let wanted = release(runtime);
        return xcodes
            .iter()
            .filter(|tool| release(tool.version.as_str()) == wanted)
            .max_by(newest)
            .or_else(|| {
                xcodes
                    .iter()
                    .filter(|tool| {
                        compare_versions(&release(tool.version.as_str()), &wanted)
                            == std::cmp::Ordering::Greater
                    })
                    .min_by(newest)
            })
            .and_then(path);
    }
    xcodes
        .iter()
        .find(|tool| tool.default)
        .or_else(|| xcodes.iter().max_by(newest))
        .and_then(path)
}

#[cfg(test)]
mod tests {
    use crate::{
        install::runtimes::*,
        tool_inventory::{ToolName, ToolVersion},
    };

    fn xcode(version: &str, default: bool) -> InstalledTool {
        InstalledTool {
            name: ToolName::new("xcode").unwrap(),
            version: ToolVersion::new(version).unwrap(),
            default,
            detail: Some(Detail::Xcode {
                build: "1".into(),
                path: PathBuf::from(format!("/Applications/Xcode-{version}.app")),
            }),
        }
    }

    #[test]
    fn a_runtime_is_downloaded_with_the_xcode_of_its_release() {
        let xcodes = [
            xcode("16.4", true),
            xcode("26.0", false),
            xcode("26.0.1", false),
            xcode("26.1", false),
            xcode("26.3", false),
        ];
        let chosen = |runtime| xcode_for(&xcodes, runtime).unwrap();
        assert_eq!(
            chosen("26.0"),
            PathBuf::from("/Applications/Xcode-26.0.1.app")
        );
        assert_eq!(
            chosen("26.1"),
            PathBuf::from("/Applications/Xcode-26.1.app")
        );
        assert_eq!(
            chosen("26.2"),
            PathBuf::from("/Applications/Xcode-26.3.app"),
            "the oldest later Xcode knows the runtime"
        );
        assert!(
            xcode_for(&xcodes, "26.4").is_none(),
            "no Xcode knows it yet: missing, not the selected 16.4"
        );
        assert_eq!(
            chosen("18.5"),
            PathBuf::from("/Applications/Xcode-16.4.app"),
            "an older runtime: the selected one"
        );
        let unselected = [xcode("16.4", false), xcode("26.2", false)];
        assert_eq!(
            xcode_for(&unselected, "18.5").unwrap(),
            PathBuf::from("/Applications/Xcode-26.2.app"),
            "the newest"
        );
        assert!(xcode_for(&[], "26.2").is_none());
    }
}
