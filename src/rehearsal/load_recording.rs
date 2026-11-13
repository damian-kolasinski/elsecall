//! Bounded session load capture. Normal minute totals cannot prove in-window
//! coverage because they also include observations before a rehearsal began.
use crate::{
    failure::Result,
    load_log::{buckets::Bucket, minute::Second},
    scheduling::timestamp::Timestamp,
};
use serde::{Deserialize, Serialize};
mod summary;
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Recorded {
    pub load: Vec<Bucket>,
    pub complete: bool,
}
#[derive(Default)]
pub struct Recorder {
    latest: Option<Second>,
    session: Option<(String, Vec<Second>)>,
}
impl Recorder {
    pub fn observe(&mut self, sample: &Second) {
        if let Some((_, samples)) = &mut self.session
            && samples.len() < 3663
            && samples.last().is_none_or(|last| last.at < sample.at)
        {
            samples.push(sample.clone());
        }
        self.latest = Some(sample.clone());
    }
    pub fn ready(&self, now: Timestamp) -> bool {
        self.latest.as_ref().is_some_and(|s| {
            now.since(s.at).as_millis() <= 2000 && s.cpu_utilization.is_some() && s.speed.is_some()
        })
    }
    pub fn begin(&mut self, session: String) {
        self.session = Some((session, self.latest.iter().cloned().collect()));
    }
    pub fn read(&self, session: &str, since: Timestamp, until: Timestamp) -> Result<Recorded> {
        let samples = &self
            .session
            .as_ref()
            .filter(|(id, _)| id == session)
            .ok_or_else(|| super::invalid("Rehearsal load recording is unavailable."))?
            .1;
        let initial = samples.iter().rev().find(|s| s.at <= since);
        let selected = initial
            .into_iter()
            .chain(samples.iter().filter(|s| s.at > since && s.at < until))
            .collect::<Vec<_>>();
        let valid = selected
            .iter()
            .copied()
            .filter(|s| s.cpu_utilization.is_some() && s.speed.is_some())
            .collect::<Vec<_>>();
        let (load, observed_ms) = summary::buckets(&selected, since, until);
        let complete = initial.is_some_and(|s| {
            s.cpu_utilization.is_some()
                && s.speed.is_some()
                && since.since(s.at).as_millis() <= 2000
        }) && valid
            .windows(2)
            .all(|p| p[1].at.since(p[0].at).as_millis() <= 2000)
            && valid
                .last()
                .is_some_and(|s| until.since(s.at).as_millis() <= 2000)
            && observed_ms * 10 >= until.since(since).as_millis() as u64 * 9;
        Ok(Recorded { load, complete })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        inspection::{MemoryPressure, ThermalState},
        scheduling::{capacity::Cores, load::Gate},
    };
    fn sample(at: u64, queued: usize) -> Second {
        Second {
            at: Timestamp::from_millis(at),
            cpu_utilization: Some(0.1),
            memory_pressure: Some(MemoryPressure::Normal),
            thermal_state: Some(ThermalState::Nominal),
            weighted_cores: Cores::ONE,
            capacity: Cores::ONE,
            in_use: Cores::ZERO,
            queued,
            queued_interactive: queued,
            gate: Gate::Open,
            speed: Some(1.0),
        }
    }
    #[test]
    fn prewindow_samples_cannot_hide_missing_session_observations() {
        let mut recorder = Recorder::default();
        for at in (0..50_000).step_by(1000) {
            recorder.observe(&sample(at, 7));
        }
        recorder.begin("session".into());
        let result = recorder
            .read(
                "session",
                Timestamp::from_millis(50_000),
                Timestamp::from_millis(60_000),
            )
            .unwrap();
        assert!(!result.complete);
        for at in (50_000..60_000).step_by(1000) {
            recorder.observe(&sample(at, 0));
        }
        let result = recorder
            .read(
                "session",
                Timestamp::from_millis(50_000),
                Timestamp::from_millis(60_000),
            )
            .unwrap();
        assert!(result.complete);
        assert_eq!(result.load[0].queued_max, 0);
        assert_eq!(result.load[0].seconds, 10);
    }
    #[test]
    fn duplicate_samples_cannot_inflate_coverage_or_minute_seconds() {
        let mut recorder = Recorder::default();
        recorder.observe(&sample(50_000, 0));
        recorder.begin("session".into());
        for at in [50_000, 52_000, 54_000] {
            for _ in 0..100 {
                recorder.observe(&sample(at, 0));
            }
        }
        let result = recorder
            .read(
                "session",
                Timestamp::from_millis(50_000),
                Timestamp::from_millis(55_000),
            )
            .unwrap();
        assert!(!result.complete);
        assert_eq!(result.load[0].seconds, 3);
    }
    #[test]
    fn clipped_observation_covers_a_short_tail_in_the_next_utc_minute() {
        let mut recorder = Recorder::default();
        recorder.observe(&sample(59_950, 0));
        recorder.begin("session".into());
        let result = recorder
            .read(
                "session",
                Timestamp::from_millis(59_975),
                Timestamp::from_millis(60_050),
            )
            .unwrap();
        assert!(result.complete);
        assert_eq!(result.load.len(), 2);
        assert_eq!(result.load[0].at, Timestamp::from_millis(0));
        assert_eq!(result.load[1].at, Timestamp::from_millis(60_000));
        assert!(
            result
                .load
                .iter()
                .all(|b| b.seconds == 1 && b.cpu_mean == Some(0.1))
        );
    }
    #[test]
    fn sampler_jitter_does_not_lose_coverage_at_wall_clock_second_edges() {
        let mut recorder = Recorder::default();
        recorder.observe(&sample(49_999, 0));
        recorder.begin("session".into());
        for at in [50_001, 50_999, 52_001, 52_999, 54_001] {
            recorder.observe(&sample(at, 0));
        }
        let result = recorder
            .read(
                "session",
                Timestamp::from_millis(50_000),
                Timestamp::from_millis(54_500),
            )
            .unwrap();
        assert!(result.complete);
        assert_eq!(result.load[0].seconds, 5);
    }
    #[test]
    fn missing_initial_state_and_large_gaps_are_incomplete() {
        let mut recorder = Recorder::default();
        recorder.begin("session".into());
        recorder.observe(&sample(51_000, 0));
        assert!(
            !recorder
                .read(
                    "session",
                    Timestamp::from_millis(50_000),
                    Timestamp::from_millis(52_000)
                )
                .unwrap()
                .complete
        );
        recorder.observe(&sample(52_000, 0));
        recorder.begin("next".into());
        recorder.observe(&sample(56_000, 0));
        assert!(
            !recorder
                .read(
                    "next",
                    Timestamp::from_millis(52_000),
                    Timestamp::from_millis(57_000)
                )
                .unwrap()
                .complete
        );
    }
}
