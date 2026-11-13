//! Latch the first controller signal across preparation and both A/B runs.
use crate::failure::{Failure, Result};
pub struct Signals {
    receiver: tokio::sync::watch::Receiver<Option<&'static str>>,
    task: tokio::task::AbortHandle,
}
impl Signals {
    pub fn register() -> Result<Self> {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .map_err(|e| Failure::io("Register rehearsal termination", e))?;
        let mut interrupt =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
                .map_err(|e| Failure::io("Register rehearsal interruption", e))?;
        let (sender, receiver) = tokio::sync::watch::channel(None);
        let task = tokio::spawn(async move {
            let reason =
                tokio::select! {_=terminate.recv()=>"terminated",_=interrupt.recv()=>"interrupted"};
            let _ = sender.send(Some(reason));
        })
        .abort_handle();
        Ok(Self { receiver, task })
    }
    pub async fn cancelled(&mut self) -> &'static str {
        loop {
            if let Some(reason) = *self.receiver.borrow() {
                return reason;
            }
            if self.receiver.changed().await.is_err() {
                return "interrupted";
            }
        }
    }
}
impl Drop for Signals {
    fn drop(&mut self) {
        self.task.abort();
    }
}
