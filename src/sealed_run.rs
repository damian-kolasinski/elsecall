//! Bounded headers for negotiating a sealed local run. These values are public;
//! shared signature/transcript verification belongs to peer authentication.
use crate::peer_auth::ChannelProof;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use hyper::HeaderMap;
pub(crate) const PROTOCOL: &str = "elsecall-run-sealed/1";
pub(crate) const EPHEMERAL: &str = "x-elsecall-run-ephemeral";
pub(crate) const ACCEPTANCE: &str = "x-elsecall-run-acceptance";
const LIMIT: usize = 2048;

pub(crate) fn encode(proof: &ChannelProof) -> String {
    URL_SAFE_NO_PAD.encode(serde_json::to_vec(proof).expect("public proof serializes"))
}
pub(crate) fn decode(headers: &HeaderMap) -> Option<ChannelProof> {
    let value = one_header(headers, ACCEPTANCE)?;
    if value.len() > LIMIT {
        return None;
    }
    serde_json::from_slice(&URL_SAFE_NO_PAD.decode(value).ok()?).ok()
}
pub(crate) fn one_header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let mut values = headers.get_all(name).iter();
    let value = values.next()?.to_str().ok()?;
    values.next().is_none().then_some(value)
}
pub(crate) fn public_header(headers: &HeaderMap) -> Option<[u8; 32]> {
    let value = one_header(headers, EPHEMERAL)?;
    if value.len() != 43 {
        return None;
    }
    let public: [u8; 32] = URL_SAFE_NO_PAD.decode(value).ok()?.try_into().ok()?;
    // Reject noncontributory public halves before consuming a remote admission.
    crate::secure_channel::is_contributory(&public).then_some(public)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        member_key::MemberKey,
        peer_auth::{self, ChannelAcceptance},
        secure_channel::Ephemeral,
    };
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
    fn local_headers_are_bounded_unique_and_strict() {
        let public = Ephemeral::generate().unwrap().public;
        let good = evidence("r-123456789abc", public);
        let mut headers = HeaderMap::new();
        headers.insert(ACCEPTANCE, encode(&good).parse().unwrap());
        assert!(
            decode(&headers)
                .unwrap()
                .verify("r-123456789abc", &public)
                .is_some()
        );
        headers.append(ACCEPTANCE, encode(&good).parse().unwrap());
        assert!(decode(&headers).is_none());
        headers.insert(ACCEPTANCE, "x".repeat(LIMIT + 1).parse().unwrap());
        assert!(decode(&headers).is_none());
        let mut value = serde_json::to_value(&good).unwrap();
        value["unknown"] = serde_json::json!(1);
        headers.insert(
            ACCEPTANCE,
            URL_SAFE_NO_PAD.encode(value.to_string()).parse().unwrap(),
        );
        assert!(decode(&headers).is_none());
        for invalid in [
            "garbage".into(),
            URL_SAFE_NO_PAD.encode([0; 32]),
            "x".repeat(4096),
        ] {
            headers.insert(EPHEMERAL, invalid.parse().unwrap());
            assert!(public_header(&headers).is_none());
        }
        headers.insert(EPHEMERAL, URL_SAFE_NO_PAD.encode(public).parse().unwrap());
        assert_eq!(public_header(&headers), Some(public));
    }
}
