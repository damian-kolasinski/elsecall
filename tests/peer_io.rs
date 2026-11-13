//! Faults injected at the network boundary, with real signed requests/responses.
use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::any,
};
use elsecall::{
    member_key::MemberKey,
    peer_auth, peer_client,
    peer_diagnostics::{self, Kind, Phase},
    roster::Member,
    scheduling::timestamp::Timestamp,
};
use hyper::Method;
use serde_json::{Value, json};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
#[derive(Clone)]
struct Peer {
    key: Arc<MemberKey>,
    calls: Arc<AtomicUsize>,
    nonces: Arc<Mutex<Vec<peer_auth::Nonce>>>,
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
            .and_then(|h| h.to_str().ok())
            .map(str::to_owned)
    })
    .unwrap();
    let count = peer.calls.fetch_add(1, Ordering::SeqCst);
    assert!(
        peer_auth::Request {
            recipient: &peer.key.public(),
            method: headers.get("x-test-method").map_or("GET", |_| "POST"),
            path: "/peer/v1/status",
            body: &body
        }
        .verify(&proof)
            || !body.is_empty()
    );
    peer.nonces.lock().unwrap().push(proof.nonce);
    if peer.fault == "stall" || peer.fault == "timeout_once" && count == 0 {
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    if peer.fault == "slow" {
        tokio::time::sleep(Duration::from_millis(2100)).await;
    }
    let refused = matches!(
        peer.fault,
        "remote" | "remote_transport_code" | "remote_malformed_code"
    );
    let value = if refused {
        let code = match peer.fault {
            "remote_transport_code" => "member_unavailable",
            "remote_malformed_code" => "Denied: see /Users/someone/secret",
            _ => "not_master",
        };
        json!({"schema_version":1,"error":{"code":code,"message":"denied","hint":"retry elsewhere","details":{"upstream":"preserved"}}})
    } else {
        let schema = if peer.fault == "bad_schema" { 99 } else { 1 };
        json!({"schema_version":schema,"data":{"ok":true}})
    };
    let bytes = serde_json::to_vec(&value).unwrap();
    let status = if refused {
        StatusCode::FORBIDDEN
    } else {
        StatusCode::OK
    };
    let signature = peer_auth::Response {
        responder: &peer.key.public(),
        requester: &proof.sender,
        nonce: &proof.nonce,
        status: status.as_u16(),
        body: &bytes,
    }
    .sign(&peer.key);
    if peer.fault == "bad_auth" || peer.fault == "retry_auth" && count == 0 {
        return (status, bytes).into_response();
    }
    (status, [(peer_auth::SIGNATURE_HEADER, signature)], bytes).into_response()
}
async fn server(fault: &'static str) -> (Member, Peer, tokio::task::JoinHandle<()>) {
    elsecall::tailnet::allow_loopback_members();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let peer = Peer {
        key: Arc::new(MemberKey::generate().unwrap()),
        calls: Arc::new(AtomicUsize::new(0)),
        nonces: Default::default(),
        fault,
    };
    let member = Member::new(
        "peer".parse().unwrap(),
        peer.key.public(),
        vec![listener.local_addr().unwrap()],
    )
    .unwrap();
    let app = Router::new().fallback(any(answer)).with_state(peer.clone());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (member, peer, task)
}
async fn call(
    key: &MemberKey,
    member: &Member,
    path: &str,
    timeout: Duration,
) -> elsecall::failure::Result<Value> {
    peer_client::call(key, member, Method::GET, path, None::<&Value>, timeout).await
}
#[tokio::test]
async fn failures_are_typed_bounded_retained_and_only_idempotent_requests_retry() {
    let home = tempfile::Builder::new()
        .prefix("ec-peer-")
        .tempdir_in("/tmp")
        .unwrap();
    let paths = elsecall::paths::Paths::for_home(home.path()).unwrap();
    elsecall::private_files::directory(&paths.data).unwrap();
    peer_diagnostics::initialize(&paths).unwrap();
    let key = MemberKey::generate().unwrap();
    let start = Timestamp::now();
    let (member, peer, task) = server("bad_auth").await;
    let error = call(&key, &member, "/peer/v1/status", Duration::from_secs(2))
        .await
        .unwrap_err();
    assert_eq!(
        error.details.as_ref().unwrap()["peer_io"]["failure_kind"],
        "authentication"
    );
    assert_eq!(peer.calls.load(Ordering::SeqCst), 1);
    task.abort();
    // A verified refusal is the member's answer to the whole request: no I/O phase
    // failed, and its stable code names what the member declined with.
    let (member, peer, task) = server("remote").await;
    let error = call(&key, &member, "/peer/v1/status", Duration::from_secs(2))
        .await
        .unwrap_err();
    assert_eq!(error.code, "not_master");
    let peer_io = &error.details.as_ref().unwrap()["peer_io"];
    assert_eq!(peer_io["failure_kind"], "remote");
    assert_eq!(peer_io["phase"], "request");
    assert_eq!(peer_io["refusal"], "not_master");
    assert_eq!(peer.calls.load(Ordering::SeqCst), 1);
    task.abort();
    // An answer whose code is not a stable code keeps the refusal, without the code.
    let (member, _, task) = server("remote_malformed_code").await;
    let error = call(&key, &member, "/peer/v1/status", Duration::from_secs(2))
        .await
        .unwrap_err();
    let peer_io = &error.details.as_ref().unwrap()["peer_io"];
    assert_eq!(peer_io["failure_kind"], "remote");
    assert_eq!(peer_io["phase"], "request");
    assert!(peer_io.get("refusal").is_none());
    task.abort();
    // A correctly signed answer it cannot read failed past authentication.
    let (member, _, task) = server("bad_schema").await;
    let error = call(&key, &member, "/peer/v1/status", Duration::from_secs(2))
        .await
        .unwrap_err();
    let peer_io = &error.details.as_ref().unwrap()["peer_io"];
    assert_eq!(peer_io["failure_kind"], "protocol");
    assert_eq!(peer_io["phase"], "receive");
    task.abort();
    let (member, peer, task) = server("remote_transport_code").await;
    let error = call(&key, &member, "/peer/v1/status", Duration::from_secs(2))
        .await
        .unwrap_err();
    assert_eq!(peer.calls.load(Ordering::SeqCst), 1);
    assert_eq!(error.details.as_ref().unwrap()["upstream"], "preserved");
    assert_eq!(
        error.details.as_ref().unwrap()["peer_io"]["failure_kind"],
        "remote"
    );
    assert_eq!(
        error.details.as_ref().unwrap()["peer_io"]["refusal"],
        "member_unavailable"
    );
    task.abort();
    let (member, peer, task) = server("stall").await;
    let error = call(&key, &member, "/peer/v1/status", Duration::from_millis(120))
        .await
        .unwrap_err();
    assert_eq!(
        error.details.as_ref().unwrap()["peer_io"]["failure_kind"],
        "timeout"
    );
    assert_eq!(
        error.details.as_ref().unwrap()["peer_io"]["phase"],
        "receive"
    );
    assert_eq!(peer.calls.load(Ordering::SeqCst), 1);
    task.abort();
    let (member, peer, task) = server("timeout_once").await;
    assert_eq!(
        call(&key, &member, "/peer/v1/status", Duration::from_millis(400))
            .await
            .unwrap()["ok"],
        true
    );
    assert_eq!(peer.calls.load(Ordering::SeqCst), 2);
    task.abort();
    let (member, _, task) = server("slow").await;
    assert_eq!(
        call(&key, &member, "/peer/v1/status", Duration::from_secs(6))
            .await
            .unwrap()["ok"],
        true
    );
    task.abort();
    // A closed port is a connect failure, retried twice within the whole budget.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let member = Member::new(
        "peer".parse().unwrap(),
        MemberKey::generate().unwrap().public(),
        vec![addr],
    )
    .unwrap();
    let error = call(&key, &member, "/peer/v1/status", Duration::from_secs(1))
        .await
        .unwrap_err();
    assert_eq!(error.details.as_ref().unwrap()["attempts"], 2);
    assert_eq!(
        error.details.as_ref().unwrap()["peer_io"]["failure_kind"],
        "connect"
    );
    // Drop after reading the request: a response/receive failure. A fresh signed
    // retry succeeds, while non-idempotent offers are never sent twice.
    for (path, partial_body, read_only) in [
        ("/peer/v1/status", false, false),
        ("/peer/v1/status", true, false),
        ("/peer/v1/placements/quote", false, false),
        ("/peer/v1/placements", false, false),
        ("/peer/v1/rehearsal", false, true),
        ("/peer/v1/rehearsal", false, false),
    ] {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let responder = MemberKey::generate().unwrap();
        let member = Member::new(
            "peer".parse().unwrap(),
            responder.public(),
            vec![listener.local_addr().unwrap()],
        )
        .unwrap();
        let handle = tokio::spawn(async move {
            let mut nonces = Vec::new();
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    request.push(stream.read_u8().await.unwrap());
                }
                let request = String::from_utf8(request).unwrap();
                let proof = peer_auth::RequestProof::parse(|name| {
                    request.lines().find_map(|l| {
                        l.split_once(':')
                            .filter(|(n, _)| n.eq_ignore_ascii_case(name))
                            .map(|(_, v)| v.trim().to_owned())
                    })
                })
                .unwrap();
                assert!(!nonces.contains(&proof.nonce));
                nonces.push(proof.nonce);
                if nonces.len() == 1 {
                    if partial_body {
                        stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 200\r\nconnection: close\r\n\r\n{\"schema_version\":").await.unwrap();
                    }
                    continue;
                }
                let body = b"{\"schema_version\":1,\"data\":{\"ok\":true}}";
                let sig = peer_auth::Response {
                    responder: &responder.public(),
                    requester: &proof.sender,
                    nonce: &proof.nonce,
                    status: 200,
                    body,
                }
                .sign(&responder);
                let head = format!(
                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nx-elsecall-signature: {sig}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(head.as_bytes()).await.unwrap();
                stream.write_all(body).await.unwrap();
            }
        });
        let method = if path == "/peer/v1/status" {
            Method::GET
        } else {
            Method::POST
        };
        let result: elsecall::failure::Result<Value> = if read_only {
            peer_client::call_with_retry(
                &key,
                &member,
                method,
                path,
                None::<&Value>,
                Duration::from_secs(2),
                peer_client::Retry::Idempotent,
            )
            .await
        } else {
            peer_client::call(
                &key,
                &member,
                method,
                path,
                None::<&Value>,
                Duration::from_secs(2),
            )
            .await
        };
        if path == "/peer/v1/placements" || path == "/peer/v1/rehearsal" && !read_only {
            assert!(result.is_err());
            handle.abort();
        } else {
            assert_eq!(result.unwrap()["ok"], true);
            handle.await.unwrap();
        }
    }
    let records = peer_diagnostics::between(start, Timestamp::now()).attempts;
    assert!(
        records
            .iter()
            .any(|a| a.failure_kind == Some(Kind::Authentication))
    );
    assert!(records.iter().any(|a| a.failure_kind == Some(Kind::Timeout)
        && a.phase == Phase::Receive
        && a.duration_ms >= 50));
    assert!(
        records
            .iter()
            .any(|a| a.failure_kind == Some(Kind::Receive))
    );
    assert!(
        records
            .iter()
            .any(|a| a.operation == "quote" && a.attempt == 1)
    );
    // Retained history keeps the refusal's phase and code, and never its message.
    assert!(records.iter().any(|a| a.failure_kind == Some(Kind::Remote)
        && a.phase == Phase::Request
        && a.refusal.as_ref().map(|c| c.as_str()) == Some("not_master")));
    let retained = serde_json::to_string(&records).unwrap();
    assert!(!retained.contains("denied") && !retained.contains("/Users/someone"));
    assert!(
        !serde_json::to_string(&records)
            .unwrap()
            .contains("x-elsecall")
    );
}
