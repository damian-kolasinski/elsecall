//! The release channel this build follows, fixed when it is compiled.
//!
//! Published builds follow only the signed GitHub releases. A build made with the
//! `local-release-channel` feature follows the local channel, and trusts the key,
//! that `ELSECALL_LOCAL_RELEASE_CHANNEL` and `ELSECALL_LOCAL_RELEASE_KEY` named at
//! compile time. Nothing read at run time can switch a build to another channel,
//! and the release workflow never enables the feature.

use super::{
    ReleaseCatalog, ReleaseDownload,
    local_channel::{LocalChannel, LocalDownload, Packslip},
};
use elsecall_failure::Result;
use elsecall_local_state::paths::Paths;
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Channel {
    Published,
    Local(LocalChannel),
}
impl Channel {
    pub fn this_build() -> Result<Self> {
        #[cfg(feature = "local-release-channel")]
        return local::channel().map(Self::Local);
        #[cfg(not(feature = "local-release-channel"))]
        Ok(Self::Published)
    }
    pub fn catalog(&self) -> Arc<dyn ReleaseCatalog> {
        match self {
            Self::Published => Arc::new(super::catalog::GithubReleases),
            Self::Local(channel) => Arc::new(channel.clone()),
        }
    }
    pub fn download(&self, paths: &Paths) -> Box<dyn ReleaseDownload> {
        match self {
            Self::Published => Box::new(super::download::MiseDownload {
                paths: paths.clone(),
            }),
            Self::Local(channel) => Box::new(LocalDownload {
                paths: paths.clone(),
                channel: channel.clone(),
                packslip: Packslip::Mise,
            }),
        }
    }
}

#[cfg(feature = "local-release-channel")]
mod local {
    use super::LocalChannel;
    use elsecall_failure::Result;

    /// Lets packaging tell this build from a publishable one; see
    /// `distribution/package.py`.
    #[used]
    static MARKER: [u8; 36] = *b"ELSECALL-LOCAL-RELEASE-CHANNEL-BUILD";

    pub(super) fn channel() -> Result<LocalChannel> {
        // Keep the marker in the executable whatever the linker strips.
        std::hint::black_box(&MARKER);
        LocalChannel::new(
            env!(
                "ELSECALL_LOCAL_RELEASE_CHANNEL",
                "A local-release-channel build needs ELSECALL_LOCAL_RELEASE_CHANNEL; build it with scripts/local-release.sh."
            ),
            env!(
                "ELSECALL_LOCAL_RELEASE_KEY",
                "A local-release-channel build needs ELSECALL_LOCAL_RELEASE_KEY; build it with scripts/local-release.sh."
            ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(feature = "local-release-channel"))]
    #[test]
    fn builds_without_the_feature_follow_only_published_releases() {
        // No environment read at run time may select the local channel.
        assert_eq!(Channel::this_build().unwrap(), Channel::Published);
    }

    #[cfg(feature = "local-release-channel")]
    #[test]
    fn feature_builds_follow_the_channel_they_were_built_for() {
        let Channel::Local(channel) = Channel::this_build().unwrap() else {
            panic!("a local-release-channel build follows the published channel");
        };
        assert_eq!(
            channel.location(),
            env!("ELSECALL_LOCAL_RELEASE_CHANNEL").trim_end_matches('/')
        );
    }
}
