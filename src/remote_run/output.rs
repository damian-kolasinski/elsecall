//! Flow-controlled output draining. Only the pipe reader can establish quiet:
//! elapsed time while a reader is waiting for CPU says nothing about its pipe.
use crate::{
    remote_run::{
        link::Outbox,
        protocol::{Stream, WINDOW},
    },
    wire::Frame,
};
use std::{
    os::fd::AsRawFd,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{io::AsyncReadExt, net::unix::pipe, sync::Notify};

const DRAIN_QUIET: Duration = Duration::from_secs(1);

#[derive(Default)]
pub(super) struct Window {
    unacknowledged: AtomicU64,
    acknowledged: Notify,
    exited: AtomicBool,
    exit: Notify,
    drained: AtomicBool,
}
impl Window {
    pub fn acknowledge(&self, bytes: u64) {
        self.unacknowledged.fetch_sub(
            bytes.min(self.unacknowledged.load(Ordering::SeqCst)),
            Ordering::SeqCst,
        );
        self.acknowledged.notify_one();
    }
    pub fn exited(&self) {
        self.exited.store(true, Ordering::SeqCst);
        self.exit.notify_one();
    }
    pub fn drained(&self) -> bool {
        self.drained.load(Ordering::SeqCst)
    }
}

pub(super) async fn read(
    stream: Stream,
    mut receiver: pipe::Receiver,
    window: Arc<Window>,
    outbox: Outbox,
) {
    let mut buffer = vec![0u8; 64 * 1024];
    if pass(stream, &mut receiver, &mut buffer, &window, outbox).await {
        return;
    }
    // Release the link before reporting quiet. Leftovers retain their open pipe,
    // as with a local command whose terminal remains open, until they end.
    window.drained.store(true, Ordering::SeqCst);
    while let Ok(1..) = receiver.read(&mut buffer).await {}
}

/// True on EOF, false on quiet after exit or a lost link.
async fn pass(
    stream: Stream,
    receiver: &mut pipe::Receiver,
    buffer: &mut [u8],
    window: &Window,
    outbox: Outbox,
) -> bool {
    loop {
        while window.unacknowledged.load(Ordering::SeqCst) >= WINDOW {
            let acknowledged = window.acknowledged.notified();
            if window.unacknowledged.load(Ordering::SeqCst) < WINDOW {
                break;
            }
            acknowledged.await;
        }
        let result = if window.exited.load(Ordering::SeqCst) {
            tokio::select! {
                // Always poll the pipe before an expired timer: if descheduled,
                // drain ready bytes even when the quiet duration has passed.
                biased;
                result = receiver.read(buffer) => result,
                _ = tokio::time::sleep(DRAIN_QUIET) => {
                    // Readiness delivery may itself be delayed. Inspect the
                    // nonblocking pipe before calling it quiet, even if Tokio
                    // has not delivered its readable event yet.
                    match read_now(receiver, buffer) {
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return false,
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        result => result,
                    }
                },
            }
        } else {
            tokio::select! {
                biased;
                result = receiver.read(buffer) => result,
                _ = window.exit.notified() => continue,
            }
        };
        let n = match result {
            Ok(0) | Err(_) => return true,
            Ok(n) => n,
        };
        window.unacknowledged.fetch_add(n as u64, Ordering::SeqCst);
        let chunk = buffer[..n].to_vec();
        let frame = match stream {
            Stream::Stderr => Frame::Stderr(chunk),
            _ => Frame::Stdout(chunk),
        };
        if outbox.send(frame).await.is_err() {
            return false;
        }
    }
}

/// The receiver owns a nonblocking descriptor and this task is its only reader.
fn read_now(receiver: &pipe::Receiver, buffer: &mut [u8]) -> std::io::Result<usize> {
    // SAFETY: the descriptor is open and the buffer is valid for its full length.
    let n = unsafe {
        libc::read(
            receiver.as_raw_fd(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
        )
    };
    if n < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(n as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote_run::link::Link;

    #[tokio::test(start_paused = true)]
    async fn a_descheduled_reader_drains_ready_output_before_declaring_quiet() {
        for (stream, delayed_first_poll) in [
            (Stream::Stdout, false),
            (Stream::Stderr, false),
            (Stream::Stdout, true),
            (Stream::Stderr, true),
        ] {
            let (mut writer, receiver) = pipe::pipe().unwrap();
            let (member, caller) = tokio::io::duplex(1 << 20);
            let member = Link::open(member);
            let mut caller = Link::open(caller);
            let window = Arc::new(Window::default());
            window.exited();
            let reader = read(stream, receiver, window.clone(), member.outbox());
            tokio::pin!(reader);
            // First poll establishes an empty pipe and a quiet timer. Keep the
            // writer open as a leftover would, then suspend the reader while
            // bytes arrive and that timer expires.
            if !delayed_first_poll {
                std::future::poll_fn(|cx| {
                    assert!(reader.as_mut().poll(cx).is_pending());
                    std::task::Poll::Ready(())
                })
                .await;
            }
            use tokio::io::AsyncWriteExt;
            writer.write_all(b"kept after exit").await.unwrap();
            tokio::time::advance(DRAIN_QUIET * 2).await;
            std::future::poll_fn(|cx| {
                assert!(reader.as_mut().poll(cx).is_pending());
                std::task::Poll::Ready(())
            })
            .await;
            assert!(!window.drained(), "buffered output must be forwarded first");
            loop {
                match caller.receive().await.unwrap() {
                    Frame::Stdout(bytes) if stream == Stream::Stdout => {
                        assert_eq!(bytes, b"kept after exit");
                        break;
                    }
                    Frame::Stderr(bytes) if stream == Stream::Stderr => {
                        assert_eq!(bytes, b"kept after exit");
                        break;
                    }
                    Frame::Heartbeat => (),
                    other => panic!("unexpected frame {other:?}"),
                }
            }
            tokio::time::advance(DRAIN_QUIET * 2).await;
            std::future::poll_fn(|cx| {
                assert!(reader.as_mut().poll(cx).is_pending());
                std::task::Poll::Ready(())
            })
            .await;
            assert!(
                window.drained(),
                "an open, empty pipe becomes quiet after exit"
            );
        }
    }
}
