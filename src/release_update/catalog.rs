//! Published stable GitHub releases. These hints are authenticated by packslip before installation.
use super::{REPOSITORY, Release, ReleaseCatalog, ReleaseVersion, invalid};
use crate::failure::Result;
use serde::Deserialize;
use std::{path::Path, time::Duration};

pub struct GithubReleases;
#[derive(Deserialize)]
struct Published {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<Asset>,
}
#[derive(Deserialize)]
struct Asset {
    name: String,
}

impl ReleaseCatalog for GithubReleases {
    fn latest(&self) -> Result<Option<Release>> {
        if std::env::consts::OS != "macos" || std::env::consts::ARCH != "aarch64" {
            return Ok(None);
        }
        let url = format!("https://api.github.com/repos/{REPOSITORY}/releases?per_page=100");
        let output = super::tools::run(
            Path::new("/usr/bin/curl"),
            &[
                "-q",
                "--fail",
                "--silent",
                "--location",
                "--proto",
                "=https",
                "--proto-redir",
                "=https",
                "--connect-timeout",
                "10",
                "--max-time",
                "25",
                "--max-filesize",
                "2097152",
                "--header",
                "Accept: application/vnd.github+json",
                "--header",
                "User-Agent: elsecall-release-updater",
                &url,
            ],
            Path::new("/"),
            Duration::from_secs(30),
            &[],
        )?;
        select(&output)
    }
}
pub fn select(bytes: &[u8]) -> Result<Option<Release>> {
    let releases: Vec<Published> = serde_json::from_slice(bytes)
        .map_err(|_| invalid("The release catalog returned an invalid response."))?;
    Ok(releases
        .into_iter()
        .filter(|release| !release.draft && !release.prerelease)
        .filter_map(|release| {
            let version =
                ReleaseVersion::try_from(release.tag_name.strip_prefix('v')?.to_owned()).ok()?;
            let candidate = Release { version };
            let has = |name: &str| release.assets.iter().any(|asset| asset.name == name);
            (has(&candidate.filename()) && has("packslip.sigstore.json")).then_some(candidate)
        })
        .max_by(|left, right| left.version.cmp(&right.version)))
}
