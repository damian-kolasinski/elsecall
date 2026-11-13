//! TCP connection and HTTP I/O progress. Tailnet roster endpoints are numeric
//! addresses, so this transport does not do DNS or TLS. Identity is signed above it.
use crate::{
    failure::Result,
    peer_client::unavailable,
    peer_diagnostics::{Kind, Phase},
    roster::Member,
};
use std::{
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::TcpStream,
};
#[derive(Clone)]
pub struct Progress(Arc<AtomicU8>);
impl Default for Progress {
    fn default() -> Self {
        Self(Arc::new(AtomicU8::new(0)))
    }
}
impl Progress {
    pub fn set(&self, phase: Phase) {
        self.0.store(
            match phase {
                Phase::Connect => 0,
                Phase::Authentication => 1,
                Phase::Send => 2,
                Phase::Receive => 3,
                Phase::Request => 4,
            },
            Ordering::Relaxed,
        );
    }
    pub fn phase(&self) -> Phase {
        match self.0.load(Ordering::Relaxed) {
            0 => Phase::Connect,
            1 => Phase::Authentication,
            2 => Phase::Send,
            4 => Phase::Request,
            _ => Phase::Receive,
        }
    }
    pub fn kind(&self) -> Kind {
        match self.phase() {
            Phase::Connect => Kind::Connect,
            Phase::Authentication => Kind::Authentication,
            Phase::Send => Kind::Send,
            Phase::Receive => Kind::Receive,
            Phase::Request => Kind::Timeout,
        }
    }
}
/// Every endpoint gets its own small connect budget; a stale first address
/// cannot consume the entire request before a usable alternate is tried.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(1);
pub async fn connect(
    member: &Member,
    progress: &Progress,
) -> Result<(Tracked<TcpStream>, std::net::SocketAddr)> {
    let mut timed_out = false;
    for endpoint in &member.endpoints {
        match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(endpoint)).await {
            Ok(Ok(stream)) => {
                let _ = stream.set_nodelay(true);
                return Ok((
                    Tracked {
                        inner: stream,
                        progress: progress.clone(),
                    },
                    *endpoint,
                ));
            }
            Err(_) => timed_out = true,
            Ok(Err(_)) => (),
        }
    }
    Err(if timed_out {
        unavailable(member).with_details(serde_json::json!({"peer_timeout":true}))
    } else {
        unavailable(member)
    })
}
pub struct Tracked<S> {
    inner: S,
    progress: Progress,
}
impl<S: std::os::fd::AsRawFd> std::os::fd::AsRawFd for Tracked<S> {
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        self.inner.as_raw_fd()
    }
}
impl<S: AsyncRead + Unpin> AsyncRead for Tracked<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        let result = Pin::new(&mut this.inner).poll_read(cx, buf);
        if buf.filled().len() > before {
            this.progress.set(Phase::Receive);
        }
        result
    }
}
impl<S: AsyncWrite + Unpin> AsyncWrite for Tracked<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        this.progress.set(Phase::Send);
        Pin::new(&mut this.inner).poll_write(cx, buf)
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let result = Pin::new(&mut this.inner).poll_flush(cx);
        if matches!(result, Poll::Ready(Ok(()))) {
            this.progress.set(Phase::Receive);
        }
        result
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

/// Sealed bulk channels outlive their HTTP handshake. Result traffic uses a
/// ten-second inactivity bound (heartbeats arrive every second); configuration
/// sync uses a whole-exchange bound of 120 seconds, including commit/application.
/// No stream operation is retried: commands and commits are not idempotent.
#[derive(Clone, Copy)]
pub enum StreamTimeout {
    Inactivity(Duration),
    Whole(Duration),
    Caller,
}
pub fn stream_timeout(protocol: &str) -> StreamTimeout {
    match protocol {
        "elsecall-run/1" => StreamTimeout::Inactivity(Duration::from_secs(10)),
        "elsecall-sync/1" => StreamTimeout::Whole(Duration::from_secs(120)),
        _ => StreamTimeout::Caller,
    }
}
pub struct Observed<S> {
    inner: S,
    member: String,
    path: String,
    started: std::time::Instant,
    read_deadline: Pin<Box<tokio::time::Sleep>>,
    write_deadline: Pin<Box<tokio::time::Sleep>>,
    timeout: StreamTimeout,
    failed: bool,
}
impl<S> Observed<S> {
    pub fn new(inner: S, member: &str, path: &str, timeout: StreamTimeout) -> Self {
        let duration = match timeout {
            StreamTimeout::Inactivity(d) | StreamTimeout::Whole(d) => d,
            StreamTimeout::Caller => Duration::ZERO,
        };
        Self {
            inner,
            member: member.into(),
            path: path.into(),
            started: std::time::Instant::now(),
            read_deadline: Box::pin(tokio::time::sleep(duration)),
            write_deadline: Box::pin(tokio::time::sleep(duration)),
            timeout,
            failed: false,
        }
    }
    fn failure(&mut self, phase: Phase, error: &std::io::Error) {
        if self.failed {
            return;
        }
        self.failed = true;
        let kind = match error.kind() {
            std::io::ErrorKind::TimedOut => Kind::Timeout,
            std::io::ErrorKind::InvalidData => Kind::Authentication,
            _ => match phase {
                Phase::Send => Kind::Send,
                _ => Kind::Receive,
            },
        };
        let attempt = crate::peer_diagnostics::Attempt::new(
            &self.member,
            &self.path,
            1,
            phase,
            Some(kind),
            self.started,
        );
        crate::peer_diagnostics::record(&attempt);
    }
    fn progress(&mut self, phase: Phase) {
        if let StreamTimeout::Inactivity(duration) = self.timeout {
            let deadline = if phase == Phase::Send {
                &mut self.write_deadline
            } else {
                &mut self.read_deadline
            };
            deadline
                .as_mut()
                .reset(tokio::time::Instant::now() + duration);
        }
    }
    fn pending(&mut self, cx: &mut Context<'_>, phase: Phase) -> Poll<std::io::Result<()>> {
        use std::future::Future;
        if matches!(self.timeout, StreamTimeout::Caller) {
            return Poll::Pending;
        }
        let deadline = if phase == Phase::Send {
            &mut self.write_deadline
        } else {
            &mut self.read_deadline
        };
        if deadline.as_mut().poll(cx).is_ready() {
            let error = std::io::Error::from(std::io::ErrorKind::TimedOut);
            self.failure(phase, &error);
            Poll::Ready(Err(error))
        } else {
            Poll::Pending
        }
    }
}
impl<S: AsyncRead + Unpin> AsyncRead for Observed<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        match Pin::new(&mut this.inner).poll_read(cx, buf) {
            Poll::Pending => this.pending(cx, Phase::Receive),
            Poll::Ready(Err(e)) => {
                this.failure(Phase::Receive, &e);
                Poll::Ready(Err(e))
            }
            Poll::Ready(Ok(())) => {
                if buf.filled().len() > before {
                    this.progress(Phase::Receive);
                } else if matches!(this.timeout, StreamTimeout::Whole(_)) && buf.remaining() > 0 {
                    this.failure(Phase::Receive, &std::io::ErrorKind::UnexpectedEof.into());
                }
                Poll::Ready(Ok(()))
            }
        }
    }
}
impl<S: AsyncWrite + Unpin> AsyncWrite for Observed<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_write(cx, buf) {
            Poll::Pending => this.pending(cx, Phase::Send).map_ok(|_| 0),
            Poll::Ready(Err(e)) => {
                this.failure(Phase::Send, &e);
                Poll::Ready(Err(e))
            }
            Poll::Ready(Ok(n)) => {
                if n > 0 {
                    this.progress(Phase::Send);
                }
                Poll::Ready(Ok(n))
            }
        }
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_flush(cx) {
            Poll::Pending => this.pending(cx, Phase::Send),
            Poll::Ready(Err(e)) => {
                this.failure(Phase::Send, &e);
                Poll::Ready(Err(e))
            }
            done => done,
        }
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    struct Broken;
    impl AsyncRead for Broken {
        fn poll_read(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            _: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            Poll::Ready(Err(std::io::ErrorKind::ConnectionReset.into()))
        }
    }
    impl AsyncWrite for Broken {
        fn poll_write(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            _: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()))
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }
    #[tokio::test]
    async fn injected_send_receive_and_authenticated_stream_failures_retain_the_io_phase() {
        let progress = Progress::default();
        let mut tracked = Tracked {
            inner: Broken,
            progress: progress.clone(),
        };
        assert!(tracked.write_all(b"request").await.is_err());
        assert_eq!(progress.phase(), Phase::Send);
        assert_eq!(progress.kind(), Kind::Send);
        let mut observed = Observed::new(
            Broken,
            "peer",
            "/peer/v1/placements/r-000000000001/attach",
            stream_timeout("elsecall-run/1"),
        );
        assert!(observed.read_u8().await.is_err());
        assert!(observed.failed);
    }
    #[tokio::test(start_paused = true)]
    async fn long_channels_use_inactivity_or_whole_sync_bounds_instead_of_two_seconds() {
        let (stream, _held) = tokio::io::duplex(64);
        let mut result = Observed::new(
            stream,
            "peer",
            "/peer/v1/placements/r-000000000001/attach",
            stream_timeout("elsecall-run/1"),
        );
        let start = tokio::time::Instant::now();
        assert_eq!(
            result.read_u8().await.unwrap_err().kind(),
            std::io::ErrorKind::TimedOut
        );
        assert_eq!(start.elapsed(), Duration::from_secs(10));
        assert!(result.failed);
        let (stream, _held) = tokio::io::duplex(64);
        let mut sync = Observed::new(
            stream,
            "peer",
            "/peer/v1/config-sync/attach",
            stream_timeout("elsecall-sync/1"),
        );
        let start = tokio::time::Instant::now();
        assert_eq!(
            sync.read_u8().await.unwrap_err().kind(),
            std::io::ErrorKind::TimedOut
        );
        assert_eq!(start.elapsed(), Duration::from_secs(120));
    }
    #[tokio::test(start_paused = true)]
    async fn protocols_with_their_own_budgets_can_upload_and_stage_for_more_than_ten_seconds() {
        let (stream, mut peer) = tokio::io::duplex(64);
        let mut channel = Observed::new(
            stream,
            "peer",
            "/peer/v1/upgrade/attach",
            stream_timeout("elsecall-upgrade/1"),
        );
        let task = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(12)).await;
            let mut upload = [0; 128];
            peer.read_exact(&mut upload).await.unwrap();
            tokio::time::sleep(Duration::from_secs(12)).await;
            peer.write_all(b"s").await.unwrap();
        });
        let start = tokio::time::Instant::now();
        channel.write_all(&[7; 128]).await.unwrap();
        assert_eq!(channel.read_u8().await.unwrap(), b's');
        assert_eq!(start.elapsed(), Duration::from_secs(24));
        task.await.unwrap();
    }
}
