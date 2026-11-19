//! The local channel's release list: a JSON array of the versions it holds, such
//! as `["0.2.1", "0.2.2"]`. Like the published catalog it is only a hint, which
//! verification later confirms.
use super::{LocalChannel, transport};
use crate::release_update::{Release, ReleaseCatalog, ReleaseVersion, invalid};
use elsecall_contract::failure::Result;

impl ReleaseCatalog for LocalChannel {
    fn latest(&self) -> Result<Option<Release>> {
        newest(&transport::fetch(
            &self.url("releases.json"),
            2 * 1024 * 1024,
            None,
        )?)
    }
}

/// The highest listed version. Only an exact stable version is a release the
/// updater installs, so other entries are passed over.
pub(super) fn newest(list: &[u8]) -> Result<Option<Release>> {
    let versions: Vec<String> = serde_json::from_slice(list)
        .map_err(|_| invalid("The local release list is not a JSON array of versions."))?;
    Ok(versions
        .into_iter()
        .filter_map(|version| ReleaseVersion::try_from(version).ok())
        .max()
        .map(|version| Release { version }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_highest_stable_version_is_offered() {
        let offered = |list: &str| {
            newest(list.as_bytes()).map(|release| release.map(|r| r.version.to_string()))
        };
        assert_eq!(
            offered(r#"["0.2.9", "0.2.10", "0.3.0-rc.1", "v1.0.0"]"#)
                .unwrap()
                .as_deref(),
            Some("0.2.10")
        );
        assert_eq!(offered("[]").unwrap(), None);
        assert!(offered(r#"{"releases": []}"#).is_err());
    }
}
