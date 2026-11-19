//! A release channel served from a developer's own builds, so the real update path
//! can be exercised without publishing. Only builds made with the
//! `local-release-channel` feature follow it (see `channel`).
//!
//! The channel is a directory, or an HTTP server publishing one:
//!
//! ```text
//! releases.json                      the versions it holds, as ["1.2.2", "1.2.3"]
//! v1.2.3/elsecall-v1.2.3-aarch64-apple-darwin.tar.gz
//! v1.2.3/packslip.sigstore.json      the archive's bundle, signed with the local key
//! ```
//!
//! As with published releases, the list is only a hint. Verification is real: the
//! packslip CLI checks each bundle against the key pinned into the build and the
//! archive against the bundle before anything from the archive runs.
mod catalog;
mod download;
mod transport;
mod verifier;

pub use download::LocalDownload;
pub use verifier::Packslip;

use base64::{Engine, engine::general_purpose::STANDARD};
use elsecall_failure::{Failure, Result};

/// The packslip project that local bundles name. `.localhost` names no host
/// anyone can publish under, so no published release can carry it.
pub const PROJECT: &str = "elsecall.localhost";
/// The packslip release that verifies local bundles, as `mise.toml` pins it.
pub const PACKSLIP: &str = "github:jdx/packslip@1.11.1";

/// Where a local channel lives and the public key its releases must be signed with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalChannel {
    base: String,
    key: String,
}
impl LocalChannel {
    /// `location` is an `http://`, `https://` or `file:///` URL of the channel's
    /// directory; `key` is the minisign public key line `packslip keygen` writes.
    pub fn new(location: &str, key: &str) -> Result<Self> {
        let base = location.trim_end_matches('/');
        let addressed = ["http://", "https://", "file:///"]
            .iter()
            .any(|scheme| base.len() > scheme.len() && base.starts_with(scheme));
        if !addressed || base.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(misconfigured(
                "The local release channel must be an http://, https:// or file:/// URL.",
            ));
        }
        let key = key.trim();
        let decoded = STANDARD
            .decode(key)
            .map_err(|_| misconfigured("The local release key is not a minisign public key."))?;
        if decoded.len() != 42 || &decoded[..2] != b"Ed" {
            return Err(misconfigured(
                "The local release key is not a minisign Ed25519 public key.",
            ));
        }
        Ok(Self {
            base: base.to_owned(),
            key: key.to_owned(),
        })
    }
    pub fn location(&self) -> &str {
        &self.base
    }
    fn url(&self, path: &str) -> String {
        format!("{}/{path}", self.base)
    }
}

fn misconfigured(message: &str) -> Failure {
    Failure::new(
        "local_release_channel_invalid",
        message,
        "Rebuild with scripts/local-release.sh, which sets the channel and its key.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pinned_verifier_is_the_packslip_release_the_repository_uses() {
        let manifest: toml::Value =
            toml::from_str(include_str!("../../../mise.toml")).expect("mise.toml");
        let (tool, version) = PACKSLIP.rsplit_once('@').expect("pinned version");
        assert_eq!(manifest["tools"][tool].as_str(), Some(version));
    }
}
