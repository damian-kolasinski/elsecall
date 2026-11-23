//! Installing Xcode: copied from the master's installed bundle over the
//! members' channel, with no Apple ID, then placed, licensed and, when the
//! member has none selected, selected through Elsecall's helper.
//!
//! The stages, each part of the step's admitted run:
//!
//! 1. `privileges`: the helper and this account's sudoers drop-in are in
//!    place (`elsecall tools privileges`); otherwise nothing is copied, since
//!    nothing could be placed.
//! 2. `copy`: the master streams its Xcode of exactly this version
//!    ([`Artifact::Xcode`]) into `ditto -x`, which unpacks it as this account
//!    into `~/Library/Application Support/Elsecall/xcode-staging/`. A master
//!    without that Xcode, or a step on the master itself, is
//!    `xcode_not_on_master`: a person gets it there, with an Apple ID.
//! 3. `place`: `sudo -n <helper> place <version>`, which copies the staged
//!    bundle into `/Library/Elsecall/Xcodes` as root's, only once Apple's
//!    signature verifies. The staged copy is removed whatever happens. This
//!    account then links it from `/Applications`, unless something already has
//!    the name or the account may not write there; root writes nothing there.
//!    Placing also accepts its license (`license` when that fails), so a
//!    placed Xcode is a ready one and a failure is retried by the next apply.
//! 4. `select`: `sudo -n <helper> select <version>`, only when `xcode-select`
//!    names no Xcode (Command Line Tools only, as on a bare Mac); a member that
//!    selects another Xcode keeps it.
//!
//! A version the helper would refuse (not two or three numbers, or before its
//! minimum) fails as `xcode_unsupported` before anything is copied, and a step
//! on the master itself as `xcode_not_on_master`, before any privilege is
//! asked for.
//!
//! `ELSECALL_XCODE_ADMIN`, an absolute path, replaces `sudo -n <helper>` and the
//! privileges check, as the other tool overrides do in every build: it runs as
//! this account, so it grants nothing.
use crate::{
    artifact::Artifact,
    install::{Installer, Ran, StepFailure, Work, request::InstallStep, source::Source},
    tool_inventory::Manager,
    xcode_privileges::{self, HELPER},
};
use elsecall_host::command_capture;
use futures_util::future::BoxFuture;
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

/// Where a staged bundle waits for the helper, relative to the home folder; the
/// helper reads the same place.
pub(crate) const STAGING: &str = "Library/Application Support/Elsecall/xcode-staging";
/// More than any Xcode unpacks to.
const MAXIMUM_XCODE: u64 = 64 * 1024 * 1024 * 1024;
/// Copying some 8 GB over a slow link.
const COPY: Duration = Duration::from_secs(2 * 60 * 60);
/// Placing copies the bundle again, verifies its whole signature and accepts
/// its license.
const PLACE: Duration = Duration::from_secs(90 * 60);
const SELECT: Duration = Duration::from_secs(60);

pub(crate) struct XcodeInstaller {
    pub(crate) source: Arc<dyn Source>,
}

impl Installer for XcodeInstaller {
    fn manager(&self) -> Manager {
        Manager::Xcodes
    }

    fn install<'a>(
        &'a self,
        step: &'a InstallStep,
        work: &'a dyn Work,
    ) -> BoxFuture<'a, Result<(), StepFailure>> {
        Box::pin(async move {
            let version = step.version.as_str();
            // Nothing is copied that could not be placed: the helper takes two
            // or three numbers, from its minimum on.
            if !placeable(version) {
                return Err(StepFailure {
                    stage: "copy",
                    code: "xcode_unsupported",
                });
            }
            // The master's own Xcode comes from nowhere: a person gets it.
            if self.source.master_is_self() {
                return Err(StepFailure {
                    stage: "copy",
                    code: "xcode_not_on_master",
                });
            }
            let admin = admin().await?;
            let home = std::env::var_os("HOME")
                .filter(|home| !home.is_empty())
                .map(PathBuf::from)
                .ok_or(StepFailure {
                    stage: "copy",
                    code: "xcode_copy_failed",
                })?;
            let staged = Staged::at(&home, version);
            self.copy(step, &staged).await?;
            // Placing also accepts the license: a placed Xcode is a ready one,
            // and one that fails is copied and placed again next time.
            let placed = run(&admin, work, &["place", version], PLACE).await;
            drop(staged);
            placed.map_err(|code| match code {
                Some(65) => StepFailure {
                    stage: "place",
                    code: "xcode_not_verified",
                },
                Some(70) => StepFailure {
                    stage: "license",
                    code: "xcode_license_failed",
                },
                _ => StepFailure {
                    stage: "place",
                    code: "xcode_place_failed",
                },
            })?;
            link(version);
            let selected = tokio::task::spawn_blocking(crate::xcodes::selected_developer)
                .await
                .ok()
                .and_then(Result::ok);
            // None selected, or only the Command Line Tools, as on a bare Mac;
            // unknown counts as selected, so a member's choice is never moved.
            let none = matches!(selected, Some(None))
                || matches!(&selected, Some(Some(path)) if path.starts_with("/Library/Developer/CommandLineTools"));
            if none {
                run(&admin, work, &["select", version], SELECT)
                    .await
                    .map_err(|_| StepFailure {
                        stage: "select",
                        code: "xcode_select_failed",
                    })?;
            }
            Ok(())
        })
    }
}

/// Two or three numbers, from the helper's minimum on.
fn placeable(version: &str) -> bool {
    let parts: Vec<u32> = version
        .split('.')
        .map_while(|part| part.parse().ok())
        .collect();
    let minimum: Vec<u32> = xcode_privileges::MINIMUM
        .split('.')
        .filter_map(|part| part.parse().ok())
        .collect();
    (2..=3).contains(&parts.len())
        && parts.len() == version.split('.').count()
        && parts[..2] >= minimum[..2]
}

impl XcodeInstaller {
    /// Streams the master's Xcode into `ditto -x`, unpacking it as staged.
    async fn copy(&self, step: &InstallStep, staged: &Staged) -> Result<(), StepFailure> {
        let failed = |code| StepFailure {
            stage: "copy",
            code,
        };
        let path = staged.path.clone();
        tokio::task::spawn_blocking(move || prepare(&path))
            .await
            .map_err(|_| failed("xcode_copy_failed"))?
            .map_err(|_| failed("xcode_copy_failed"))?;
        let mut unpack = tokio::process::Command::new("/usr/bin/ditto");
        unpack
            .args(["-x", "-"])
            .arg(&staged.path)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = elsecall_host::child_start::spawn_async(&mut unpack)
            .map_err(|_| failed("xcode_copy_failed"))?;
        let mut stdin = child.stdin.take().expect("piped stdin");
        let artifact = Artifact::Xcode(step.version.clone());
        let fetched = tokio::time::timeout(
            COPY,
            self.source.fetch(&artifact, &mut stdin, MAXIMUM_XCODE),
        )
        .await
        .unwrap_or(Err("artifact_cut"));
        drop(stdin);
        let unpacked = child.wait().await.is_ok_and(|status| status.success());
        match fetched {
            Ok(_) if unpacked => Ok(()),
            Ok(_) => Err(failed("xcode_copy_failed")),
            Err("artifact_unavailable" | "artifact_on_master") => {
                Err(failed("xcode_not_on_master"))
            }
            Err(code) => Err(failed(code)),
        }
    }
}

/// Links a placed Xcode from `/Applications` as this account, for people and
/// tools that look there; the inventory finds it in the store either way. Not
/// with an Applications override, which has no store.
fn link(version: &str) {
    if command_capture::path_override("ELSECALL_XCODE_APPLICATIONS").is_some() {
        return;
    }
    let link = Path::new("/Applications").join(format!("Xcode-{version}.app"));
    let bundle = Path::new(xcode_privileges::STORE).join(format!("Xcode-{version}.app"));
    if std::fs::symlink_metadata(&link).is_err() {
        let _ = std::os::unix::fs::symlink(bundle, link);
    }
}

/// An empty staging folder for the bundle: its parent made if needed, and a
/// leftover from an earlier try removed.
fn prepare(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::remove_dir_all(path) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

/// The staged bundle, removed however the step ends.
struct Staged {
    path: PathBuf,
}

impl Staged {
    fn at(home: &Path, version: &str) -> Self {
        Self {
            path: home.join(STAGING).join(format!("Xcode-{version}.app")),
        }
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        // Some 8 GB of files: removed off the async worker.
        let path = std::mem::take(&mut self.path);
        tokio::task::spawn_blocking(move || std::fs::remove_dir_all(path));
    }
}

/// How the root steps are taken: `sudo -n <helper>`, or the override.
async fn admin() -> Result<Vec<PathBuf>, StepFailure> {
    let missing = StepFailure {
        stage: "privileges",
        code: "xcode_privileges_missing",
    };
    match command_capture::classify(command_capture::path_override("ELSECALL_XCODE_ADMIN")) {
        command_capture::OverridePath::Absolute(path) => Ok(vec![path]),
        command_capture::OverridePath::NotAbsolute => Err(missing),
        command_capture::OverridePath::Unset => {
            let ready = tokio::task::spawn_blocking(xcode_privileges::status)
                .await
                .ok()
                .and_then(Result::ok)
                .is_some_and(|status| status.ready);
            if ready {
                Ok(vec!["/usr/bin/sudo".into(), "-n".into(), HELPER.into()])
            } else {
                Err(missing)
            }
        }
    }
}

/// Runs one root step through `admin`; its exit code when it failed.
async fn run(
    admin: &[PathBuf],
    work: &dyn Work,
    arguments: &[&str],
    limit: Duration,
) -> Result<(), Option<i32>> {
    let mut command = tokio::process::Command::new(&admin[0]);
    command.args(&admin[1..]).args(arguments).current_dir("/");
    match work.run(command, limit).await {
        Ran::Ended(status) if status.success() => Ok(()),
        Ran::Ended(status) => Err(status.code()),
        Ran::TimedOut | Ran::NotStarted | Ran::FailedWith(_) => Err(None),
    }
}

#[cfg(test)]
mod tests {
    use crate::install::xcode::placeable;

    #[test]
    fn only_versions_the_helper_places_are_copied() {
        for version in ["16.0", "26.2", "26.0.1"] {
            assert!(placeable(version), "{version}");
        }
        for version in ["15.4", "26", "26.2.1.4", "13.0"] {
            assert!(!placeable(version), "{version}");
        }
    }
}
