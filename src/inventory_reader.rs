//! At most one read of this machine's installed tools is in flight.
//!
//! The local route, the fleet route's own-member adapter and the peer route
//! share it. A caller that disconnects does not start another read: the read
//! was started aside from that caller, finishes, and its result is delivered to
//! whoever is still waiting. Nothing is kept once that read has finished.
use crate::tool_inventory::Inventory;
use std::sync::{Arc, Mutex};
use tokio::sync::watch;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadFailed;

struct Flight<T> {
    id: u64,
    rx: watch::Receiver<Option<Result<T, ReadFailed>>>,
}

struct Slot<T> {
    next: u64,
    flight: Option<Flight<T>>,
}

struct SharedRead<T> {
    inner: Arc<Mutex<Slot<T>>>,
}

impl<T> SharedRead<T>
where
    T: Clone + Send + Sync + 'static,
{
    fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Slot {
                next: 1,
                flight: None,
            })),
        }
    }

    async fn run<F>(&self, read: F) -> Result<T, ReadFailed>
    where
        F: FnOnce() -> T + Send + 'static,
    {
        let mut rx = self.subscribe(read);
        loop {
            if let Some(result) = rx.borrow().clone() {
                return result;
            }
            if rx.changed().await.is_err() {
                return rx.borrow().clone().unwrap_or(Err(ReadFailed));
            }
        }
    }

    fn subscribe<F>(&self, read: F) -> watch::Receiver<Option<Result<T, ReadFailed>>>
    where
        F: FnOnce() -> T + Send + 'static,
    {
        let mut slot = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(flight) = &slot.flight
            && flight.rx.borrow().is_none()
        {
            return flight.rx.clone();
        }
        let id = slot.next;
        slot.next += 1;
        let (tx, rx) = watch::channel(None);
        slot.flight = Some(Flight { id, rx: rx.clone() });
        drop(slot);
        let inner = Arc::clone(&self.inner);
        tokio::spawn(async move {
            let result = match tokio::task::spawn_blocking(read).await {
                Ok(value) => Ok(value),
                Err(_) => Err(ReadFailed),
            };
            let _ = tx.send(Some(result));
            let mut slot = inner.lock().unwrap_or_else(|error| error.into_inner());
            if slot.flight.as_ref().is_some_and(|flight| flight.id == id) {
                slot.flight = None;
            }
        });
        rx
    }
}

pub async fn installed() -> Result<Inventory, ReadFailed> {
    reader()
        .run(crate::fleet_tool_inventory::read_installed)
        .await
}

fn reader() -> &'static SharedRead<Inventory> {
    static READER: std::sync::OnceLock<SharedRead<Inventory>> = std::sync::OnceLock::new();
    READER.get_or_init(SharedRead::new)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn two_callers_share_one_read_and_the_next_read_runs_again() {
        let shared = SharedRead::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let first_calls = Arc::clone(&calls);
        let second_calls = Arc::clone(&calls);
        let first = shared.run(move || {
            first_calls.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(80));
            1
        });
        let second = shared.run(move || {
            second_calls.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(80));
            1
        });
        let (first, second) = tokio::join!(first, second);
        assert_eq!(first.unwrap(), 1);
        assert_eq!(second.unwrap(), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let third_calls = Arc::clone(&calls);
        let third = shared
            .run(move || {
                third_calls.fetch_add(1, Ordering::SeqCst);
                2
            })
            .await
            .unwrap();
        assert_eq!(third, 2);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }
}
