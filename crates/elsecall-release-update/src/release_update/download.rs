//! Mise's packslip backend verifies the fixed repository/workflow/issuer before exporting files.
use super::{ISSUER, Release, ReleaseDownload, StagedRelease, TOOL, invalid};
use elsecall_contract::build_identity::Digest;
use elsecall_contract::failure::{Failure, Result};
use elsecall_local_state::{paths::Paths, private_files};
use std::{
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

pub struct MiseDownload {
    pub paths: Paths,
}
impl ReleaseDownload for MiseDownload {
    fn stage(&self, release: &Release, directory: &Path) -> Result<StagedRelease> {
        let mise = super::tools::mise(&self.paths)?;
        private_files::directory(directory)?;
        let config = directory.join("mise.toml");
        let configuration = format!(
            "[tools]\n{} = {{ version = {}, identity = {}, issuer = {} }}\n",
            quoted(TOOL),
            quoted(&release.version.to_string()),
            quoted(&release.identity()),
            quoted(ISSUER)
        );
        private_files::write(&config, configuration.as_bytes(), 0o600)?;
        let empty = directory.join("empty.toml");
        private_files::write(&empty, b"", 0o600)?;
        let data = directory.join("data");
        let environment: Vec<(OsString, OsString)> = [
            ("MISE_CONFIG_FILE", config),
            ("MISE_GLOBAL_CONFIG_FILE", empty),
            ("MISE_CONFIG_DIR", directory.join("config")),
            ("MISE_DATA_DIR", data.clone()),
            ("MISE_CACHE_DIR", directory.join("cache")),
            ("MISE_STATE_DIR", directory.join("state")),
            // The ceiling is excluded, so retain this stage's config while
            // ignoring all user configs in ancestor directories.
            (
                "MISE_CEILING_PATHS",
                fs::canonicalize(directory.parent().expect("stage parent"))
                    .map_err(|e| Failure::io("Locate release config ceiling", e))?,
            ),
        ]
        .into_iter()
        .map(|(key, value)| (key.into(), value.into_os_string()))
        .chain([
            ("MISE_YES".into(), "1".into()),
            ("HOME".into(), self.paths.home.as_os_str().to_owned()),
        ])
        .collect();
        super::tools::run(
            &mise,
            &[
                "trust",
                directory.join("mise.toml").to_str().expect("UTF8 path"),
            ],
            directory,
            Duration::from_secs(30),
            &environment,
        )?;
        super::tools::run(
            &mise,
            &["install", &format!("{TOOL}@{}", release.version)],
            directory,
            Duration::from_secs(300),
            &environment,
        )?;
        let found = super::tools::run(
            &mise,
            &["which", "elsecall"],
            directory,
            Duration::from_secs(30),
            &environment,
        )?;
        let path = std::str::from_utf8(&found)
            .ok()
            .map(str::trim)
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .ok_or_else(|| invalid("The verified release executable could not be located."))?;
        let executable =
            fs::canonicalize(path).map_err(|e| Failure::io("Locate verified release", e))?;
        let data = fs::canonicalize(data).map_err(|e| Failure::io("Locate verified install", e))?;
        let parent = executable.parent().expect("executable parent");
        if !executable.starts_with(data) || !parent.join(".mise-packslip.json").is_file() {
            return Err(invalid("Mise did not export a verified packslip release."));
        }
        verified(executable, release, directory)
    }
}
/// The release's CLI and its window companion beside it, each a bounded regular
/// file, with the CLI reporting the release's own version. Shared by every channel
/// once its own verification has exported the files.
pub(super) fn verified(
    executable: PathBuf,
    release: &Release,
    directory: &Path,
) -> Result<StagedRelease> {
    let window = executable.with_file_name("elsecall-ui");
    for binary in [&executable, &window] {
        let metadata = fs::symlink_metadata(binary)
            .map_err(|e| Failure::io("Inspect release executable", e))?;
        if !metadata.is_file() || metadata.len() > 256 * 1024 * 1024 {
            return Err(invalid(
                "The verified release is missing a bounded executable.",
            ));
        }
    }
    let version = super::tools::run(
        &executable,
        &["--version"],
        directory,
        Duration::from_secs(10),
        &[],
    )?;
    if std::str::from_utf8(&version).ok().map(str::trim)
        != Some(format!("elsecall {}", release.version).as_str())
    {
        return Err(invalid(
            "The verified executable reports a different release version.",
        ));
    }
    Ok(StagedRelease {
        digest: Digest::of_file(&executable).map_err(|e| Failure::io("Hash release", e))?,
        window_digest: Digest::of_file(&window).map_err(|e| Failure::io("Hash window", e))?,
        executable,
        window,
    })
}
fn quoted(value: &str) -> String {
    serde_json::to_string(value).expect("TOML basic string")
}
