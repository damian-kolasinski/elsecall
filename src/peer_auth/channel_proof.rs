//! Verification and derivation of an existing signed channel transcript.
use crate::{
    member_key::PublicKey,
    peer_auth::{self, ChannelAcceptance},
    secure_channel::{Ephemeral, Keys},
};
use serde::{Deserialize, Serialize};
/// Signed evidence for an ephemeral channel, independent of its transport and
/// the adapter that will own decryption. Only public values are serialized.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ChannelProof {
    run: String,
    requester: PublicKey,
    responder: PublicKey,
    nonce: peer_auth::Nonce,
    requester_ephemeral: [u8; 32],
    channel: String,
    response: String,
}

impl ChannelProof {
    pub(crate) fn new(
        run: &str,
        requester: PublicKey,
        responder: PublicKey,
        nonce: peer_auth::Nonce,
        requester_ephemeral: [u8; 32],
        channel: String,
        response: String,
    ) -> Self {
        Self {
            run: run.into(),
            requester,
            responder,
            nonce,
            requester_ephemeral,
            channel,
            response,
        }
    }
    /// Recompute the existing signed transcript from the caller's own run and
    /// ephemeral. Never accept an arbitrary key-derivation context from IPC.
    pub(crate) fn verify(&self, run: &str, public: &[u8; 32]) -> Option<([u8; 32], Vec<u8>)> {
        if self.channel.len() != 130
            || self.response.len() != 86
            || self.run != run
            || self.requester_ephemeral != *public
            || !(peer_auth::Response {
                responder: &self.responder,
                requester: &self.requester,
                nonce: &self.nonce,
                status: 101,
                body: &[],
            })
            .verify(&self.response)
        {
            return None;
        }
        let peer = ChannelAcceptance::verify(
            &self.channel,
            &self.responder,
            &self.requester,
            &self.nonce,
            run,
            public,
        )?;
        if !crate::secure_channel::is_contributory(&peer) {
            return None;
        }
        let context = ChannelAcceptance {
            responder: &self.responder,
            requester: &self.requester,
            nonce: &self.nonce,
            run,
            requester_ephemeral: public,
            responder_ephemeral: &peer,
        }
        .context();
        Some((peer, context))
    }
    pub(crate) fn keys(self, run: &str, ephemeral: Ephemeral) -> Option<Keys> {
        let (peer, context) = self.verify(run, &ephemeral.public)?;
        ephemeral.keys(&peer, &context, true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::member_key::MemberKey;
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    fn evidence(run: &str, public: [u8; 32]) -> ChannelProof {
        let requester = MemberKey::generate().unwrap().public();
        let responder = MemberKey::generate().unwrap();
        let theirs = Ephemeral::generate().unwrap().public;
        let nonce = [17; 16];
        let channel = ChannelAcceptance {
            responder: &responder.public(),
            requester: &requester,
            nonce: &nonce,
            run,
            requester_ephemeral: &public,
            responder_ephemeral: &theirs,
        }
        .sign(&responder);
        let response = peer_auth::Response {
            responder: &responder.public(),
            requester: &requester,
            nonce: &nonce,
            status: 101,
            body: &[],
        }
        .sign(&responder);
        ChannelProof::new(
            run,
            requester,
            responder.public(),
            nonce,
            public,
            channel,
            response,
        )
    }
    #[test]
    fn public_evidence_binds_every_transcript_input_and_both_proofs() {
        let public = Ephemeral::generate().unwrap().public;
        let good = evidence("r-123456789abc", public);
        assert!(good.verify("r-123456789abc", &public).is_some());
        assert!(good.verify("r-123456789abd", &public).is_none());
        assert!(good.verify("r-123456789abc", &[1; 32]).is_none());
        for field in [
            "run",
            "requester",
            "responder",
            "nonce",
            "requester_ephemeral",
            "channel",
            "response",
        ] {
            let mut value = serde_json::to_value(&good).unwrap();
            value[field] = match field {
                "run" => serde_json::json!("r-123456789abd"),
                "requester" | "responder" => {
                    serde_json::json!(MemberKey::generate().unwrap().public())
                }
                "nonce" => serde_json::to_value([8; 16]).unwrap(),
                "requester_ephemeral" => serde_json::to_value([8; 32]).unwrap(),
                "channel" => serde_json::json!(format!(
                    "{}.{}",
                    URL_SAFE_NO_PAD.encode([8; 32]),
                    URL_SAFE_NO_PAD.encode([8; 64])
                )),
                _ => serde_json::json!(URL_SAFE_NO_PAD.encode([8; 64])),
            };
            let bad: ChannelProof = serde_json::from_value(value).unwrap();
            assert!(bad.verify("r-123456789abc", &public).is_none(), "{field}");
        }
    }
}
