//! This member's signing key and the public keys that identify members.
//!
//! The private key lives only in an owner-only file in Elsecall's state directory and
//! in the service process. Neither type prints key material through `Debug`.
use crate::{
    failure::{Failure, Result},
    local_access::user_id,
    private_files,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
};
use zeroize::Zeroizing;

const HEADER: &str = "elsecall-member-key-v1\n";
pub const SIGNATURE_LENGTH: usize = 64;

/// A member's public key: the only thing that identifies a member to another.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PublicKey([u8; 32]);
impl PublicKey {
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
    /// Verifies a signature with strict Ed25519 rules (no malleable or weak-key forms).
    pub fn verify(&self, message: &[u8], signature: &[u8]) -> bool {
        let Ok(signature) = <[u8; SIGNATURE_LENGTH]>::try_from(signature) else {
            return false;
        };
        let Ok(key) = VerifyingKey::from_bytes(&self.0) else {
            return false;
        };
        !key.is_weak()
            && key
                .verify_strict(message, &Signature::from_bytes(&signature))
                .is_ok()
    }
}
impl std::str::FromStr for PublicKey {
    type Err = Failure;
    fn from_str(text: &str) -> Result<Self> {
        let invalid = || {
            Failure::new(
                "invalid_member_key",
                "A member key is not a valid Ed25519 public key.",
                "Copy the member code again from elsecall fleet identity.",
            )
        };
        let bytes: [u8; 32] = URL_SAFE_NO_PAD
            .decode(text)
            .map_err(|_| invalid())?
            .try_into()
            .map_err(|_| invalid())?;
        match VerifyingKey::from_bytes(&bytes) {
            Ok(key) if !key.is_weak() => Ok(Self(bytes)),
            _ => Err(invalid()),
        }
    }
}
impl std::fmt::Display for PublicKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&URL_SAFE_NO_PAD.encode(self.0))
    }
}
impl std::fmt::Debug for PublicKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PublicKey({self})")
    }
}
impl Serialize for PublicKey {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}
impl<'de> Deserialize<'de> for PublicKey {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse()
            .map_err(|_| serde::de::Error::custom("invalid member key"))
    }
}

/// This member's private signing key.
pub struct MemberKey(SigningKey);
impl std::fmt::Debug for MemberKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "MemberKey(public: {})", self.public())
    }
}
impl MemberKey {
    pub fn generate() -> Result<Self> {
        let mut seed = Zeroizing::new([0u8; 32]);
        getrandom::fill(seed.as_mut()).map_err(|_| {
            Failure::new(
                "random_unavailable",
                "The operating system's random number generator failed.",
                "Retry; restart the machine if it keeps failing.",
            )
        })?;
        Ok(Self(SigningKey::from_bytes(&seed)))
    }
    pub fn public(&self) -> PublicKey {
        PublicKey(self.0.verifying_key().to_bytes())
    }
    pub fn sign(&self, message: &[u8]) -> [u8; SIGNATURE_LENGTH] {
        self.0.sign(message).to_bytes()
    }

    /// Loads the key, creating it on first use. Only the single service instance calls
    /// this, under its instance lock, so creation cannot race.
    pub fn load_or_create(path: &Path) -> Result<Self> {
        match fs::symlink_metadata(path) {
            Ok(_) => Self::load(path),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let key = Self::generate()?;
                let text = Zeroizing::new(format!(
                    "{HEADER}{}\n",
                    URL_SAFE_NO_PAD.encode(key.0.to_bytes())
                ));
                private_files::write(path, text.as_bytes(), 0o600)?;
                Ok(key)
            }
            Err(e) => Err(Failure::io("Inspect member key", e)),
        }
    }

    fn load(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .map_err(|e| Failure::io("Open member key", e))?;
        let meta = file
            .metadata()
            .map_err(|e| Failure::io("Inspect member key", e))?;
        if !meta.is_file() || meta.uid() != user_id() || meta.mode() & 0o077 != 0 {
            return Err(Failure::new(
                "unsafe_member_key",
                "The member key must be a regular file owned by you with mode 0600.",
                "Inspect member.key in the Elsecall state directory and correct its permissions.",
            ));
        }
        let mut text = Zeroizing::new(String::new());
        file.take(256)
            .read_to_string(&mut text)
            .map_err(|e| Failure::io("Read member key", e))?;
        let invalid = || {
            Failure::new(
                "invalid_member_key_file",
                "The member key file is not recognized.",
                "Move member.key and fleet.json out of the Elsecall state directory, run elsecall service restart to create a new key, then enrol this member again.",
            )
        };
        let encoded = text
            .strip_prefix(HEADER)
            .and_then(|rest| rest.strip_suffix('\n'))
            .ok_or_else(invalid)?;
        let seed = Zeroizing::new(URL_SAFE_NO_PAD.decode(encoded).map_err(|_| invalid())?);
        let seed: &[u8; 32] = seed.as_slice().try_into().map_err(|_| invalid())?;
        Ok(Self(SigningKey::from_bytes(seed)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn signatures_verify_only_for_the_signed_message_and_key() {
        let key = MemberKey::generate().unwrap();
        let other = MemberKey::generate().unwrap();
        let signature = key.sign(b"message");
        assert!(key.public().verify(b"message", &signature));
        assert!(!key.public().verify(b"messagf", &signature));
        assert!(!other.public().verify(b"message", &signature));
        assert!(!key.public().verify(b"message", &signature[..63]));
    }

    #[test]
    fn public_keys_round_trip_and_reject_malformed_text() {
        let key = MemberKey::generate().unwrap().public();
        assert_eq!(key.to_string().parse::<PublicKey>().unwrap(), key);
        assert!("not a key".parse::<PublicKey>().is_err());
        assert!(
            URL_SAFE_NO_PAD
                .encode([0u8; 31])
                .parse::<PublicKey>()
                .is_err()
        );
        // The identity point is a weak key and must never identify a member.
        let mut identity = [0u8; 32];
        identity[0] = 1;
        assert!(
            URL_SAFE_NO_PAD
                .encode(identity)
                .parse::<PublicKey>()
                .is_err()
        );
    }

    #[test]
    fn key_file_is_owner_only_stable_and_never_debug_printed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("member.key");
        let created = MemberKey::load_or_create(&path).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let loaded = MemberKey::load_or_create(&path).unwrap();
        assert_eq!(created.public(), loaded.public());
        let secret = URL_SAFE_NO_PAD.encode(created.0.to_bytes());
        assert!(!format!("{created:?}").contains(&secret));
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            MemberKey::load_or_create(&path).unwrap_err().code,
            "unsafe_member_key"
        );
    }
}
