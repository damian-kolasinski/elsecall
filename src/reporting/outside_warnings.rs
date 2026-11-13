//! Early warnings about outside processes, as a report shows them per member:
//! each warning that held in the period and how long it had held. Pure.
use crate::{
    outside_pressure::warning::Warning, reporting::outside::OutsideHistory,
    reporting::window::Window, scheduling::timestamp::Timestamp,
};
use serde::Serialize;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PeriodWarning {
    #[serde(flatten)]
    pub warning: Warning,
    /// When it last held; absent when it still held at the end of the period.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub until: Option<Timestamp>,
    /// How long it had held by then, from its onset, to a tenth of an hour.
    pub hours: f64,
}

/// The warnings of `history` that held in `window`, the longest-held first.
pub fn in_period(history: &OutsideHistory, window: Window) -> Vec<PeriodWarning> {
    let mut warnings: Vec<PeriodWarning> = history
        .warnings
        .iter()
        .filter(|episode| {
            episode.warning.since < window.until
                && episode.until.is_none_or(|until| until >= window.since)
        })
        .map(|episode| {
            let until = episode.until.filter(|until| *until < window.until);
            let end = until.unwrap_or(window.until);
            let hours = end.since(episode.warning.since).as_secs_f64() / 3600.0;
            PeriodWarning {
                warning: episode.warning.clone(),
                until,
                hours: (hours * 10.0).round() / 10.0,
            }
        })
        .collect();
    warnings.sort_by(|a, b| b.hours.total_cmp(&a.hours));
    warnings
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outside_pressure::{
        sampling::Owner,
        warning::{Concern, WarningEpisode},
    };

    fn at(hours: u64) -> Timestamp {
        Timestamp::from_millis(1_794_000_000_000 + hours * 3_600_000)
    }

    fn episode(process: &str, since: u64, until: Option<u64>) -> WarningEpisode {
        WarningEpisode {
            warning: Warning {
                concern: Concern::Spinning { cpu_cores: 1.0 },
                process: process.into(),
                pid: 412,
                started: at(0),
                owner: Owner::Root,
                since: at(since),
                observed_at: at(until.unwrap_or(100)),
            },
            until: until.map(at),
        }
    }

    #[test]
    fn keeps_what_held_in_the_period_and_how_long_it_had() {
        let window = Window::new(at(24), at(72), at(72)).unwrap();
        let history = OutsideHistory {
            warnings: vec![
                episode("before", 2, Some(10)),
                episode("ended", 20, Some(30)),
                episode("ongoing", 30, None),
                episode("ended_after", 60, Some(80)),
            ],
            ..Default::default()
        };
        let warnings = in_period(&history, window);
        let found: Vec<(&str, Option<Timestamp>, f64)> = warnings
            .iter()
            .map(|w| (w.warning.process.as_str(), w.until, w.hours))
            .collect();
        assert_eq!(
            found,
            [
                ("ongoing", None, 42.0),
                ("ended_after", None, 12.0),
                ("ended", Some(at(30)), 10.0),
            ]
        );
    }
}
