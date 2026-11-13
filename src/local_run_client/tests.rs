use super::*;
use crate::{
    member_key::MemberKey,
    peer_auth,
    wire::{self, Frame},
};
use hyper::{Response, service::service_fn};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::{io::AsyncWriteExt, net::UnixListener};

// External local-service boundary. Each accepted connection supplies an
// upgrade, never a second attach hidden behind an in-process mock.
async fn service(
    mode: &'static str,
) -> (
    tempfile::TempDir,
    std::path::PathBuf,
    Arc<AtomicUsize>,
    tokio::task::JoinHandle<()>,
) {
    let home = tempfile::Builder::new()
        .prefix("ec-local-run-")
        .tempdir_in("/tmp")
        .unwrap();
    let state = home.path().canonicalize().unwrap().join("state");
    private_files::directory(&state).unwrap();
    let socket = state.join("service.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let task = tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let count = count.clone();
            tokio::spawn(async move {
                let handler =
                    service_fn(move |mut request: hyper::Request<hyper::body::Incoming>| {
                        count.fetch_add(1, Ordering::SeqCst);
                        async move {
                            assert_eq!(request.headers()[header::UPGRADE], sealed_run::PROTOCOL);
                            let public = sealed_run::public_header(request.headers()).unwrap();
                            assert!(
                                request
                                    .body_mut()
                                    .collect()
                                    .await
                                    .unwrap()
                                    .to_bytes()
                                    .is_empty()
                            );
                            let requester = MemberKey::generate().unwrap().public();
                            let responder = MemberKey::generate().unwrap();
                            let ephemeral = Ephemeral::generate().unwrap();
                            let nonce = [21; 16];
                            let run = if mode == "wrong_run" {
                                "r-000000000002"
                            } else {
                                "r-000000000001"
                            };
                            let public = if mode == "wrong_ephemeral" {
                                [7; 32]
                            } else {
                                public
                            };
                            let acceptance = peer_auth::ChannelAcceptance {
                                responder: &responder.public(),
                                requester: &requester,
                                nonce: &nonce,
                                run,
                                requester_ephemeral: &public,
                                responder_ephemeral: &ephemeral.public,
                            };
                            let context = acceptance.context();
                            let proof = peer_auth::ChannelProof::new(
                                run,
                                requester,
                                responder.public(),
                                nonce,
                                public,
                                acceptance.sign(&responder),
                                (peer_auth::Response {
                                    responder: &responder.public(),
                                    requester: &requester,
                                    nonce: &nonce,
                                    status: 101,
                                    body: &[],
                                })
                                .sign(&responder),
                            );
                            let upgrade = hyper::upgrade::on(&mut request);
                            tokio::spawn(async move {
                                if let Ok(upgraded) = upgrade.await {
                                    let stream = TokioIo::new(upgraded);
                                    if mode == "legacy" {
                                        let mut stream = stream;
                                        wire::write(
                                            &mut stream,
                                            &Frame::Stdout(b"same connection".to_vec()),
                                        )
                                        .await
                                        .unwrap();
                                        stream.shutdown().await.unwrap();
                                    } else if mode == "sealed" || mode == "frame" {
                                        let keys =
                                            ephemeral.keys(&public, &context, false).unwrap();
                                        let mut stream = SecureStream::new(stream, keys);
                                        if mode == "frame" {
                                            stream.write_all(&[0, 0, 0, 1, 99]).await.unwrap();
                                        } else {
                                            wire::write(
                                                &mut stream,
                                                &Frame::Stdout(b"sealed".to_vec()),
                                            )
                                            .await
                                            .unwrap();
                                        }
                                        let _ = stream.shutdown().await;
                                    }
                                }
                            });
                            let mut response = Response::builder()
                                .status(101)
                                .header("connection", "upgrade")
                                .header(
                                    "upgrade",
                                    if mode == "legacy" {
                                        "elsecall-run/1"
                                    } else if mode == "unknown" {
                                        "unknown/1"
                                    } else {
                                        sealed_run::PROTOCOL
                                    },
                                );
                            if mode != "legacy" && mode != "missing" {
                                response = response.header(
                                    sealed_run::ACCEPTANCE,
                                    if mode == "malformed" {
                                        "invalid".into()
                                    } else {
                                        sealed_run::encode(&proof)
                                    },
                                );
                            }
                            Ok::<_, std::convert::Infallible>(
                                response.body(Full::new(Bytes::new())).unwrap(),
                            )
                        }
                    });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), handler)
                    .with_upgrades()
                    .await;
            });
        }
    });
    (home, socket, calls, task)
}
#[tokio::test]
async fn negotiation_validates_success_and_legacy_uses_the_same_connection() {
    for mode in [
        "sealed",
        "legacy",
        "wrong_run",
        "wrong_ephemeral",
        "missing",
        "malformed",
        "unknown",
        "frame",
    ] {
        let (_home, socket, calls, task) = service(mode).await;
        let run = RunId::parse("r-000000000001").unwrap();
        let result = attach(&socket, &run).await;
        if ["sealed", "legacy", "frame"].contains(&mode) {
            let mut link = result.unwrap();
            let frame = link.receive().await;
            if mode == "frame" {
                let error = frame.unwrap_err();
                assert_eq!(error.details.unwrap()["peer_stream"]["kind"], "protocol");
            } else {
                assert_eq!(
                    frame.unwrap(),
                    Frame::Stdout(if mode == "legacy" {
                        b"same connection".to_vec()
                    } else {
                        b"sealed".to_vec()
                    })
                );
            }
        } else {
            assert_eq!(result.err().unwrap().code, "protocol_error", "{mode}");
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "no second attach for {mode}"
        );
        task.abort();
    }
}
