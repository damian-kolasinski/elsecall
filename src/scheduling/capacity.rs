//! Capacity and request sizes, counted in weighted cores.
use crate::inspection::CoreCounts;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// An amount of capacity, held in thousandths of a weighted core so that admission
/// adds and subtracts exactly.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Cores(u32);

/// The largest amount a size or setting may name; far above any real machine.
const MAXIMUM_MILLIS: u32 = 1_000_000 * 1000;

impl Cores {
    pub const ZERO: Cores = Cores(0);
    pub const ONE: Cores = Cores(1000);

    pub fn from_millis(millis: u32) -> Self {
        Self(millis)
    }
    pub fn millis(self) -> u32 {
        self.0
    }
    pub fn saturating_sub(self, other: Cores) -> Cores {
        Cores(self.0.saturating_sub(other.0))
    }
    pub fn saturating_add(self, other: Cores) -> Cores {
        Cores(self.0.saturating_add(other.0))
    }
    pub fn as_f64(self) -> f64 {
        f64::from(self.0) / 1000.0
    }
    /// Converts a nonnegative amount, rounding to a thousandth of a core.
    pub fn from_f64(cores: f64) -> Option<Cores> {
        let millis = (cores * 1000.0).round();
        (cores.is_finite() && cores >= 0.0 && millis <= f64::from(MAXIMUM_MILLIS))
            .then_some(Cores(millis as u32))
    }
    /// Parses a decimal amount such as `2` or `0.5` with at most three decimals.
    pub fn parse(text: &str) -> Option<Cores> {
        let (whole, fraction) = text.split_once('.').unwrap_or((text, "0"));
        let digits = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
        if !digits(whole) || !digits(fraction) || fraction.len() > 3 {
            return None;
        }
        let whole: u32 = whole.parse().ok()?;
        let fraction: u32 = format!("{fraction:0<3}").parse().ok()?;
        let millis = whole.checked_mul(1000)?.checked_add(fraction)?;
        (millis <= MAXIMUM_MILLIS).then_some(Cores(millis))
    }
}

impl std::fmt::Display for Cores {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_f64())
    }
}
impl Serialize for Cores {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_f64(self.as_f64())
    }
}
impl<'de> Deserialize<'de> for Cores {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let cores = f64::deserialize(deserializer)?;
        Cores::from_f64(cores).ok_or_else(|| {
            serde::de::Error::custom("cores must be a nonnegative number below one million")
        })
    }
}

/// How much one logical CPU of each type counts. Efficiency cores deliver roughly
/// half the throughput of performance cores on current Apple silicon.
pub const PERFORMANCE_CORE: Cores = Cores(1000);
pub const EFFICIENCY_CORE: Cores = Cores(500);

/// A member's capacity: its weighted cores minus the operator's reserve.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capacity {
    pub weighted_cores: Cores,
    pub reserved: Cores,
    /// What admission may hand out; never below zero.
    pub capacity: Cores,
}
impl Capacity {
    pub fn of(cores: CoreCounts, reserved: Cores) -> Self {
        let weighted_cores = Cores(
            cores
                .performance
                .saturating_mul(PERFORMANCE_CORE.0)
                .saturating_add(cores.efficiency.saturating_mul(EFFICIENCY_CORE.0)),
        );
        Self {
            weighted_cores,
            reserved,
            capacity: weighted_cores.saturating_sub(reserved),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_decimal_sizes_exactly() {
        assert_eq!(Cores::parse("2"), Some(Cores(2000)));
        assert_eq!(Cores::parse("0.5"), Some(Cores(500)));
        assert_eq!(Cores::parse("1.25"), Some(Cores(1250)));
        assert_eq!(Cores::parse("0"), Some(Cores::ZERO));
        for invalid in ["", ".5", "1.", "-1", "1.2345", "1e3", " 1", "x", "2000000"] {
            assert_eq!(Cores::parse(invalid), None, "{invalid}");
        }
        assert_eq!(Cores::from_f64(1.0004), Some(Cores(1000)));
        assert_eq!(Cores::from_f64(-0.5), None);
        assert_eq!(Cores::from_f64(f64::NAN), None);
        assert_eq!(Cores(1500).to_string(), "1.5");
    }

    #[test]
    fn weights_core_types_and_subtracts_reserve() {
        let mac_mini = CoreCounts {
            performance: 10,
            efficiency: 4,
        };
        let capacity = Capacity::of(mac_mini, Cores(2000));
        assert_eq!(capacity.weighted_cores, Cores(12_000));
        assert_eq!(capacity.capacity, Cores(10_000));
        assert_eq!(
            Capacity::of(mac_mini, Cores(20_000)).capacity,
            Cores::ZERO,
            "a reserve larger than the machine leaves no capacity"
        );
    }
}
