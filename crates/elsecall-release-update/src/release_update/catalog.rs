//! The latest stable published GitHub release. It is only a hint: packslip
//! authenticates a release before it is installed.
//!
//! The release is read from GitHub's web pages rather than its REST API. Without a
//! token the API allows 60 requests an hour per address, which every Mac behind one
//! router shares with each other and with mise, so update checks failed on any
//! network with a few machines.
//!
//! GitHub's latest release is the most recently published stable one, not
//! necessarily the highest version; a check only offers a release newer than the
//! running one, so it can never downgrade.
use super::{REPOSITORY, Release, ReleaseCatalog, ReleaseVersion, invalid};
use elsecall_failure::{Failure, Result};
use std::{path::Path, time::Duration};

pub struct GithubReleases;

impl ReleaseCatalog for GithubReleases {
    fn latest(&self) -> Result<Option<Release>> {
        if std::env::consts::OS != "macos" || std::env::consts::ARCH != "aarch64" {
            return Ok(None);
        }
        let latest = format!("https://github.com/{REPOSITORY}/releases/latest");
        let response = curl(&[
            "--head",
            "--output",
            "/dev/null",
            "--write-out",
            "%{http_code} %{redirect_url}",
            &latest,
        ])?;
        let redirect = redirected(&response)?;
        latest_installable(&redirect, |release, name| {
            let asset = format!(
                "https://github.com/{REPOSITORY}/releases/download/v{}/{name}",
                release.version
            );
            published(&curl(&[
                "--head",
                "--location",
                "--proto-redir",
                "=https",
                "--output",
                "/dev/null",
                "--write-out",
                "%{http_code}",
                &asset,
            ])?)
        })
    }
}

/// The release GitHub's latest-release page redirects to, offered only when both its
/// archive for this Mac and its packslip signature are published.
pub fn latest_installable(
    redirect: &str,
    published: impl Fn(&Release, &str) -> Result<bool>,
) -> Result<Option<Release>> {
    let releases = format!("https://github.com/{REPOSITORY}/releases");
    // A repository without releases redirects to its release list.
    if redirect == releases {
        return Ok(None);
    }
    let tag = redirect
        .strip_prefix(&format!("{releases}/tag/"))
        .ok_or_else(|| invalid("GitHub's latest release page redirected somewhere unexpected."))?;
    // Only a `v` tag of an exact stable version is a release this updater installs.
    let Some(Ok(version)) = tag
        .strip_prefix('v')
        .map(|version| ReleaseVersion::try_from(version.to_owned()))
    else {
        return Ok(None);
    };
    let release = Release { version };
    for name in [release.filename(), "packslip.sigstore.json".to_owned()] {
        if !published(&release, &name)? {
            return Ok(None);
        }
    }
    Ok(Some(release))
}

/// Whether an asset exists, from its final HTTP status. Only a missing asset is an
/// answer; anything else, such as a rate limit or a server error, fails the check so
/// the cached offer stays.
fn published(status: &str) -> Result<bool> {
    match status.trim() {
        "200" => Ok(true),
        "404" => Ok(false),
        other => Err(unreachable(&format!(
            "GitHub answered {} for a release asset",
            if other.is_empty() { "nothing" } else { other }
        ))),
    }
}

/// The redirect target of a `%{http_code} %{redirect_url}` response.
fn redirected(response: &str) -> Result<String> {
    let response = response.trim();
    let (status, url) = response.split_once(' ').unwrap_or((response, ""));
    if status.starts_with('3') && !url.is_empty() {
        return Ok(url.to_owned());
    }
    let status = if status.is_empty() { "nothing" } else { status };
    Err(unreachable(&format!(
        "GitHub answered {status} instead of redirecting to the latest release"
    )))
}

fn curl(arguments: &[&str]) -> Result<String> {
    let mut all = vec![
        "-q",
        "--silent",
        "--proto",
        "=https",
        // Three requests make a check; together they stay within the 40 seconds
        // elsecall update check waits.
        "--connect-timeout",
        "5",
        "--max-time",
        "10",
        "--header",
        "User-Agent: elsecall-release-updater",
    ];
    all.extend_from_slice(arguments);
    let output = super::tools::run(
        Path::new("/usr/bin/curl"),
        &all,
        Path::new("/"),
        Duration::from_secs(12),
        &[],
    )
    .map_err(|_| unreachable("GitHub could not be reached"))?;
    String::from_utf8(output).map_err(|_| unreachable("GitHub answered with unreadable data"))
}

fn unreachable(reason: &str) -> Failure {
    Failure::new(
        "release_check_failed",
        format!("Elsecall could not look for new releases: {reason}."),
        "Check this Mac's internet connection. The service looks again within the hour; elsecall update check looks now.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_200_and_404_answer_whether_an_asset_is_published() {
        assert!(published("200").unwrap());
        assert!(!published("404\n").unwrap());
        for status in ["429", "500", "403", ""] {
            assert_eq!(
                published(status).unwrap_err().code,
                "release_check_failed",
                "{status:?}"
            );
        }
    }

    #[test]
    fn only_a_redirect_names_the_latest_release() {
        assert_eq!(
            redirected("302 https://github.com/a/b/releases/tag/v0.2.1\n").unwrap(),
            "https://github.com/a/b/releases/tag/v0.2.1"
        );
        for response in ["200 ", "404", "429 ", ""] {
            assert_eq!(
                redirected(response).unwrap_err().code,
                "release_check_failed",
                "{response:?}"
            );
        }
    }
}
