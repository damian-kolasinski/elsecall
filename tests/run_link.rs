//! The public run link against external byte streams, without a peer service.
use elsecall::{
    remote_run::link::{GRACE, Link},
    wire::{self, Frame},
};
use std::time::Duration;
use tokio::io::AsyncWriteExt;

#[tokio::test(start_paused = true)]
async fn fragmented_headers_and_payloads_can_take_longer_than_the_grace_period() {
    for frame in [
        Frame::Stdout(vec![0, 255, 128]),
        Frame::Stderr(vec![128, 0, 255]),
        Frame::Content(vec![1, 2, 3]),
    ] {
        let mut encoded = Vec::new();
        wire::write(&mut encoded, &frame).await.unwrap();
        let (stream, mut peer) = tokio::io::duplex(4096);
        let mut link = Link::open(stream);
        let sender = tokio::spawn(async move {
            for byte in encoded {
                tokio::time::sleep(Duration::from_secs(2)).await;
                peer.write_all(&[byte]).await.unwrap();
            }
        });
        let started = tokio::time::Instant::now();
        assert_eq!(link.receive().await.unwrap(), frame);
        assert!(started.elapsed() > GRACE);
        sender.await.unwrap();
    }
}

#[tokio::test(start_paused = true)]
async fn a_stalled_partial_header_or_payload_times_out_from_the_last_progress() {
    for bytes in [&[0, 0][..], &[0, 0, 0, 4, 4, 7][..]] {
        let (stream, mut peer) = tokio::io::duplex(4096);
        let mut link = Link::open(stream);
        let bytes = bytes.to_vec();
        let sender = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(6)).await;
            peer.write_all(&bytes).await.unwrap();
            peer
        });
        let started = tokio::time::Instant::now();
        // Retain the silent peer's connection after its partial write.
        let held = tokio::spawn(async move {
            let peer = sender.await.unwrap();
            tokio::time::sleep(Duration::from_secs(60)).await;
            drop(peer);
        });
        let failure = link.receive().await.unwrap_err();
        assert_eq!(started.elapsed(), Duration::from_secs(6) + GRACE);
        assert_eq!(failure.code, "connection_lost");
        assert_eq!(
            failure.details.as_ref().unwrap()["peer_stream"]["kind"],
            "timeout"
        );
        assert_eq!(
            failure.details.as_ref().unwrap()["peer_stream"]["phase"],
            "receive"
        );
        held.abort();
    }
}

#[tokio::test(start_paused = true)]
async fn eof_between_frames_or_mid_frame_is_immediate() {
    for bytes in [&[][..], &[0, 0][..], &[0, 0, 0, 4, 4, 7][..]] {
        let (stream, mut peer) = tokio::io::duplex(4096);
        peer.write_all(bytes).await.unwrap();
        drop(peer);
        let mut link = Link::open(stream);
        let started = tokio::time::Instant::now();
        let failure = link.receive().await.unwrap_err();
        assert_eq!(started.elapsed(), Duration::ZERO);
        assert_eq!(failure.code, "connection_lost");
        assert_eq!(
            failure.details.as_ref().unwrap()["peer_stream"]["kind"],
            "receive"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn heartbeats_keep_a_quiet_command_alive() {
    let (stream, mut peer) = tokio::io::duplex(4096);
    let mut link = Link::open(stream);
    let sender = tokio::spawn(async move {
        for _ in 0..15 {
            wire::write(&mut peer, &Frame::Heartbeat).await.unwrap();
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        wire::write(&mut peer, &Frame::Stdout(vec![7]))
            .await
            .unwrap();
    });
    assert_eq!(link.receive().await.unwrap(), Frame::Stdout(vec![7]));
    sender.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn bounded_delivery_backpressure_does_not_spend_the_next_read_budget() {
    let (stream, mut peer) = tokio::io::duplex(65536);
    let mut link = Link::open(stream);
    let sender = tokio::spawn(async move {
        for n in 0..64 {
            wire::write(&mut peer, &Frame::Stdout(vec![n; 65536]))
                .await
                .unwrap();
        }
    });
    tokio::time::sleep(GRACE * 2).await;
    assert!(
        !sender.is_finished(),
        "a paused consumer must backpressure the sender"
    );
    for n in 0..64 {
        assert_eq!(link.receive().await.unwrap(), Frame::Stdout(vec![n; 65536]));
    }
    sender.await.unwrap();
}

#[tokio::test]
async fn a_read_failure_wakes_blocked_producers_without_receiving_and_keeps_its_origin() {
    use elsecall::secure_channel::{Ephemeral, SecureStream};
    for kind in ["protocol", "authentication", "receive"] {
        let (stream, mut peer) = tokio::io::duplex(64);
        let link = if kind == "authentication" {
            let ephemeral = Ephemeral::generate().unwrap();
            let other = Ephemeral::generate().unwrap();
            let keys = ephemeral.keys(&other.public, b"test", true).unwrap();
            Link::open(SecureStream::new(stream, keys))
        } else {
            Link::open(stream)
        };
        let outbox = link.outbox();
        // The peer consumes no output: one frame blocks in the stream and
        // sixteen fill the bounded queue. No receive() is called to unblock it.
        for _ in 0..17 {
            outbox.send(Frame::Stdin(vec![7; 65536])).await.unwrap();
        }
        let blocked = outbox.clone();
        let mut producer =
            tokio::spawn(async move { blocked.send(Frame::Stdin(vec![8; 65536])).await });
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut producer)
                .await
                .is_err()
        );
        match kind {
            "protocol" => peer.write_all(&[0, 0, 0, 0]).await.unwrap(),
            "authentication" => peer.write_all(&65537u32.to_be_bytes()).await.unwrap(),
            _ => peer.shutdown().await.unwrap(),
        }
        let failure = tokio::time::timeout(Duration::from_secs(1), producer)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert_eq!(
            failure.details.as_ref().unwrap()["peer_stream"]["kind"],
            kind
        );
        assert_eq!(
            failure.details.as_ref().unwrap()["peer_stream"]["phase"],
            "receive"
        );
        drop(peer);
        let later = outbox.send(Frame::Heartbeat).await.unwrap_err();
        assert_eq!(
            later.details, failure.details,
            "cleanup must keep the first failure"
        );
    }
}
