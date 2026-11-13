//! A pending raw write's byte-progress budget. Writes are observed below record
//! sealing; incoming heartbeats cannot renew a stuck send operation.
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    time::Sleep,
};
pub(crate) struct Writer<S> {
    inner: S,
    grace: Duration,
    pending: Option<Pin<Box<Sleep>>>,
}
impl<S> Writer<S> {
    pub(crate) fn new(inner: S, grace: Duration) -> Self {
        Self {
            inner,
            grace,
            pending: None,
        }
    }
    fn waiting(&mut self, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let timer = self
            .pending
            .get_or_insert_with(|| Box::pin(tokio::time::sleep(self.grace)));
        if timer.as_mut().poll(cx).is_ready() {
            Poll::Ready(Err(std::io::ErrorKind::TimedOut.into()))
        } else {
            Poll::Pending
        }
    }
}
impl<S: AsyncRead + Unpin> AsyncRead for Writer<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_read(cx, buf)
    }
}
impl<S: AsyncWrite + Unpin> AsyncWrite for Writer<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_write(cx, buf) {
            Poll::Pending => this.waiting(cx).map_ok(|_| 0),
            Poll::Ready(result) => {
                if matches!(result, Ok(1..)) {
                    this.pending = None;
                }
                Poll::Ready(result)
            }
        }
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_flush(cx) {
            Poll::Pending => this.waiting(cx),
            Poll::Ready(result) => {
                this.pending = None;
                Poll::Ready(result)
            }
        }
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_shutdown(cx) {
            Poll::Pending => this.waiting(cx),
            Poll::Ready(result) => {
                this.pending = None;
                Poll::Ready(result)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    #[tokio::test(start_paused = true)]
    async fn idle_time_does_not_spend_the_first_pending_write_budget() {
        let (mut inner, _peer) = tokio::io::duplex(1);
        inner.write_u8(1).await.unwrap();
        let mut stream = Writer::new(inner, Duration::from_secs(10));
        tokio::time::sleep(Duration::from_secs(30)).await;
        let start = tokio::time::Instant::now();
        assert_eq!(
            stream.write_u8(2).await.unwrap_err().kind(),
            std::io::ErrorKind::TimedOut
        );
        assert_eq!(start.elapsed(), Duration::from_secs(10));
    }
    #[tokio::test(start_paused = true)]
    async fn positive_raw_writes_can_take_longer_than_the_grace_period() {
        let (stream, mut peer) = tokio::io::duplex(1);
        let mut stream = Writer::new(stream, Duration::from_secs(10));
        let receiver = tokio::spawn(async move {
            for _ in 0..16 {
                tokio::time::sleep(Duration::from_secs(1)).await;
                assert_eq!(peer.read_u8().await.unwrap(), 7);
            }
        });
        let start = tokio::time::Instant::now();
        stream.write_all(&[7; 16]).await.unwrap();
        assert!(start.elapsed() > Duration::from_secs(10));
        receiver.await.unwrap();
    }
    #[tokio::test(start_paused = true)]
    async fn receive_progress_cannot_renew_a_stuck_send() {
        let (stream, mut peer) = tokio::io::duplex(1);
        let stream = Writer::new(stream, Duration::from_secs(10));
        let (mut read, mut write) = tokio::io::split(stream);
        let heartbeats = tokio::spawn(async move {
            loop {
                peer.write_all(&[1]).await.unwrap();
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });
        let drain = tokio::spawn(async move { while read.read_u8().await.is_ok() {} });
        let start = tokio::time::Instant::now();
        assert_eq!(
            write.write_all(&[7; 16]).await.unwrap_err().kind(),
            std::io::ErrorKind::TimedOut
        );
        assert_eq!(start.elapsed(), Duration::from_secs(10));
        heartbeats.abort();
        drain.abort();
    }
}
