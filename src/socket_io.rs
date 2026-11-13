use std::{
    io,
    os::fd::{AsRawFd, RawFd},
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

pub(crate) struct Socket<S> {
    stream: S,
    grace: Duration,
    pending_since: Option<tokio::time::Instant>,
}

impl<S> Socket<S> {
    pub(crate) fn new(stream: S, grace: Duration) -> Self {
        Self {
            stream,
            grace,
            pending_since: None,
        }
    }
}

impl<S: AsRawFd> AsRawFd for Socket<S> {
    fn as_raw_fd(&self) -> RawFd {
        self.stream.as_raw_fd()
    }
}

impl<S: AsyncRead + AsRawFd + Unpin> AsyncRead for Socket<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        match Pin::new(&mut this.stream).poll_read(cx, buf) {
            Poll::Pending => {
                let since = this
                    .pending_since
                    .get_or_insert_with(tokio::time::Instant::now);
                if since.elapsed() < this.grace {
                    return Poll::Pending;
                }
                let target = buf.initialize_unfilled();
                loop {
                    // The owned socket remains open; MSG_DONTWAIT never parks
                    // the runtime, and the initialized slice bounds the write.
                    let received = unsafe {
                        libc::recv(
                            this.stream.as_raw_fd(),
                            target.as_mut_ptr().cast(),
                            target.len(),
                            libc::MSG_DONTWAIT,
                        )
                    };
                    if received >= 0 {
                        this.pending_since = None;
                        buf.advance(received as usize);
                        return Poll::Ready(Ok(()));
                    }
                    let error = io::Error::last_os_error();
                    match error.kind() {
                        io::ErrorKind::Interrupted => continue,
                        io::ErrorKind::WouldBlock => return Poll::Pending,
                        _ => {
                            this.pending_since = None;
                            return Poll::Ready(Err(error));
                        }
                    }
                }
            }
            ready => {
                this.pending_since = None;
                ready
            }
        }
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Socket<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().stream).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().stream).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().stream).poll_shutdown(cx)
    }
}
