//! Which way a member's load is heading, read from its load averages.

use crate::fleet_source::LoadAverages;

/// The 1-minute average counts as steady while it stays within this fraction of the
/// 15-minute average. The 1-minute average is noisy; a tighter band lights the arrow on
/// machines doing nothing in particular.
const STEADY_FRACTION: f64 = 0.2;
/// The steady band never narrows below this many processes, so an idle Mac, which
/// usually sits at a load of one to three, does not flicker between arrows.
const STEADY_FLOOR: f64 = 0.5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LoadTrend {
    Rising,
    Steady,
    Falling,
}

impl LoadTrend {
    /// Compares the last minute with the last fifteen.
    pub(crate) fn of(load: &LoadAverages) -> Self {
        let band = (load.fifteen.abs() * STEADY_FRACTION).max(STEADY_FLOOR);
        let change = load.one - load.fifteen;
        if change > band {
            Self::Rising
        } else if change < -band {
            Self::Falling
        } else {
            Self::Steady
        }
    }

    pub(crate) fn word(self) -> &'static str {
        match self {
            Self::Rising => "rising",
            Self::Steady => "steady",
            Self::Falling => "falling",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load(one: f64, fifteen: f64) -> LoadAverages {
        LoadAverages {
            one,
            five: (one + fifteen) / 2.0,
            fifteen,
        }
    }

    #[test]
    fn the_last_minute_against_the_last_fifteen_sets_the_trend() {
        assert_eq!(LoadTrend::of(&load(207.4, 141.3)), LoadTrend::Rising);
        assert_eq!(LoadTrend::of(&load(9.0, 12.0)), LoadTrend::Falling);
        assert_eq!(LoadTrend::of(&load(1.4, 1.5)), LoadTrend::Steady);
    }

    #[test]
    fn a_fifth_of_the_fifteen_minute_average_is_still_steady() {
        assert_eq!(LoadTrend::of(&load(12.0, 10.0)), LoadTrend::Steady);
        assert_eq!(LoadTrend::of(&load(8.0, 10.0)), LoadTrend::Steady);
        assert_eq!(LoadTrend::of(&load(12.2, 10.0)), LoadTrend::Rising);
        assert_eq!(LoadTrend::of(&load(7.8, 10.0)), LoadTrend::Falling);
    }

    #[test]
    fn an_idle_machine_does_not_flicker_on_noise() {
        assert_eq!(LoadTrend::of(&load(2.3, 2.0)), LoadTrend::Steady);
        assert_eq!(LoadTrend::of(&load(1.6, 2.0)), LoadTrend::Steady);
        assert_eq!(LoadTrend::of(&load(0.4, 0.0)), LoadTrend::Steady);
        assert_eq!(LoadTrend::of(&load(0.0, 0.45)), LoadTrend::Steady);
        assert_eq!(LoadTrend::of(&load(0.6, 0.0)), LoadTrend::Rising);
    }
}
