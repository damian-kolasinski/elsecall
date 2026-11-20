//! Checks a local bundle with the packslip CLI against the channel's pinned key,
//! and an archive against the bundle.
use super::PACKSLIP;
use elsecall_contract::failure::{Failure, Result};
use elsecall_host::{paths::Paths, private_files};
use serde::Deserialize;
use std::{
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

/// How to run the packslip CLI.
#[derive(Clone, Debug)]
pub enum Packslip {
    /// The pinned release through mise, which installs it on first use.
    Mise,
    /// An executable already on disk.
    At(PathBuf),
}

/// What packslip verified: the signed project and version, and which of the
/// given files it checked against their signed digests.
#[derive(Deserialize)]
pub struct Verified {
    pub project: String,
    pub version: String,
    pub checked_artifacts: Vec<String>,
}

impl Packslip {
    /// Verifies `bundle` with `key` and `archive` against it. A release that fails
    /// verification is `local_release_unverified`; a verifier that cannot run
    /// reports why instead.
    pub fn verify(
        &self,
        paths: &Paths,
        directory: &Path,
        key: &str,
        bundle: &Path,
        archive: &Path,
    ) -> Result<Verified> {
        let verify = [
            "verify",
            bundle.to_str().expect("UTF8 stage path"),
            "--pubkey",
            key,
            // Local keys sign offline, with no transparency log to record them.
            "--allow-unlogged",
            "--json",
            "--artifact",
            archive.to_str().expect("UTF8 stage path"),
        ];
        let report = match self {
            Self::At(program) => run(program, &verify, directory, Duration::from_secs(60), &[]),
            Self::Mise => {
                let mise = crate::release_update::tools::mise(paths)?;
                let environment = isolated(paths, directory)?;
                run(
                    &mise,
                    &["install", PACKSLIP],
                    directory,
                    Duration::from_secs(300),
                    &environment,
                )
                .map_err(|error| match error.code.as_str() {
                    "release_tool_failed" => Failure::new(
                        "local_release_verifier_unavailable",
                        format!("mise could not install {PACKSLIP} to verify local releases."),
                        "Check that this Mac reaches GitHub and that mise works, then retry.",
                    ),
                    _ => error,
                })?;
                let arguments: Vec<&str> = ["exec", PACKSLIP, "--", "packslip"]
                    .into_iter()
                    .chain(verify)
                    .collect();
                run(
                    &mise,
                    &arguments,
                    directory,
                    Duration::from_secs(60),
                    &environment,
                )
            }
        };
        let report = report.map_err(|error| match error.code.as_str() {
            "release_tool_failed" => unverified(),
            _ => error,
        })?;
        serde_json::from_slice(&report).map_err(|_| unverified())
    }
}

/// Neither the user's mise configuration nor any project above the stage may
/// choose the verifier.
fn isolated(paths: &Paths, directory: &Path) -> Result<Vec<(OsString, OsString)>> {
    let empty = directory.join("empty.toml");
    private_files::write(&empty, b"", 0o600)?;
    let ceiling = fs::canonicalize(directory.parent().expect("stage parent"))
        .map_err(|e| Failure::io("Locate release config ceiling", e))?;
    Ok(vec![
        ("MISE_GLOBAL_CONFIG_FILE".into(), empty.into_os_string()),
        ("MISE_CEILING_PATHS".into(), ceiling.into_os_string()),
        ("MISE_YES".into(), "1".into()),
        ("HOME".into(), paths.home.as_os_str().to_owned()),
    ])
}

fn run(
    program: &Path,
    arguments: &[&str],
    directory: &Path,
    timeout: Duration,
    environment: &[(OsString, OsString)],
) -> Result<Vec<u8>> {
    crate::release_update::tools::run(program, arguments, directory, timeout, environment)
}

pub(super) fn unverified() -> Failure {
    Failure::new(
        "local_release_unverified",
        "The local release did not verify against the key this build trusts.",
        "Rebuild the channel's releases with scripts/local-release.sh, which signs every build with the same local key.",
    )
}
