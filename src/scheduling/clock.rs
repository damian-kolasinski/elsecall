//! Time at the scheduling boundary, shared by deadlines, records and learning.
use crate::scheduling::timestamp::Timestamp;

pub trait Clock: Send + Sync {
    fn now(&self) -> Timestamp;
}

pub struct SystemClock;
impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        Timestamp::now()
    }
}

/// Epoch time advanced by Tokio's monotonic clock. On a paused current-thread
/// runtime it advances only when the virtual executor advances time.
pub struct VirtualClock {
    epoch: Timestamp,
    start: tokio::time::Instant,
}
impl VirtualClock {
    pub fn new(epoch: Timestamp) -> Self {
        Self {
            epoch,
            start: tokio::time::Instant::now(),
        }
    }
}
impl Clock for VirtualClock {
    fn now(&self) -> Timestamp {
        self.epoch.after(self.start.elapsed())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test(start_paused = true)]
    async fn virtual_time_advances_without_wall_time_and_preserves_epoch() {
        let clock = VirtualClock::new(Timestamp::from_millis(1_000));
        assert_eq!(clock.now().millis(), 1_000);
        tokio::time::advance(Duration::from_millis(250)).await;
        assert_eq!(clock.now().millis(), 1_250);
        tokio::task::yield_now().await;
        assert_eq!(clock.now().millis(), 1_250);
        tokio::time::advance(Duration::from_secs(24 * 60 * 60)).await;
        assert_eq!(clock.now().millis(), 86_401_250);
    }
}
