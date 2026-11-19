//! The Tailscale build Add Mac installs on a Mac without Tailscale: the
//! open-source `tailscale` and `tailscaled` from Homebrew's bottle for the Mac's
//! macOS, pinned here by SHA-256 so the download is checked against what this
//! Elsecall build was reviewed with.
//!
//! Bump: take `versions.stable` and each `arm64_*` file's `sha256` from
//! https://formulae.brew.sh/api/formula/tailscale.json, and add a bottle for each
//! new macOS.
use serde::Serialize;

pub const VERSION: &str = "1.104.7";

/// One bottle: the oldest macOS it runs on and its digest, which is also its
/// blob's name in Homebrew's registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Bottle {
    pub version: &'static str,
    pub macos: u32,
    pub sha256: &'static str,
}

/// Newest first. Each bottle's `tailscaled` needs at least its own macOS.
const BOTTLES: [Bottle; 3] = [
    Bottle {
        version: VERSION,
        macos: 27,
        sha256: "9474decfa9812ca996b0183c7ad77e218cdecafbd6a03e8d7458d1519145e6fd",
    },
    Bottle {
        version: VERSION,
        macos: 26,
        sha256: "cab30e65f83a47aaea0ece351b925eefa065b8ad40c56e858c9eef89d98c240e",
    },
    Bottle {
        version: VERSION,
        macos: 15,
        sha256: "72f51a3524200b18fbb593d10a8e11a69a63c6d773cff241204b4c32c0e8e214",
    },
];

/// The newest bottle that runs on `macos_major`, or `None` when the Mac's macOS
/// is older than every bottle.
pub fn for_macos(macos_major: u32) -> Option<Bottle> {
    BOTTLES
        .into_iter()
        .find(|bottle| bottle.macos <= macos_major)
}

impl Bottle {
    /// Where Homebrew's registry serves the bottle; it needs an anonymous token.
    pub fn url(&self) -> String {
        format!(
            "https://ghcr.io/v2/homebrew/core/tailscale/blobs/sha256:{}",
            self.sha256
        )
    }
}

#[cfg(test)]
mod tests {
    use crate::tailscale_bottle::*;

    #[test]
    fn the_newest_bottle_a_mac_can_run_is_chosen() {
        assert_eq!(for_macos(26).unwrap().macos, 26);
        assert_eq!(for_macos(15).unwrap().macos, 15);
        // A macOS newer than every bottle takes the newest one.
        assert_eq!(for_macos(28).unwrap().macos, 27);
        assert_eq!(for_macos(14), None);
        for bottle in BOTTLES {
            assert_eq!(bottle.sha256.len(), 64);
            assert!(bottle.sha256.bytes().all(|b| b.is_ascii_hexdigit()));
        }
        assert!(BOTTLES.windows(2).all(|pair| pair[0].macos > pair[1].macos));
    }
}
