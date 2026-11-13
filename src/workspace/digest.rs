//! Content digests: SHA-256 of file bytes, symlink targets and listings.
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest as _, Sha256};
use std::{fmt, io::Read};

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Digest(pub [u8; 32]);

impl Digest {
    pub fn of(bytes: &[u8]) -> Self {
        Self(Sha256::digest(bytes).into())
    }

    /// Hashes everything `reader` yields.
    pub fn of_reader(mut reader: impl Read) -> std::io::Result<(Self, u64)> {
        let mut hasher = Sha256::new();
        let mut buffer = vec![0u8; 256 * 1024];
        let mut total = 0u64;
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    hasher.update(&buffer[..n]);
                    total += n as u64;
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        Ok((Self(hasher.finalize().into()), total))
    }

    pub fn hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }

    pub fn parse(text: &str) -> Option<Self> {
        if text.len() != 64 {
            return None;
        }
        let mut bytes = [0u8; 32];
        for (i, chunk) in text.as_bytes().chunks(2).enumerate() {
            bytes[i] = u8::from_str_radix(std::str::from_utf8(chunk).ok()?, 16).ok()?;
        }
        Some(Self(bytes))
    }
}

/// Incremental hashing, for content that arrives in chunks.
#[derive(Default)]
pub struct Hasher(Sha256);
impl Hasher {
    pub fn update(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }
    pub fn finish(self) -> Digest {
        Digest(self.0.finalize().into())
    }
}

impl fmt::Debug for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Digest({})", &self.hex()[..12])
    }
}
impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.hex())
    }
}
impl Serialize for Digest {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.hex())
    }
}
impl<'de> Deserialize<'de> for Digest {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Digest::parse(&text).ok_or_else(|| serde::de::Error::custom("invalid digest"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_and_round_trips_hex() {
        let digest = Digest::of(b"abc");
        assert_eq!(
            digest.hex(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(Digest::parse(&digest.hex()), Some(digest));
        assert_eq!(Digest::of_reader(&b"abc"[..]).unwrap(), (digest, 3));
        let mut hasher = Hasher::default();
        hasher.update(b"a");
        hasher.update(b"bc");
        assert_eq!(hasher.finish(), digest);
        assert!(Digest::parse("zz").is_none());
    }
}
