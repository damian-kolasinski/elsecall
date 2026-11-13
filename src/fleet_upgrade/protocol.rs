//! The upgrade channel between the master and a member: a sealed channel (see
//! `secure_channel`) the master opens on `ROUTE`.
//!
//! 1. The master sends `Offer` with a manifest it signs with its member key: the
//!    upgrade's ID, the recipient, the content's SHA-256 digest and size, the
//!    platform and architectures it runs on, and how long the member may wait for
//!    its work to end.
//! 2. The member answers `Accepted`, `AlreadyCurrent` or `Refused`.
//! 3. After `Accepted` the master sends the content as one blob. The member checks
//!    its size, digest and executable header, stages it, and answers `Staged` or
//!    `Refused`. Nothing is installed before `Staged`, and the channel then closes:
//!    the master follows the rest on `STATUS_ROUTE`, which survives the restart.
//!
//! This wire format and the status route are frozen (`elsecall-upgrade/1`): every
//! later build must still accept them, since they are how members on older builds
//! are brought forward.
use crate::{
    build_identity::{Build, Digest},
    executable_format::Architecture,
    failure::{Failure, Result},
    member_key::{MemberKey, PublicKey},
    wire::{self, Frame},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

pub const ROUTE: &str = "/peer/v1/upgrade/attach";
pub const STATUS_ROUTE: &str = "/peer/v1/upgrade/status";
/// The connection upgrade a member answers on `ROUTE`.
pub const PROTOCOL: &str = "elsecall-upgrade/1";
/// What both members sign the channel for; never a run ID.
pub const PURPOSE: &str = "fleet-upgrade";
/// The largest executable a member accepts. Release builds are about 15 MiB.
pub const MAXIMUM_EXECUTABLE: u64 = 128 << 20;
/// The longest a member waits for its runs and leases to end.
pub const MAXIMUM_WAIT_SECONDS: u64 = 60 * 60;
/// How old a manifest may be when it arrives.
pub const MANIFEST_WINDOW_SECONDS: u64 = 10 * 60;
const MAXIMUM_MESSAGE: usize = 64 * 1024;

/// One upgrade attempt's ID, such as `u-3f9a0c41d2b7e5a1`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct UpgradeId(String);
impl UpgradeId {
    pub fn generate() -> Option<Self> {
        let mut bytes = [0u8; 8];
        getrandom::fill(&mut bytes).ok()?;
        Some(Self(format!(
            "u-{}",
            bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
        )))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl<'de> Deserialize<'de> for UpgradeId {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        let valid = text.len() == 18
            && text.starts_with("u-")
            && text[2..]
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if valid {
            Ok(Self(text))
        } else {
            Err(serde::de::Error::custom("invalid upgrade ID"))
        }
    }
}
impl std::fmt::Display for UpgradeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The operating systems builds are distributed for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Os {
    Macos,
}
impl Os {
    /// The OS this process was built for.
    pub fn this() -> Option<Self> {
        (std::env::consts::OS == "macos").then_some(Self::Macos)
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Macos => "macos",
        }
    }
}

/// What the master states about the build it sends, and signs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub id: UpgradeId,
    /// The member this upgrade is for; a manifest is never valid for another.
    pub recipient: PublicKey,
    pub digest: Digest,
    pub size: u64,
    pub os: Os,
    pub architectures: Vec<Architecture>,
    /// How long the member waits for its runs and leases before deferring.
    pub wait_seconds: u64,
    /// When the master issued it, in seconds since the Unix epoch.
    pub issued_at: u64,
}
impl Manifest {
    fn transcript(&self, master: &PublicKey) -> Vec<u8> {
        let architectures = self
            .architectures
            .iter()
            .map(|a| a.name())
            .collect::<Vec<_>>()
            .join(",");
        crate::peer_auth::transcript(
            "elsecall/upgrade-manifest/v1",
            &[
                master.as_bytes(),
                self.recipient.as_bytes(),
                self.id.as_str().as_bytes(),
                self.digest.as_bytes(),
                &self.size.to_be_bytes(),
                self.os.name().as_bytes(),
                architectures.as_bytes(),
                &self.wait_seconds.to_be_bytes(),
                &self.issued_at.to_be_bytes(),
            ],
        )
    }

    pub fn sign(self, master: &MemberKey) -> SignedManifest {
        let signature = URL_SAFE_NO_PAD.encode(master.sign(&self.transcript(&master.public())));
        SignedManifest {
            manifest: self,
            signature,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedManifest {
    pub manifest: Manifest,
    pub signature: String,
}
impl SignedManifest {
    /// Whether `master` signed exactly this manifest.
    pub fn verify(&self, master: &PublicKey) -> bool {
        URL_SAFE_NO_PAD
            .decode(&self.signature)
            .is_ok_and(|signature| master.verify(&self.manifest.transcript(master), &signature))
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Message {
    Offer {
        manifest: SignedManifest,
    },
    Accepted,
    AlreadyCurrent {
        running: Build,
    },
    /// The content is verified and staged; `running` is what the member runs now.
    Staged {
        running: Build,
    },
    Refused {
        failure: Failure,
    },
}

pub fn protocol_error() -> Failure {
    Failure::new(
        "protocol_error",
        "The upgrade channel carried an unexpected or invalid message.",
        "Run elsecall fleet upgrade again; check both members' service logs if it repeats.",
    )
}

pub fn lost() -> Failure {
    Failure::new(
        "connection_lost",
        "The upgrade channel closed before the build was staged; nothing changed on the member.",
        "Run elsecall fleet upgrade again once the member is reachable.",
    )
}

/// Sends `bytes` as content frames ended by an empty one.
pub async fn send_blob<W: AsyncWrite + Unpin>(writer: &mut W, bytes: &[u8]) -> Result<()> {
    let written = async {
        for chunk in bytes.chunks(wire::CHUNK) {
            wire::write(writer, &Frame::Content(chunk.to_vec())).await?;
        }
        wire::write(writer, &Frame::Content(Vec::new())).await?;
        writer.flush().await
    };
    written.await.map_err(|_| lost())
}

/// Reads one blob of at most `limit` bytes.
pub async fn receive_blob<R: AsyncRead + Unpin>(reader: &mut R, limit: u64) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    loop {
        match wire::read(reader).await {
            Ok(Some(Frame::Content(chunk))) if chunk.is_empty() => return Ok(bytes),
            Ok(Some(Frame::Content(chunk))) => {
                if (bytes.len() + chunk.len()) as u64 > limit {
                    return Err(Failure::new(
                        "digest_mismatch",
                        "The build is larger than its signed manifest says.",
                        "Run elsecall fleet upgrade again; the content was altered in transit or on the master.",
                    ));
                }
                bytes.extend_from_slice(&chunk);
            }
            Ok(Some(Frame::Heartbeat)) => (),
            Ok(Some(_)) => return Err(protocol_error()),
            Ok(None) | Err(_) => return Err(lost()),
        }
    }
}

pub async fn send<W: AsyncWrite + Unpin>(writer: &mut W, message: &Message) -> Result<()> {
    send_blob(
        writer,
        &serde_json::to_vec(message).expect("messages serialize"),
    )
    .await
}

pub async fn receive<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Message> {
    let bytes = receive_blob(reader, MAXIMUM_MESSAGE as u64)
        .await
        .map_err(|error| {
            if error.code == "digest_mismatch" {
                protocol_error()
            } else {
                error
            }
        })?;
    serde_json::from_slice(&bytes).map_err(|_| protocol_error())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(recipient: PublicKey) -> Manifest {
        Manifest {
            id: UpgradeId::generate().unwrap(),
            recipient,
            digest: Digest::of(b"build"),
            size: 5,
            os: Os::Macos,
            architectures: vec![Architecture::Aarch64],
            wait_seconds: 300,
            issued_at: 1_800_000_000,
        }
    }

    #[test]
    fn only_the_signer_verifies_and_every_field_is_signed() {
        let master = MemberKey::generate().unwrap();
        let member = MemberKey::generate().unwrap();
        let signed = manifest(member.public()).sign(&master);
        assert!(signed.verify(&master.public()));
        assert!(!signed.verify(&member.public()));
        type Change = Box<dyn Fn(&mut Manifest)>;
        let changes: Vec<Change> = vec![
            Box::new(|m| m.digest = Digest::of(b"other")),
            Box::new(|m| m.size += 1),
            Box::new(|m| m.recipient = MemberKey::generate().unwrap().public()),
            Box::new(|m| m.architectures = vec![Architecture::X86_64]),
            Box::new(|m| m.architectures.push(Architecture::Aarch64)),
            Box::new(|m| m.wait_seconds = 0),
            Box::new(|m| m.issued_at += 1),
            Box::new(|m| m.id = UpgradeId::generate().unwrap()),
        ];
        for change in changes {
            let mut altered = signed.clone();
            change(&mut altered.manifest);
            assert!(!altered.verify(&master.public()), "{altered:?}");
        }
    }

    #[test]
    fn upgrade_ids_are_validated() {
        let id = UpgradeId::generate().unwrap();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(serde_json::from_str::<UpgradeId>(&json).unwrap(), id);
        for bad in ["\"u-\"", "\"x-0123456789abcdef\"", "\"u-0123456789ABCDEF\""] {
            assert!(serde_json::from_str::<UpgradeId>(bad).is_err(), "{bad}");
        }
    }

    #[tokio::test]
    async fn blobs_larger_than_their_limit_are_refused() {
        let (mut a, mut b) = tokio::io::duplex(1 << 20);
        let writer = tokio::spawn(async move { send_blob(&mut a, &[1u8; 100]).await });
        let error = receive_blob(&mut b, 99).await.unwrap_err();
        assert_eq!(error.code, "digest_mismatch");
        let _ = writer.await;
    }
}
