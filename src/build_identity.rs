//! Which build of Elsecall a process runs: the package version and a build ID, the
//! first 16 hex digits of the SHA-256 digest of its executable. Two members with the
//! same build ID run byte-identical executables, which is what `fleet upgrade`
//! distributes and what version skew compares.
use crate::failure::{Failure, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::{
    path::Path,
    sync::{
        OnceLock,
        atomic::{AtomicBool, Ordering},
    },
};

/// The SHA-256 digest of an executable's content.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Digest([u8; 32]);
impl Digest {
    pub fn of(bytes: &[u8]) -> Self {
        Self(Sha256::digest(bytes).into())
    }
    pub fn of_file(path: &Path) -> std::io::Result<Self> {
        let mut file = std::fs::File::open(path)?;
        let mut hasher = Sha256::new();
        std::io::copy(&mut file, &mut hasher)?;
        Ok(Self(hasher.finalize().into()))
    }
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
    pub fn build_id(&self) -> BuildId {
        BuildId(hex(&self.0[..8]))
    }
}
impl std::fmt::Debug for Digest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Digest({})", hex(&self.0))
    }
}
impl Serialize for Digest {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&hex(&self.0))
    }
}
impl<'de> Deserialize<'de> for Digest {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        parse_hex::<32>(&text)
            .map(Self)
            .ok_or_else(|| serde::de::Error::custom("a digest is 64 lowercase hex digits"))
    }
}

/// A build's identity: 16 lowercase hex digits of its executable's digest.
#[derive(Clone, PartialEq, Eq, Hash, Debug, Serialize)]
pub struct BuildId(String);
impl BuildId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl std::str::FromStr for BuildId {
    type Err = Failure;
    fn from_str(text: &str) -> Result<Self> {
        parse_hex::<8>(text)
            .map(|_| Self(text.to_owned()))
            .ok_or_else(|| {
                Failure::new(
                    "invalid_build",
                    "A build ID is 16 lowercase hex digits.",
                    "Copy the build ID from elsecall fleet or elsecall status.",
                )
            })
    }
}
impl<'de> Deserialize<'de> for BuildId {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(|_| serde::de::Error::custom("a build ID is 16 lowercase hex digits"))
    }
}
impl std::fmt::Display for BuildId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A running build: what `status`, `fleet` and `doctor` show.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Build {
    pub version: String,
    pub build: BuildId,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn parse_hex<const N: usize>(text: &str) -> Option<[u8; N]> {
    if text.len() != N * 2
        || !text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return None;
    }
    let mut out = [0u8; N];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

static THIS: OnceLock<std::result::Result<Build, Failure>> = OnceLock::new();
static ANNOUNCED: AtomicBool = AtomicBool::new(false);

/// This process's build, read from its executable the first time it is asked.
/// The service asks at startup, before anything could replace its executable.
pub fn this_build() -> Result<Build> {
    THIS.get_or_init(|| {
        let unreadable = |error: std::io::Error| {
            Failure::new(
                "build_unidentified",
                format!("This executable could not be read to identify its build: {error}"),
                "Run elsecall install again from a complete build.",
            )
        };
        let path = std::env::current_exe().map_err(unreadable)?;
        let digest = Digest::of_file(&path).map_err(unreadable)?;
        Ok(Build {
            version: env!("CARGO_PKG_VERSION").into(),
            build: digest.build_id(),
        })
    })
    .clone()
}

/// Makes this process name its build on every member request, as the service does,
/// so members can refuse work between different builds.
pub fn announce() -> Result<Build> {
    let build = this_build()?;
    ANNOUNCED.store(true, Ordering::Relaxed);
    Ok(build)
}

/// The build this process names on member requests, once it announced one.
pub fn announced() -> Option<BuildId> {
    if ANNOUNCED.load(Ordering::Relaxed) {
        this_build().ok().map(|build| build.build)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_ids_are_the_digest_prefix_and_round_trip() {
        let digest = Digest::of(b"elsecall");
        let id = digest.build_id();
        assert_eq!(id.as_str().len(), 16);
        assert!(hex(digest.as_bytes()).starts_with(id.as_str()));
        let json = serde_json::to_string(&digest).unwrap();
        assert_eq!(serde_json::from_str::<Digest>(&json).unwrap(), digest);
        let id_json = serde_json::to_string(&id).unwrap();
        assert_eq!(serde_json::from_str::<BuildId>(&id_json).unwrap(), id);
        assert_ne!(Digest::of(b"elsecall!").build_id(), id);
    }

    #[test]
    fn malformed_ids_and_digests_are_refused() {
        for text in [
            "",
            "0123",
            "0123456789ABCDEF",
            "0123456789abcdeg",
            "0123456789abcdef0",
        ] {
            assert!(text.parse::<BuildId>().is_err(), "{text}");
        }
        assert!(serde_json::from_str::<Digest>("\"00\"").is_err());
    }
}
