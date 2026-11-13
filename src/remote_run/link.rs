//! Frames between a caller and a member, with liveness: each side sends a heartbeat
//! every second it has nothing else to send, and treats ten seconds of silence, or
//! the stream ending, as a lost connection.

use crate::{
    failure::{Failure, Result},
    remote_run::protocol::RunMessage,
    wire::{self, Frame},
    workspace::sync::{self, Received, SyncMessage},
};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt},
    sync::{Notify, mpsc},
    task::JoinHandle,
};

pub const HEARTBEAT: Duration = Duration::from_secs(1);
/// Silence longer than this is a lost connection.
pub const GRACE: Duration = Duration::from_secs(10);

pub fn lost(detail: &str) -> Failure {
    Failure::new(
        "connection_lost",
        format!("Contact with the running command was lost: {detail}."),
        "Check the member with elsecall fleet. The command was stopped on the member; its record says how far it got.",
    )
}

pub struct Link {
    outgoing: Outbox,
    incoming: mpsc::Receiver<Result<Frame>>,
    tasks: [JoinHandle<()>; 2],
}

impl Drop for Link {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

/// Sends frames from other tasks, such as output readers.
#[derive(Clone)]
pub struct Outbox {
    sender: mpsc::Sender<Frame>,
    failure: Arc<Mutex<Option<Failure>>>,
}
impl Outbox {
    pub async fn send(&self, frame: Frame) -> Result<()> {
        if !frame.fits() {
            return Err(Failure::new(
                "protocol_error",
                "A run message was larger than one frame.",
                "Report this; Elsecall chunks bulk data.",
            ));
        }
        self.sender.send(frame).await.map_err(|_| {
            self.failure
                .lock()
                .expect("terminal failure lock")
                .clone()
                .unwrap_or_else(|| lost("the link closed"))
        })
    }
    pub async fn message(&self, message: &RunMessage) -> Result<()> {
        self.send(Frame::Message(
            serde_json::to_vec(message).expect("messages serialize"),
        ))
        .await
    }
}

impl Link {
    pub fn open<S>(stream: S) -> Self
    where
        S: AsyncRead + AsyncWrite + Send + 'static,
    {
        Self::open_monitored(crate::receive_inactivity::Reader::new(
            Box::pin(stream),
            GRACE,
        ))
    }

    /// The encryption owner already monitors raw receive inactivity beneath
    /// decryption. Authenticated plaintext may legitimately take longer than GRACE.
    pub(crate) fn open_monitored<S>(stream: S) -> Self
    where
        S: AsyncRead + AsyncWrite + Send + 'static,
    {
        let (mut reader, mut writer) = tokio::io::split(stream);
        let (outgoing, mut to_write) = mpsc::channel::<Frame>(16);
        let (to_deliver, incoming) = mpsc::channel(16);
        let started = std::time::Instant::now();
        let terminal_failure = Arc::new(Mutex::new(None));
        let failure_slot = terminal_failure.clone();
        let send_failed = Arc::new(Notify::new());
        let write_failed = send_failed.clone();
        let write = tokio::spawn(async move {
            let mut heartbeat =
                tokio::time::interval_at(tokio::time::Instant::now() + HEARTBEAT, HEARTBEAT);
            heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                let frame = tokio::select! {
                    frame = to_write.recv() => match frame {
                        Some(frame) => frame,
                        None => break,
                    },
                    _ = heartbeat.tick() => Frame::Heartbeat,
                };
                let sent = async {
                    wire::write(&mut writer, &frame).await?;
                    writer.flush().await
                };
                if let Err(error) = sent.await {
                    let failure =
                        stream_failure(&error, crate::peer_diagnostics::Phase::Send, started);
                    remember(&failure_slot, failure);
                    // Closing the sender queue wakes blocked producers even when
                    // delivery of this failure waits behind bounded received data.
                    drop(to_write);
                    write_failed.notify_one();
                    return;
                }
                if !matches!(frame, Frame::Heartbeat) {
                    heartbeat.reset();
                }
            }
            let _ = writer.shutdown().await;
        });
        let stop_writer = write.abort_handle();
        let read_failure = terminal_failure.clone();
        let read = tokio::spawn(async move {
            loop {
                // Drain already-ready frames before a send-side failure. A
                // failed heartbeat must not overtake buffered output or Exited.
                let next = tokio::select! {
                    biased;
                    frame = wire::read(&mut reader) => frame.map_err(|error|
                        stream_failure(&error, crate::peer_diagnostics::Phase::Receive, started)),
                    _ = send_failed.notified() => Err(read_failure.lock().expect("terminal failure lock").clone().expect("notified failure")),
                };
                let frame = match next {
                    Ok(Some(Frame::Heartbeat)) => continue,
                    Ok(Some(frame)) => Ok(frame),
                    Ok(None) => Err(stream_failure(
                        &std::io::ErrorKind::UnexpectedEof.into(),
                        crate::peer_diagnostics::Phase::Receive,
                        started,
                    )),
                    Err(error) => Err(error),
                };
                let last = frame.is_err();
                let frame = frame.map_err(|failure| {
                    // Publish the original typed failure before aborting the
                    // writer closes its queue and wakes blocked producers.
                    let failure = remember(&read_failure, failure);
                    stop_writer.abort();
                    failure
                });
                if to_deliver.send(frame).await.is_err() || last {
                    return;
                }
            }
        });
        Self {
            outgoing: Outbox {
                sender: outgoing,
                failure: terminal_failure,
            },
            incoming,
            tasks: [write, read],
        }
    }

    pub fn outbox(&self) -> Outbox {
        self.outgoing.clone()
    }

    pub async fn send(&self, frame: Frame) -> Result<()> {
        self.outbox().send(frame).await
    }

    pub async fn message(&self, message: &RunMessage) -> Result<()> {
        self.outbox().message(message).await
    }

    /// The next frame other than a heartbeat.
    pub async fn receive(&mut self) -> Result<Frame> {
        self.incoming
            .recv()
            .await
            .unwrap_or_else(|| Err(lost("the connection closed")))
    }

    /// Waits until everything queued so far is written, then closes the link.
    pub async fn close(mut self) {
        // The writer ends once every sender, including outboxes, is gone.
        let (closed, _) = mpsc::channel(1);
        drop(std::mem::replace(&mut self.outgoing.sender, closed));
        let _ = tokio::time::timeout(Duration::from_secs(5), &mut self.tasks[0]).await;
    }
}

/// Read and write failures race during teardown. Keep the first detected cause
/// for both queued error delivery and producers whose writer has terminated.
fn remember(slot: &Mutex<Option<Failure>>, failure: Failure) -> Failure {
    slot.lock()
        .expect("terminal failure lock")
        .get_or_insert(failure)
        .clone()
}

fn stream_failure(
    error: &std::io::Error,
    phase: crate::peer_diagnostics::Phase,
    started: std::time::Instant,
) -> Failure {
    use crate::peer_diagnostics::Kind;
    let (kind, detail) = if crate::secure_channel::is_authentication_failure(error) {
        (
            Kind::Authentication,
            "the secure channel did not authenticate".into(),
        )
    } else {
        match error.kind() {
            std::io::ErrorKind::TimedOut => (
                Kind::Timeout,
                if phase == crate::peer_diagnostics::Phase::Send {
                    format!("nothing could be sent for {} seconds", GRACE.as_secs())
                } else {
                    format!("nothing arrived for {} seconds", GRACE.as_secs())
                },
            ),
            std::io::ErrorKind::UnexpectedEof => (Kind::Receive, "the connection closed".into()),
            std::io::ErrorKind::InvalidData => (Kind::Protocol, "invalid stream framing".into()),
            _ => (
                if phase == crate::peer_diagnostics::Phase::Send {
                    Kind::Send
                } else {
                    Kind::Receive
                },
                "the connection failed".into(),
            ),
        }
    };
    lost(&detail).with_details(serde_json::json!({"peer_stream":crate::remote_run::protocol::StreamFailure{kind,phase,duration_ms:started.elapsed().as_millis() as u64}}))
}

pub fn decode(bytes: &[u8]) -> Result<RunMessage> {
    crate::remote_run::protocol::decode(bytes).map_err(|_| {
        Failure::new(
            "protocol_error",
            "The other member sent an invalid run message.",
            "Install the same Elsecall version on every member.",
        )
    })
}

/// The workspace sync, carried inside the run's link.
pub struct SyncOverLink<'a>(pub &'a mut Link);

impl sync::Link for SyncOverLink<'_> {
    async fn send(&mut self, message: SyncMessage) -> Result<()> {
        self.0.message(&RunMessage::Sync(message)).await
    }
    async fn send_content(&mut self, chunk: Vec<u8>) -> Result<()> {
        self.0.send(Frame::Content(chunk)).await
    }
    async fn receive(&mut self) -> Result<Received> {
        match self.0.receive().await? {
            Frame::Content(chunk) => Ok(Received::Content(chunk)),
            Frame::Message(bytes) => match decode(&bytes)? {
                RunMessage::Sync(message) => Ok(Received::Message(message)),
                RunMessage::Failed { failure } => Err(failure),
                _ => Err(crate::workspace::sync_failed(
                    "The other side sent a run message during the sync.",
                )),
            },
            _ => Err(crate::workspace::sync_failed(
                "The other side sent output during the sync.",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn delivers_frames_and_reports_a_closed_peer_as_lost() {
        let (a, b) = tokio::io::duplex(1 << 16);
        let a = Link::open(a);
        let mut b = Link::open(b);
        a.message(&RunMessage::Done).await.unwrap();
        a.send(Frame::Stdout(b"x".to_vec())).await.unwrap();
        assert!(matches!(b.receive().await.unwrap(), Frame::Message(_)));
        assert_eq!(b.receive().await.unwrap(), Frame::Stdout(b"x".to_vec()));
        a.close().await;
        let failure = b.receive().await.unwrap_err();
        assert_eq!(failure.code, "connection_lost");
        assert_eq!(
            failure.details.as_ref().unwrap()["peer_stream"]["kind"],
            "receive"
        );
        assert_eq!(
            failure.details.as_ref().unwrap()["peer_stream"]["phase"],
            "receive"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn silence_past_the_grace_period_is_a_lost_connection() {
        // A peer that never writes, not even heartbeats.
        let (a, _silent) = tokio::io::duplex(1 << 16);
        let mut a = Link::open(a);
        let failure = a.receive().await.unwrap_err();
        assert_eq!(failure.code, "connection_lost");
        assert_eq!(
            failure.details.as_ref().unwrap()["peer_stream"]["kind"],
            "timeout"
        );
        assert!(
            failure.message.contains("10 seconds"),
            "{}",
            failure.message
        );
    }
}
