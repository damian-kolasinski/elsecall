//! The sync channel between the master and a member: a sealed channel (see
//! `secure_channel`) the master opens, on which it offers a manifest.
//!
//! - To plan, the member answers `Planned` with how the offer would change it.
//! - To push, the member answers `Need` with the content it lacks, the master sends
//!   each as a blob, then `Commit`; the member applies and answers `Applied`.
//!
//! Either side may answer `Failed`. Nothing changes on the member before `Commit`,
//! so a channel lost earlier leaves its previous state in place. Every message and
//! blob travels as a sequence of content frames ended by an empty one, so large
//! manifests need no special case.
use crate::{
    config_sync::{
        fingerprint::Fingerprint,
        manifest::Manifest,
        report::{Applied, PathReport},
    },
    failure::{Failure, Result},
    wire::{self, Frame},
};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use zeroize::Zeroizing;

/// The upgrade a member answers on the sync route.
pub const PROTOCOL: &str = "elsecall-sync/1";
/// What both members sign the channel for; never a run ID.
pub const PURPOSE: &str = "config-sync";
pub const ROUTE: &str = "/peer/v1/config-sync/attach";
/// The largest message: a manifest or plan at the limits fits well within it.
const MAXIMUM_MESSAGE: usize = 16 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Purpose {
    Plan,
    ServicePlan,
    Push,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Message {
    Offer {
        purpose: Purpose,
        manifest: Manifest,
    },
    Planned {
        paths: Vec<PathReport>,
    },
    ServicePlanned {
        plan: crate::service_configuration::MemberPlan,
    },
    Need {
        fingerprints: Vec<Fingerprint>,
    },
    Commit,
    Applied {
        applied: Applied,
    },
    Failed {
        failure: Failure,
    },
}

pub fn protocol_error() -> Failure {
    Failure::new(
        "protocol_error",
        "The sync channel carried an unexpected or invalid message.",
        "Install the same Elsecall version on every member.",
    )
}

fn lost() -> Failure {
    Failure::new(
        "connection_lost",
        "The sync channel closed before the exchange finished; nothing changed on the member.",
        "Run elsecall sync push once the member is reachable.",
    )
}

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
pub async fn receive_blob<R: AsyncRead + Unpin>(
    reader: &mut R,
    limit: usize,
) -> Result<Zeroizing<Vec<u8>>> {
    let mut bytes = Zeroizing::new(Vec::new());
    loop {
        match wire::read(reader).await {
            Ok(Some(Frame::Content(chunk))) if chunk.is_empty() => return Ok(bytes),
            Ok(Some(Frame::Content(chunk))) => {
                if bytes.len() + chunk.len() > limit {
                    return Err(protocol_error());
                }
                bytes.extend_from_slice(&chunk);
                drop(Zeroizing::new(chunk));
            }
            Ok(Some(Frame::Heartbeat)) => (),
            Ok(Some(_)) => return Err(protocol_error()),
            Ok(None) | Err(_) => return Err(lost()),
        }
    }
}

pub async fn send<W: AsyncWrite + Unpin>(writer: &mut W, message: &Message) -> Result<()> {
    // An offer carries the fleet key: keep its encoding out of freed memory.
    let bytes = Zeroizing::new(serde_json::to_vec(message).expect("messages serialize"));
    if bytes.len() > MAXIMUM_MESSAGE {
        return Err(Failure::new(
            "sync_set_invalid",
            "The synced set is too large to send.",
            "Disable a path or sync a smaller one.",
        ));
    }
    send_blob(writer, &bytes).await
}

pub async fn receive<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Message> {
    let bytes = receive_blob(reader, MAXIMUM_MESSAGE).await?;
    // Untrusted input: deserializing validates the manifest and every report.
    serde_json::from_slice(&bytes).map_err(|_| protocol_error())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn messages_and_blobs_round_trip_within_limits() {
        let (mut a, mut b) = tokio::io::duplex(1 << 16);
        let big = vec![7u8; wire::CHUNK * 2 + 3];
        let sent = big.clone();
        let writer = tokio::spawn(async move {
            send(&mut a, &Message::Commit).await.unwrap();
            send_blob(&mut a, &sent).await.unwrap();
            let _ = send_blob(&mut a, &sent).await;
        });
        assert!(matches!(receive(&mut b).await.unwrap(), Message::Commit));
        assert_eq!(&*receive_blob(&mut b, big.len()).await.unwrap(), &big[..]);
        assert_eq!(
            receive_blob(&mut b, big.len() - 1).await.unwrap_err().code,
            "protocol_error"
        );
        drop(b);
        writer.await.unwrap();
        let (a, mut b) = tokio::io::duplex(64);
        drop(a);
        assert_eq!(receive(&mut b).await.unwrap_err().code, "connection_lost");
    }
}
