//! A pending read's inactivity budget, independent of frame completion. No
//! budget runs while the consumer is delivering a frame to its bounded queue.
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

pub(crate) struct Reader<R> {
    inner: R,
    grace: Duration,
    pending: Option<Pin<Box<Sleep>>>,
}

impl<R> Reader<R> {
    pub(crate) fn new(inner: R, grace: Duration) -> Self {
        Self {
            inner,
            grace,
            pending: None,
        }
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for Reader<R> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        // Ready bytes, EOF and I/O errors win over a delayed timer after this
        // task has been descheduled. The frame parser retains partial progress.
        match Pin::new(&mut this.inner).poll_read(cx, buf) {
            Poll::Ready(result) => {
                this.pending = None;
                Poll::Ready(result)
            }
            Poll::Pending => {
                let timer = this
                    .pending
                    .get_or_insert_with(|| Box::pin(tokio::time::sleep(this.grace)));
                if timer.as_mut().poll(cx).is_ready() {
                    Poll::Ready(Err(std::io::ErrorKind::TimedOut.into()))
                } else {
                    Poll::Pending
                }
            }
        }
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Reader<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tokio::io::AsyncReadExt;

    async fn poll_once<F: std::future::Future + Unpin>(future: &mut F) -> Poll<F::Output> {
        std::future::poll_fn(|cx| Poll::Ready(Pin::new(&mut *future).poll(cx))).await
    }

    #[tokio::test(start_paused = true)]
    async fn queued_socket_bytes_win_after_the_readiness_driver_was_delayed() {
        let (stream, peer) = tokio::net::UnixStream::pair().unwrap();
        let mut peer = peer.into_std().unwrap();
        let grace = Duration::from_secs(10);
        let mut reader = Reader::new(crate::socket_io::Socket::new(stream, grace), grace);
        let mut next = Box::pin(reader.read_u8());
        assert!(poll_once(&mut next).await.is_pending());
        tokio::time::advance(grace * 2).await;
        peer.write_all(&[42]).unwrap();
        assert!(matches!(poll_once(&mut next).await, Poll::Ready(Ok(42))));
    }

    #[tokio::test(start_paused = true)]
    async fn socket_eof_wins_after_the_readiness_driver_was_delayed() {
        let (stream, peer) = tokio::net::UnixStream::pair().unwrap();
        let grace = Duration::from_secs(10);
        let mut reader = Reader::new(crate::socket_io::Socket::new(stream, grace), grace);
        let mut next = Box::pin(reader.read_u8());
        assert!(poll_once(&mut next).await.is_pending());
        tokio::time::advance(grace * 2).await;
        drop(peer);
        assert!(matches!(
            poll_once(&mut next).await,
            Poll::Ready(Err(error)) if error.kind() == std::io::ErrorKind::UnexpectedEof
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn an_empty_socket_still_times_out_at_the_original_deadline() {
        let (stream, _peer) = tokio::net::UnixStream::pair().unwrap();
        let grace = Duration::from_secs(10);
        let mut reader = Reader::new(crate::socket_io::Socket::new(stream, grace), grace);
        let start = tokio::time::Instant::now();
        assert_eq!(
            reader.read_u8().await.unwrap_err().kind(),
            std::io::ErrorKind::TimedOut
        );
        assert_eq!(start.elapsed(), grace);
    }

    #[tokio::test(start_paused = true)]
    async fn a_zero_capacity_socket_read_completes_without_waiting_for_readiness() {
        let (stream, _peer) = tokio::net::UnixStream::pair().unwrap();
        let mut stream = crate::socket_io::Socket::new(stream, Duration::from_secs(10));
        let mut empty = [];
        let mut next = Box::pin(stream.read(&mut empty));
        assert!(matches!(poll_once(&mut next).await, Poll::Ready(Ok(0))));
    }

    #[tokio::test(start_paused = true)]
    async fn readable_sockets_preserve_cooperative_yields_before_the_deadline() {
        let (stream, peer) = tokio::net::UnixStream::pair().unwrap();
        let mut peer = peer.into_std().unwrap();
        peer.write_all(&[42; 1024]).unwrap();
        stream.readable().await.unwrap();
        let mut stream = crate::socket_io::Socket::new(stream, Duration::from_secs(10));
        for received in 0..1024 {
            let mut next = Box::pin(stream.read_u8());
            match poll_once(&mut next).await {
                Poll::Ready(Ok(42)) => (),
                Poll::Pending => {
                    assert!(received > 0);
                    return;
                }
                other => panic!("unexpected read: {other:?}"),
            }
        }
        panic!("readable socket bypassed every cooperative yield");
    }
}
