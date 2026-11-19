//! Learned speed ratios, kept for a minute. Learning them reads the latest
//! completed runs of every label, which is too much to read for every
//! placeable request, and a ratio between two members moves far more slowly
//! than requests arrive.
use crate::scheduling::learning::ratios::SpeedRatios;
use std::{sync::Mutex, time::Duration};
use tokio::time::Instant;

/// How long learned ratios are used before they are learned again.
pub const KEEP_FOR: Duration = Duration::from_secs(60);

#[derive(Default)]
pub struct RatioCache {
    kept: Mutex<Option<(Instant, SpeedRatios)>>,
}

impl RatioCache {
    /// The ratios learned within [`KEEP_FOR`] before `now`, else `learn`'s.
    pub fn get(&self, now: Instant, learn: impl FnOnce() -> SpeedRatios) -> SpeedRatios {
        let mut kept = self.kept.lock().unwrap_or_else(|e| e.into_inner());
        match &*kept {
            Some((at, ratios)) if now.saturating_duration_since(*at) < KEEP_FOR => ratios.clone(),
            _ => {
                let ratios = learn();
                *kept = Some((now, ratios.clone()));
                ratios
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduling::learning::tests::{defaults, labelled};

    #[test]
    fn ratios_are_learned_again_once_a_minute() {
        let cache = RatioCache::default();
        let start = Instant::now();
        let learned = SpeedRatios::learn(
            &(0..3)
                .flat_map(|i| {
                    [
                        labelled("a", None, "mini", i, 10.0, false, None),
                        labelled("a", None, "devbox", i, 20.0, false, None),
                    ]
                })
                .collect::<Vec<_>>(),
            &defaults(),
        );
        assert_eq!(cache.get(start, || learned.clone()), learned);
        assert_eq!(
            cache.get(start + Duration::from_secs(59), SpeedRatios::default),
            learned,
            "kept"
        );
        assert_eq!(
            cache.get(start + KEEP_FOR, SpeedRatios::default),
            SpeedRatios::default(),
            "learned again"
        );
    }
}
