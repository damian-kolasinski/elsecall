//! One lock per member for the master's exchanges with it: pushes and plans to
//! the same member never overlap, while exchanges with different members proceed
//! independently, so a slow member delays only its own syncs.
use crate::roster::MemberName;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use tokio::sync::OwnedMutexGuard;

#[derive(Default)]
pub struct MemberLocks {
    /// One entry per member ever synced; a fleet has a handful.
    locks: Mutex<HashMap<MemberName, Arc<tokio::sync::Mutex<()>>>>,
}

/// Exchanges with the member wait until this is dropped.
pub struct MemberGuard {
    _held: OwnedMutexGuard<()>,
}

impl MemberLocks {
    /// Waits until no other exchange with `member` is in progress.
    pub async fn lock(&self, member: &MemberName) -> MemberGuard {
        let lock = self
            .locks
            .lock()
            .expect("member locks")
            .entry(member.clone())
            .or_default()
            .clone();
        MemberGuard {
            _held: lock.lock_owned().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn name(text: &str) -> MemberName {
        text.parse().unwrap()
    }

    #[tokio::test(start_paused = true)]
    async fn the_same_member_waits_and_another_does_not() {
        let locks = Arc::new(MemberLocks::default());
        let held = locks.lock(&name("alpha")).await;

        // Another member is not held up.
        tokio::time::timeout(Duration::from_millis(100), locks.lock(&name("bravo")))
            .await
            .expect("bravo is independent");

        // The same member waits until the first exchange ends.
        let alpha = name("alpha");
        let waiting = locks.lock(&alpha);
        tokio::pin!(waiting);
        std::future::poll_fn(|cx| {
            assert!(
                waiting.as_mut().poll(cx).is_pending(),
                "alpha's second exchange waits"
            );
            std::task::Poll::Ready(())
        })
        .await;
        drop(held);
        tokio::time::timeout(Duration::from_millis(500), waiting)
            .await
            .expect("it proceeds once the first ends");
    }
}
