//! Untrusted peer boundary: signed but invalid channel evidence must fail before
//! a successful attempt is recorded, for run and frozen non-run callers alike.
use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::post,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use elsecall::{
    member_key::MemberKey, peer_auth, peer_client, roster::Member, secure_channel::Ephemeral,
};
use std::sync::Arc;
#[derive(Clone)]
struct Peer {
    key: Arc<MemberKey>,
    fault: &'static str,
}
async fn answer(
    State(peer): State<Peer>,
    headers: HeaderMap,
    body: Bytes,
) -> axum::response::Response {
    let proof = peer_auth::RequestProof::parse(|name| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    })
    .unwrap();
    assert!(
        peer_auth::Request {
            recipient: &peer.key.public(),
            method: "POST",
            path: "/channel",
            body: &body
        }
        .verify(&proof)
    );
    if peer.fault == "refused" {
        let bytes = serde_json::to_vec(&serde_json::json!({"schema_version":1,"error":{"code":"member_upgrading","message":"upgrading","hint":"retry later"}})).unwrap();
        let signature = peer_auth::Response {
            responder: &peer.key.public(),
            requester: &proof.sender,
            nonce: &proof.nonce,
            status: 409,
            body: &bytes,
        }
        .sign(&peer.key);
        return (
            StatusCode::CONFLICT,
            [(peer_auth::SIGNATURE_HEADER, signature)],
            bytes,
        )
            .into_response();
    }
    let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let public: [u8; 32] = URL_SAFE_NO_PAD
        .decode(value["ephemeral"].as_str().unwrap())
        .unwrap()
        .try_into()
        .unwrap();
    let public = if peer.fault == "wrong_ephemeral" {
        [7; 32]
    } else {
        public
    };
    let theirs = if peer.fault == "noncontributory" {
        [0; 32]
    } else {
        Ephemeral::generate().unwrap().public
    };
    let channel = peer_auth::ChannelAcceptance {
        responder: &peer.key.public(),
        requester: &proof.sender,
        nonce: &proof.nonce,
        run: if peer.fault == "wrong_run" {
            "other"
        } else {
            "purpose"
        },
        requester_ephemeral: &public,
        responder_ephemeral: &theirs,
    }
    .sign(&peer.key);
    let signature = peer_auth::Response {
        responder: &peer.key.public(),
        requester: &proof.sender,
        nonce: &proof.nonce,
        status: 101,
        body: &[],
    }
    .sign(&peer.key);
    let mut response = StatusCode::SWITCHING_PROTOCOLS.into_response();
    response
        .headers_mut()
        .insert("connection", "upgrade".parse().unwrap());
    response
        .headers_mut()
        .insert("upgrade", headers["upgrade"].clone());
    if peer.fault != "missing_channel" {
        response
            .headers_mut()
            .insert(peer_auth::CHANNEL_HEADER, channel.parse().unwrap());
    }
    if peer.fault != "missing_signature" {
        response.headers_mut().insert(
            peer_auth::SIGNATURE_HEADER,
            if peer.fault == "bad_signature" {
                URL_SAFE_NO_PAD.encode([0; 64])
            } else {
                signature
            }
            .parse()
            .unwrap(),
        );
    }
    response
}
#[tokio::test]
async fn signed_noncontributory_or_missing_proofs_remain_authentication_failures() {
    elsecall::tailnet::allow_loopback_members();
    for protocol in ["elsecall-run/1", "elsecall-sync/1", "elsecall-upgrade/1"] {
        for fault in [
            "noncontributory",
            "missing_channel",
            "missing_signature",
            "bad_signature",
            "wrong_run",
            "wrong_ephemeral",
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let peer = Peer {
                key: Arc::new(MemberKey::generate().unwrap()),
                fault,
            };
            let member = Member::new(
                "peer".parse().unwrap(),
                peer.key.public(),
                vec![listener.local_addr().unwrap()],
            )
            .unwrap();
            let app = Router::new()
                .route("/channel", post(answer))
                .with_state(peer);
            let server = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            let key = MemberKey::generate().unwrap();
            let error = peer_client::channel(&key, &member, "/channel", "purpose", protocol)
                .await
                .err()
                .unwrap();
            assert_eq!(error.code, "member_unverified", "{protocol} {fault}");
            assert_eq!(
                error.details.unwrap()["peer_io"]["failure_kind"],
                "authentication",
                "{protocol} {fault}"
            );
            server.abort();
        }
    }
}
#[tokio::test]
async fn a_verified_channel_refusal_is_recorded_as_the_members_refusal() {
    elsecall::tailnet::allow_loopback_members();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let peer = Peer {
        key: Arc::new(MemberKey::generate().unwrap()),
        fault: "refused",
    };
    let member = Member::new(
        "peer".parse().unwrap(),
        peer.key.public(),
        vec![listener.local_addr().unwrap()],
    )
    .unwrap();
    let app = Router::new()
        .route("/channel", post(answer))
        .with_state(peer);
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let key = MemberKey::generate().unwrap();
    let error = peer_client::channel(&key, &member, "/channel", "purpose", "elsecall-run/1")
        .await
        .err()
        .unwrap();
    assert_eq!(error.code, "member_upgrading");
    let details = error.details.unwrap();
    assert_eq!(details["peer_io"]["failure_kind"], "remote");
    assert_eq!(details["peer_io"]["phase"], "request");
    assert_eq!(details["peer_io"]["refusal"], "member_upgrading");
    server.abort();
}
