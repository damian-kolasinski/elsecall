//! Authentication of member-to-member messages.
//!
//! Every request is signed by the sender's member key over the recipient's key, a
//! timestamp, a random nonce, the method, the path and the body. Every response is
//! signed by the responder over the request's nonce, the status and the body, so a
//! response cannot be forged or replayed onto another request. Signed inputs are
//! length-prefixed and domain-separated, so no two distinct messages share a transcript.
use crate::member_key::{MemberKey, PublicKey, SIGNATURE_LENGTH};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use std::{
    collections::HashMap,
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

pub const MEMBER_HEADER: &str = "x-elsecall-member";
pub const TIMESTAMP_HEADER: &str = "x-elsecall-timestamp";
pub const NONCE_HEADER: &str = "x-elsecall-nonce";
pub const SIGNATURE_HEADER: &str = "x-elsecall-signature";

/// Accepted clock difference between members, in seconds, in either direction.
pub const WINDOW_SECONDS: u64 = 60;
const MAX_REMEMBERED_NONCES: usize = 100_000;

pub type Nonce = [u8; 16];

pub(crate) fn transcript(label: &str, parts: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::with_capacity(64 + parts.iter().map(|p| p.len() + 4).sum::<usize>());
    out.extend_from_slice(label.as_bytes());
    out.push(0);
    for part in parts {
        out.extend_from_slice(
            &u32::try_from(part.len())
                .expect("bounded part")
                .to_be_bytes(),
        );
        out.extend_from_slice(part);
    }
    out
}

/// What a request was signed with, read from its headers.
#[derive(Debug, Clone)]
pub struct RequestProof {
    pub sender: PublicKey,
    pub timestamp: u64,
    pub nonce: Nonce,
    pub signature: [u8; SIGNATURE_LENGTH],
}

pub struct Request<'a> {
    pub recipient: &'a PublicKey,
    pub method: &'a str,
    pub path: &'a str,
    pub body: &'a [u8],
}
impl Request<'_> {
    fn transcript(&self, sender: &PublicKey, timestamp: u64, nonce: &Nonce) -> Vec<u8> {
        transcript(
            "elsecall/peer-request/v1",
            &[
                sender.as_bytes(),
                self.recipient.as_bytes(),
                &timestamp.to_be_bytes(),
                nonce,
                self.method.as_bytes(),
                self.path.as_bytes(),
                self.body,
            ],
        )
    }
    pub fn sign(&self, key: &MemberKey, timestamp: u64, nonce: Nonce) -> RequestProof {
        RequestProof {
            sender: key.public(),
            timestamp,
            nonce,
            signature: key.sign(&self.transcript(&key.public(), timestamp, &nonce)),
        }
    }
    pub fn verify(&self, proof: &RequestProof) -> bool {
        proof.sender.verify(
            &self.transcript(&proof.sender, proof.timestamp, &proof.nonce),
            &proof.signature,
        )
    }
}

impl RequestProof {
    pub fn headers(&self) -> [(&'static str, String); 4] {
        [
            (MEMBER_HEADER, self.sender.to_string()),
            (TIMESTAMP_HEADER, self.timestamp.to_string()),
            (NONCE_HEADER, URL_SAFE_NO_PAD.encode(self.nonce)),
            (SIGNATURE_HEADER, URL_SAFE_NO_PAD.encode(self.signature)),
        ]
    }
    /// Reads a proof from header values; `None` when any is missing or malformed.
    pub fn parse(header: impl Fn(&str) -> Option<String>) -> Option<Self> {
        Some(Self {
            sender: header(MEMBER_HEADER)?.parse().ok()?,
            timestamp: header(TIMESTAMP_HEADER)?.parse().ok()?,
            nonce: URL_SAFE_NO_PAD
                .decode(header(NONCE_HEADER)?)
                .ok()?
                .try_into()
                .ok()?,
            signature: URL_SAFE_NO_PAD
                .decode(header(SIGNATURE_HEADER)?)
                .ok()?
                .try_into()
                .ok()?,
        })
    }
}

pub struct Response<'a> {
    pub responder: &'a PublicKey,
    pub requester: &'a PublicKey,
    pub nonce: &'a Nonce,
    pub status: u16,
    pub body: &'a [u8],
}
impl Response<'_> {
    fn transcript(&self) -> Vec<u8> {
        transcript(
            "elsecall/peer-response/v1",
            &[
                self.responder.as_bytes(),
                self.requester.as_bytes(),
                self.nonce,
                &self.status.to_be_bytes(),
                self.body,
            ],
        )
    }
    pub fn sign(&self, key: &MemberKey) -> String {
        URL_SAFE_NO_PAD.encode(key.sign(&self.transcript()))
    }
    pub fn verify(&self, signature: &str) -> bool {
        URL_SAFE_NO_PAD
            .decode(signature)
            .is_ok_and(|signature| self.responder.verify(&self.transcript(), &signature))
    }
}

/// The header carrying a member's half of a run channel and its signature.
pub const CHANNEL_HEADER: &str = "x-elsecall-channel";

/// A member's acceptance of a run channel: its ephemeral key, signed together with
/// the requester's, both identity keys, the attach request's nonce and the run, so
/// the channel's keys belong to exactly this exchange.
pub struct ChannelAcceptance<'a> {
    pub responder: &'a PublicKey,
    pub requester: &'a PublicKey,
    pub nonce: &'a Nonce,
    pub run: &'a str,
    pub requester_ephemeral: &'a [u8; 32],
    pub responder_ephemeral: &'a [u8; 32],
}
impl ChannelAcceptance<'_> {
    /// What both sides bind the channel's keys to.
    pub fn context(&self) -> Vec<u8> {
        transcript(
            "elsecall/channel/v1",
            &[
                self.requester.as_bytes(),
                self.responder.as_bytes(),
                self.nonce,
                self.run.as_bytes(),
            ],
        )
    }
    fn transcript(&self) -> Vec<u8> {
        transcript(
            "elsecall/channel-accept/v1",
            &[
                &self.context(),
                self.requester_ephemeral,
                self.responder_ephemeral,
            ],
        )
    }
    /// The header value: the responder's ephemeral key and the signature.
    pub fn sign(&self, key: &MemberKey) -> String {
        format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(self.responder_ephemeral),
            URL_SAFE_NO_PAD.encode(key.sign(&self.transcript()))
        )
    }
    /// Reads the responder's ephemeral key from a header value, if its signature
    /// verifies with `responder`.
    pub fn verify(
        header: &str,
        responder: &PublicKey,
        requester: &PublicKey,
        nonce: &Nonce,
        run: &str,
        requester_ephemeral: &[u8; 32],
    ) -> Option<[u8; 32]> {
        let (ephemeral, signature) = header.split_once('.')?;
        let ephemeral: [u8; 32] = URL_SAFE_NO_PAD.decode(ephemeral).ok()?.try_into().ok()?;
        let signature = URL_SAFE_NO_PAD.decode(signature).ok()?;
        let acceptance = ChannelAcceptance {
            responder,
            requester,
            nonce,
            run,
            requester_ephemeral,
            responder_ephemeral: &ephemeral,
        };
        responder
            .verify(&acceptance.transcript(), &signature)
            .then_some(ephemeral)
    }
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn nonce() -> Option<Nonce> {
    let mut nonce = [0u8; 16];
    getrandom::fill(&mut nonce).ok()?;
    Some(nonce)
}

/// Rejects requests outside the clock window and nonces seen within it. Only
/// requests with valid signatures from enrolled members reach the guard, so only
/// members can fill it.
#[derive(Default)]
pub struct ReplayGuard {
    seen: Mutex<HashMap<Nonce, u64>>,
}
impl ReplayGuard {
    pub fn admit(&self, proof: &RequestProof, now: u64) -> bool {
        if proof.timestamp.abs_diff(now) > WINDOW_SECONDS {
            return false;
        }
        let mut seen = self.seen.lock().expect("replay guard lock");
        if seen.len() >= 1024 {
            seen.retain(|_, expiry| *expiry >= now);
        }
        if seen.len() >= MAX_REMEMBERED_NONCES {
            return false;
        }
        // Remember the nonce until its timestamp leaves the window.
        seen.insert(proof.nonce, proof.timestamp + WINDOW_SECONDS)
            .is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request<'a>(recipient: &'a PublicKey, body: &'a [u8]) -> Request<'a> {
        Request {
            recipient,
            method: "PUT",
            path: "/peer/v1/roster",
            body,
        }
    }

    #[test]
    fn request_signatures_bind_every_field() {
        let sender = MemberKey::generate().unwrap();
        let recipient = MemberKey::generate().unwrap().public();
        let other = MemberKey::generate().unwrap().public();
        let proof = request(&recipient, b"body").sign(&sender, 100, [7; 16]);
        assert!(request(&recipient, b"body").verify(&proof));
        let parsed = RequestProof::parse(|name| {
            proof
                .headers()
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, v)| v.clone())
        })
        .unwrap();
        assert!(request(&recipient, b"body").verify(&parsed));
        assert!(
            !request(&other, b"body").verify(&proof),
            "relayed to another member"
        );
        assert!(!request(&recipient, b"bodx").verify(&proof));
        let mut moved = request(&recipient, b"body");
        moved.path = "/peer/v1/status";
        assert!(!moved.verify(&proof));
        for tamper in [
            RequestProof {
                timestamp: 101,
                ..proof.clone()
            },
            RequestProof {
                nonce: [8; 16],
                ..proof.clone()
            },
            RequestProof {
                sender: other,
                ..proof.clone()
            },
        ] {
            assert!(!request(&recipient, b"body").verify(&tamper));
        }
        assert!(RequestProof::parse(|_| Some("x".into())).is_none());
    }

    #[test]
    fn response_signatures_bind_request_status_and_body() {
        let responder = MemberKey::generate().unwrap();
        let requester = MemberKey::generate().unwrap().public();
        let response = |nonce: &'static Nonce, status, body: &'static [u8]| Response {
            responder: Box::leak(Box::new(responder.public())),
            requester: Box::leak(Box::new(requester)),
            nonce,
            status,
            body,
        };
        let signature = response(&[1; 16], 200, b"ok").sign(&responder);
        assert!(response(&[1; 16], 200, b"ok").verify(&signature));
        assert!(!response(&[2; 16], 200, b"ok").verify(&signature));
        assert!(!response(&[1; 16], 403, b"ok").verify(&signature));
        assert!(!response(&[1; 16], 200, b"no").verify(&signature));
        assert!(!response(&[1; 16], 200, b"ok").verify("garbage"));
    }

    #[test]
    fn replay_guard_rejects_reuse_and_clock_skew() {
        let key = MemberKey::generate().unwrap();
        let recipient = key.public();
        let guard = ReplayGuard::default();
        let proof = request(&recipient, b"").sign(&key, 1000, [1; 16]);
        assert!(guard.admit(&proof, 1000));
        assert!(!guard.admit(&proof, 1001), "replayed nonce");
        let late = request(&recipient, b"").sign(&key, 1000, [2; 16]);
        assert!(!guard.admit(&late, 1000 + WINDOW_SECONDS + 1));
        assert!(!guard.admit(&late, 1000 - WINDOW_SECONDS - 1));
        assert!(guard.admit(&late, 1000 + WINDOW_SECONDS));
    }
}

mod channel_proof;
pub(crate) use channel_proof::ChannelProof;
