//! Keyed fingerprints of synced content.
//!
//! Secrets are often short, so a plain hash shown in `sync status` could be
//! reversed by guessing. Every fingerprint is therefore keyed with the fleet's sync
//! key: 32 random bytes the first master creates, which travel only inside replicas
//! (owner-only state) and the encrypted sync channel. Output shows only the first
//! 16 hex digits.
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest as _, Sha256};
use std::fmt;
use zeroize::Zeroizing;

const DOMAIN: &[u8] = b"elsecall/config-sync/content/v1\0";

/// The fleet's fingerprint key. Never printed: `Debug` is redacted, and it
/// serializes only into replicas and the sync channel.
#[derive(Clone, PartialEq, Eq)]
pub struct SyncKey(Zeroizing<[u8; 32]>);

impl SyncKey {
    /// A fresh random key; `None` when the system has no randomness.
    pub fn generate() -> Option<Self> {
        let mut bytes = Zeroizing::new([0u8; 32]);
        getrandom::fill(bytes.as_mut()).ok()?;
        Some(Self(bytes))
    }

    pub fn hasher(&self) -> FingerprintHasher {
        let mut hasher = Sha256::new();
        hasher.update(self.0.as_ref());
        hasher.update(DOMAIN);
        FingerprintHasher(hasher)
    }

    pub fn fingerprint(&self, bytes: &[u8]) -> Fingerprint {
        let mut hasher = self.hasher();
        hasher.update(bytes);
        hasher.finish()
    }

    /// A keyed digest of public values, such as a manifest's shape.
    pub fn digest_labelled(&self, label: &str, bytes: &[u8]) -> Fingerprint {
        let mut hasher = Sha256::new();
        hasher.update(self.0.as_ref());
        hasher.update(label.as_bytes());
        hasher.update([0]);
        hasher.update(bytes);
        Fingerprint(hasher.finalize().into())
    }
}

impl fmt::Debug for SyncKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SyncKey(<redacted>)")
    }
}
impl Serialize for SyncKey {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        crate::wire::bytes::serialize(self.0.as_ref(), serializer)
    }
}
impl<'de> Deserialize<'de> for SyncKey {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let bytes = Zeroizing::new(crate::wire::bytes::deserialize(deserializer)?);
        let array: [u8; 32] = bytes
            .as_slice()
            .try_into()
            .map_err(|_| serde::de::Error::custom("a sync key has 32 bytes"))?;
        Ok(Self(Zeroizing::new(array)))
    }
}

pub struct FingerprintHasher(Sha256);
impl FingerprintHasher {
    pub fn update(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }
    pub fn finish(self) -> Fingerprint {
        Fingerprint(self.0.finalize().into())
    }
}

/// A keyed fingerprint. Serialized in full (in replicas and on the channel);
/// `short` is what output shows.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Fingerprint(pub [u8; 32]);

impl Fingerprint {
    pub fn hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }
    /// The first 16 hex digits, as output shows them.
    pub fn short(&self) -> String {
        self.hex()[..16].to_owned()
    }
    pub fn parse(text: &str) -> Option<Self> {
        if text.len() != 64 || !text.is_ascii() {
            return None;
        }
        let mut bytes = [0u8; 32];
        for (i, chunk) in text.as_bytes().chunks(2).enumerate() {
            bytes[i] = u8::from_str_radix(std::str::from_utf8(chunk).ok()?, 16).ok()?;
        }
        Some(Self(bytes))
    }
}
impl fmt::Debug for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Fingerprint({})", self.short())
    }
}
impl Serialize for Fingerprint {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.hex())
    }
}
impl<'de> Deserialize<'de> for Fingerprint {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::parse(&text).ok_or_else(|| serde::de::Error::custom("invalid fingerprint"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyed_fingerprints_depend_on_the_key_and_never_print_it() {
        let a = SyncKey::generate().unwrap();
        let b = SyncKey::generate().unwrap();
        assert_eq!(a.fingerprint(b"secret"), a.fingerprint(b"secret"));
        assert_ne!(a.fingerprint(b"secret"), b.fingerprint(b"secret"));
        assert_ne!(a.fingerprint(b"secret"), a.fingerprint(b"other"));
        let mut hasher = a.hasher();
        hasher.update(b"sec");
        hasher.update(b"ret");
        assert_eq!(hasher.finish(), a.fingerprint(b"secret"));
        assert_eq!(format!("{a:?}"), "SyncKey(<redacted>)");
        let round: SyncKey = serde_json::from_str(&serde_json::to_string(&a).unwrap()).unwrap();
        assert_eq!(round, a);
        let fingerprint = a.fingerprint(b"x");
        assert_eq!(Fingerprint::parse(&fingerprint.hex()), Some(fingerprint));
        assert_eq!(fingerprint.short().len(), 16);
        assert!(Fingerprint::parse("zz").is_none());
    }
}
