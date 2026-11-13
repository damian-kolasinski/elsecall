//! An authenticated, encrypted byte stream between two members, for the traffic of a
//! placed run. Tailscale encrypts the network but does not establish identity, so
//! the run's stream is keyed by an ephemeral X25519 exchange whose public halves
//! both members sign with their enrolled keys during the attach request. Every
//! record is sealed with ChaCha20-Poly1305 under a per-direction key and a counter
//! nonce, so nothing can be injected, altered, reordered or replayed.
use chacha20poly1305::{
    ChaCha20Poly1305, KeyInit, Nonce,
    aead::{Aead, Payload},
};
use hkdf::Hkdf;
use sha2::Sha256;
use std::{
    io,
    pin::Pin,
    task::{Context, Poll, ready},
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use x25519_dalek::{PublicKey, StaticSecret};
use zeroize::Zeroizing;

/// The most plaintext one record carries.
const RECORD: usize = 64 * 1024;
const TAG: usize = 16;

/// Validate an untrusted public half before consuming a remote admission or
/// recording a successful handshake. The fixed probe carries no secret material.
pub(crate) fn is_contributory(public: &[u8; 32]) -> bool {
    StaticSecret::from([42; 32])
        .diffie_hellman(&PublicKey::from(*public))
        .was_contributory()
}

/// One side's half of the key exchange.
pub struct Ephemeral {
    secret: StaticSecret,
    pub public: [u8; 32],
}
impl Ephemeral {
    pub fn generate() -> Option<Self> {
        let mut bytes = Zeroizing::new([0u8; 32]);
        getrandom::fill(bytes.as_mut()).ok()?;
        let secret = StaticSecret::from(*bytes);
        let public = PublicKey::from(&secret).to_bytes();
        Some(Self { secret, public })
    }

    /// Derives the stream's keys. `context` binds them to both members' identity
    /// keys and the request, and must be the same on both sides. `None` when the
    /// peer's half is degenerate.
    pub fn keys(self, peer: &[u8; 32], context: &[u8], initiator: bool) -> Option<Keys> {
        let shared = self.secret.diffie_hellman(&PublicKey::from(*peer));
        if !shared.was_contributory() {
            return None;
        }
        let (initiator_public, responder_public) = if initiator {
            (self.public, *peer)
        } else {
            (*peer, self.public)
        };
        let mut info = b"elsecall/channel/v1".to_vec();
        info.extend_from_slice(&initiator_public);
        info.extend_from_slice(&responder_public);
        info.extend_from_slice(context);
        let hkdf = Hkdf::<Sha256>::new(None, shared.as_bytes());
        let mut forward = Zeroizing::new([0u8; 32]);
        let mut backward = Zeroizing::new([0u8; 32]);
        let with = |label: &[u8], out: &mut [u8; 32]| {
            let mut full = info.clone();
            full.extend_from_slice(label);
            hkdf.expand(&full, out).expect("32 bytes is a valid length");
        };
        with(b"/initiator", &mut forward);
        with(b"/responder", &mut backward);
        let (send, receive) = if initiator {
            (forward, backward)
        } else {
            (backward, forward)
        };
        Some(Keys {
            send: ChaCha20Poly1305::new((&*send).into()),
            receive: ChaCha20Poly1305::new((&*receive).into()),
        })
    }
}

pub struct Keys {
    send: ChaCha20Poly1305,
    receive: ChaCha20Poly1305,
}

fn nonce(counter: u64) -> Nonce {
    let mut bytes = [0u8; 12];
    bytes[4..].copy_from_slice(&counter.to_be_bytes());
    bytes.into()
}

#[derive(Debug)]
struct AuthenticationFailure;
impl std::fmt::Display for AuthenticationFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the secure channel did not authenticate")
    }
}
impl std::error::Error for AuthenticationFailure {}

/// Record failures carry their origin through I/O adapters. Decoded run-frame
/// failures also use InvalidData, but must remain protocol diagnostics.
pub fn is_authentication_failure(error: &io::Error) -> bool {
    error
        .get_ref()
        .is_some_and(|source| source.is::<AuthenticationFailure>())
}
fn broken() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, AuthenticationFailure)
}

/// A stream whose every byte is sealed. Reading fails at the first record that
/// does not authenticate.
pub struct SecureStream<S> {
    inner: S,
    keys: Keys,
    sent: u64,
    received: u64,
    /// A sealed record waiting to be written, and how much of it was.
    outgoing: Vec<u8>,
    outgoing_written: usize,
    /// Bytes of the record being read.
    incoming: Vec<u8>,
    plaintext: Vec<u8>,
    plaintext_read: usize,
}

impl<S> SecureStream<S> {
    pub fn new(inner: S, keys: Keys) -> Self {
        Self {
            inner,
            keys,
            sent: 0,
            received: 0,
            outgoing: Vec::new(),
            outgoing_written: 0,
            incoming: Vec::new(),
            plaintext: Vec::new(),
            plaintext_read: 0,
        }
    }
}

impl<S: AsyncWrite + Unpin> SecureStream<S> {
    fn drain(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        while self.outgoing_written < self.outgoing.len() {
            let n = ready!(
                Pin::new(&mut self.inner).poll_write(cx, &self.outgoing[self.outgoing_written..])
            )?;
            if n == 0 {
                return Poll::Ready(Err(io::ErrorKind::WriteZero.into()));
            }
            self.outgoing_written += n;
        }
        self.outgoing.clear();
        self.outgoing_written = 0;
        Poll::Ready(Ok(()))
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for SecureStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        ready!(this.drain(cx))?;
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        let take = buf.len().min(RECORD);
        let header = (take as u32).to_be_bytes();
        let sealed = this
            .keys
            .send
            .encrypt(
                &nonce(this.sent),
                Payload {
                    msg: &buf[..take],
                    aad: &header,
                },
            )
            .map_err(|_| broken())?;
        this.sent += 1;
        this.outgoing.extend_from_slice(&header);
        this.outgoing.extend_from_slice(&sealed);
        // The record is committed; it is written by this or a later call.
        let _ = this.drain(cx)?;
        Poll::Ready(Ok(take))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        ready!(this.drain(cx))?;
        Pin::new(&mut this.inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        ready!(this.drain(cx))?;
        Pin::new(&mut this.inner).poll_shutdown(cx)
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for SecureStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        loop {
            if this.plaintext_read < this.plaintext.len() {
                let available = &this.plaintext[this.plaintext_read..];
                let n = available.len().min(buf.remaining());
                buf.put_slice(&available[..n]);
                this.plaintext_read += n;
                return Poll::Ready(Ok(()));
            }
            // Read the 4-byte length, then the sealed record.
            let needed = if this.incoming.len() < 4 {
                4
            } else {
                let length =
                    u32::from_be_bytes(this.incoming[..4].try_into().expect("4 bytes")) as usize;
                if length == 0 || length > RECORD {
                    return Poll::Ready(Err(broken()));
                }
                4 + length + TAG
            };
            if this.incoming.len() == needed && needed > 4 {
                let header: [u8; 4] = this.incoming[..4].try_into().expect("4 bytes");
                let opened = this
                    .keys
                    .receive
                    .decrypt(
                        &nonce(this.received),
                        Payload {
                            msg: &this.incoming[4..],
                            aad: &header,
                        },
                    )
                    .map_err(|_| broken())?;
                this.received += 1;
                this.incoming.clear();
                this.plaintext = opened;
                this.plaintext_read = 0;
                continue;
            }
            let mut chunk = vec![0u8; needed - this.incoming.len()];
            let mut chunk_buf = ReadBuf::new(&mut chunk);
            ready!(Pin::new(&mut this.inner).poll_read(cx, &mut chunk_buf))?;
            let filled = chunk_buf.filled().len();
            if filled == 0 {
                // End of stream: clean only between records.
                return if this.incoming.is_empty() {
                    Poll::Ready(Ok(()))
                } else {
                    Poll::Ready(Err(io::ErrorKind::UnexpectedEof.into()))
                };
            }
            this.incoming.extend_from_slice(&chunk[..filled]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn pair() -> (Keys, Keys) {
        let a = Ephemeral::generate().unwrap();
        let b = Ephemeral::generate().unwrap();
        let (a_public, b_public) = (a.public, b.public);
        (
            a.keys(&b_public, b"context", true).unwrap(),
            b.keys(&a_public, b"context", false).unwrap(),
        )
    }

    #[tokio::test]
    async fn no_plaintext_byte_is_readable_before_the_complete_record_tag() {
        let (send, receive) = pair();
        let payload = vec![7; RECORD];
        let mut sealed = SecureStream::new(Vec::new(), send);
        sealed.write_all(&payload).await.unwrap();
        sealed.flush().await.unwrap();
        let bytes = sealed.inner;
        let (mut writer, reader) = tokio::io::duplex(bytes.len());
        writer.write_all(&bytes[..bytes.len() - TAG]).await.unwrap();
        let mut opened = SecureStream::new(reader, receive);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), opened.read_u8())
                .await
                .is_err()
        );
        writer.write_all(&bytes[bytes.len() - TAG..]).await.unwrap();
        let mut received = vec![0; RECORD];
        opened.read_exact(&mut received).await.unwrap();
        assert_eq!(received, payload);
    }

    #[tokio::test]
    async fn carries_bytes_both_ways() {
        let (a_keys, b_keys) = pair();
        let (a, b) = tokio::io::duplex(4096);
        let mut a = SecureStream::new(a, a_keys);
        let mut b = SecureStream::new(b, b_keys);
        let data: Vec<u8> = (0..300_000u32).map(|i| i as u8).collect();
        let sent = data.clone();
        let writer = tokio::spawn(async move {
            a.write_all(&sent).await.unwrap();
            a.flush().await.unwrap();
            let mut back = [0u8; 5];
            a.read_exact(&mut back).await.unwrap();
            back
        });
        let mut received = vec![0u8; data.len()];
        b.read_exact(&mut received).await.unwrap();
        assert_eq!(received, data);
        b.write_all(b"reply").await.unwrap();
        b.flush().await.unwrap();
        assert_eq!(&writer.await.unwrap(), b"reply");
    }

    #[tokio::test]
    async fn refuses_tampering_and_mismatched_keys() {
        let (a_keys, _) = pair();
        let (mut wire_a, wire_b) = tokio::io::duplex(4096);
        let mut a = SecureStream::new(Vec::new(), a_keys);
        a.write_all(b"hello").await.unwrap();
        let mut sealed = a.inner.clone();
        sealed[6] ^= 1;
        wire_a.write_all(&sealed).await.unwrap();
        let (_, b_keys) = pair();
        let mut b = SecureStream::new(wire_b, b_keys);
        let mut out = [0u8; 5];
        assert!(b.read_exact(&mut out).await.is_err());

        // A peer with other keys cannot read either.
        let (x, _) = pair();
        let (_, y) = pair();
        let (left, right) = tokio::io::duplex(4096);
        let mut left = SecureStream::new(left, x);
        let mut right = SecureStream::new(right, y);
        left.write_all(b"secret").await.unwrap();
        let mut out = [0u8; 6];
        assert!(right.read_exact(&mut out).await.is_err());
    }
}
