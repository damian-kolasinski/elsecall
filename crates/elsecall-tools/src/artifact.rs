//! Copying an installed tool from one member to another over the members'
//! channel: the master's mise executable for a member without mise, and later
//! an installed Xcode bundle.
//!
//! The member that needs it opens a sealed channel to the member that has it
//! (`POST /peer/v1/tools/artifact/<artifact>`, upgraded to
//! `elsecall-artifact/1`); both sign the channel for the artifact it carries, so
//! a channel opened for one cannot carry another. The source answers with the
//! artifact's bytes as content frames, an empty one, then a message with their
//! count and SHA-256; the receiver writes them as they arrive and keeps them
//! only when both match, so a cut connection never leaves a partial tool
//! behind. A tree travels as `ditto` archives it (`ditto -c` on the source,
//! `ditto -x` on the receiver), keeping links, modes and extended attributes.
//! Any enrolled member may ask: what travels is installed software, not a
//! secret.
use elsecall_contract::wire::{self, CHUNK, Frame};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub(crate) const PROTOCOL: &str = "elsecall-artifact/1";

/// What one member copies from another.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Artifact {
    /// The mise executable, one file.
    Mise,
}

impl Artifact {
    /// The route that serves it.
    pub(crate) fn path(&self) -> String {
        match self {
            Self::Mise => "/peer/v1/tools/artifact/mise".into(),
        }
    }

    /// What both members sign the channel for.
    pub(crate) fn purpose(&self) -> String {
        match self {
            Self::Mise => "tools-artifact:mise".into(),
        }
    }
}

/// The trailer after the content: how many bytes it held, and their digest.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Trailer {
    bytes: u64,
    sha256: String,
}

/// Sends everything `source` reads as the artifact's content, then its trailer.
#[cfg(test)]
pub(crate) async fn send<R, W>(source: &mut R, channel: &mut W) -> io::Result<u64>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    send_content(source, channel).await?.finish(channel).await
}

/// Content sent and not yet vouched for: the receiver keeps nothing until
/// [`Sent::finish`] sends the trailer, so a source that turns out to have
/// failed is dropped instead, and the copy is cut.
pub(crate) struct Sent {
    bytes: u64,
    digest: Sha256,
}

impl Sent {
    pub(crate) async fn finish<W: AsyncWrite + Unpin>(self, channel: &mut W) -> io::Result<u64> {
        let trailer = Trailer {
            bytes: self.bytes,
            sha256: hex(&self.digest.finalize()),
        };
        wire::write(
            channel,
            &Frame::Message(serde_json::to_vec(&trailer).expect("a trailer serializes")),
        )
        .await?;
        channel.flush().await?;
        Ok(self.bytes)
    }
}

/// Sends everything `source` reads as content frames, then the empty one.
pub(crate) async fn send_content<R, W>(source: &mut R, channel: &mut W) -> io::Result<Sent>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut digest = Sha256::new();
    let mut bytes = 0u64;
    let mut buffer = vec![0u8; CHUNK];
    loop {
        let read = source.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
        bytes += read as u64;
        wire::write(channel, &Frame::Content(buffer[..read].to_vec())).await?;
    }
    wire::write(channel, &Frame::Content(Vec::new())).await?;
    Ok(Sent { bytes, digest })
}

/// Writes the artifact's content into `sink` as it arrives, at most `limit`
/// bytes. Ok only when the whole artifact arrived intact; the caller then keeps
/// what `sink` wrote.
pub(crate) async fn receive<R, W>(channel: &mut R, sink: &mut W, limit: u64) -> io::Result<u64>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + ?Sized,
{
    let mut digest = Sha256::new();
    let mut bytes = 0u64;
    loop {
        match wire::read(channel).await? {
            Some(Frame::Content(chunk)) if chunk.is_empty() => break,
            Some(Frame::Content(chunk)) => {
                digest.update(&chunk);
                bytes += chunk.len() as u64;
                if bytes > limit {
                    return Err(cut());
                }
                sink.write_all(&chunk).await?;
            }
            Some(Frame::Heartbeat) => {}
            _ => return Err(cut()),
        }
    }
    sink.flush().await?;
    let trailer = match wire::read(channel).await? {
        Some(Frame::Message(message)) => {
            serde_json::from_slice::<Trailer>(&message).map_err(|_| cut())?
        }
        _ => return Err(cut()),
    };
    if trailer.bytes != bytes || trailer.sha256 != hex(&digest.finalize()) {
        return Err(cut());
    }
    Ok(bytes)
}

fn cut() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "the artifact did not arrive whole",
    )
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use crate::artifact::*;

    #[tokio::test]
    async fn an_artifact_arrives_whole_or_not_at_all() {
        let content: Vec<u8> = (0..(CHUNK * 2 + 17)).map(|i| (i % 251) as u8).collect();
        let (mut near, mut far) = tokio::io::duplex(64 * 1024);
        let sent = content.clone();
        let sender = tokio::spawn(async move {
            let mut source = sent.as_slice();
            send(&mut source, &mut near).await.unwrap()
        });
        let mut received = Vec::new();
        assert_eq!(
            receive(&mut far, &mut received, u64::MAX).await.unwrap(),
            content.len() as u64
        );
        assert_eq!(received, content);
        sender.await.unwrap();

        let (mut near, mut far) = tokio::io::duplex(64 * 1024);
        tokio::spawn(async move {
            wire::write(&mut near, &Frame::Content(vec![1, 2, 3]))
                .await
                .unwrap();
        });
        let mut partial = Vec::new();
        assert!(
            receive(&mut far, &mut partial, u64::MAX).await.is_err(),
            "a cut stream"
        );

        let (mut near, mut far) = tokio::io::duplex(64 * 1024);
        tokio::spawn(async move {
            wire::write(&mut near, &Frame::Content(vec![1, 2, 3]))
                .await
                .unwrap();
            wire::write(&mut near, &Frame::Content(Vec::new()))
                .await
                .unwrap();
            let trailer = Trailer {
                bytes: 3,
                sha256: "0".repeat(64),
            };
            wire::write(
                &mut near,
                &Frame::Message(serde_json::to_vec(&trailer).unwrap()),
            )
            .await
            .unwrap();
        });
        let mut wrong = Vec::new();
        assert!(
            receive(&mut far, &mut wrong, u64::MAX).await.is_err(),
            "content that does not match its digest"
        );
    }
}
