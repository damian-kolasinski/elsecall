//! Sync traffic as frames on a byte stream, without heartbeats: for streams whose
//! liveness something else watches, and for tests.
use crate::{
    failure::{Failure, Result},
    wire,
    workspace::sync::{Link, Received, SyncMessage, unexpected},
};

pub struct FramedLink<R, W> {
    pub reader: R,
    pub writer: W,
}

fn lost(error: std::io::Error) -> Failure {
    Failure::new(
        "connection_lost",
        format!("The connection to the other side was lost: {error}."),
        "Retry; check that the member is reachable with elsecall fleet.",
    )
}

impl<R, W> Link for FramedLink<R, W>
where
    R: tokio::io::AsyncRead + Unpin + Send,
    W: tokio::io::AsyncWrite + Unpin + Send,
{
    async fn send(&mut self, message: SyncMessage) -> Result<()> {
        let bytes = serde_json::to_vec(&message).expect("messages serialize");
        wire::write(&mut self.writer, &wire::Frame::Message(bytes))
            .await
            .map_err(lost)
    }
    async fn send_content(&mut self, chunk: Vec<u8>) -> Result<()> {
        wire::write(&mut self.writer, &wire::Frame::Content(chunk))
            .await
            .map_err(lost)
    }
    async fn receive(&mut self) -> Result<Received> {
        loop {
            match wire::read(&mut self.reader).await.map_err(lost)? {
                Some(wire::Frame::Message(bytes)) => {
                    return serde_json::from_slice(&bytes)
                        .map(Received::Message)
                        .map_err(|_| unexpected());
                }
                Some(wire::Frame::Content(chunk)) => return Ok(Received::Content(chunk)),
                Some(wire::Frame::Heartbeat) => continue,
                Some(_) => return Err(unexpected()),
                None => return Err(lost(std::io::ErrorKind::UnexpectedEof.into())),
            }
        }
    }
}
