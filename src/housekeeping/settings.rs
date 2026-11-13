//! The operator's housekeeping settings, from `[housekeeping]` in `config.toml`.
use crate::scheduling::request::AtDeadline;
use serde::Serialize;
use std::time::Duration;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Settings {
    /// How often a pass runs, as deferrable, pinned work; also its deadline.
    #[serde(with = "crate::scheduling::timestamp::seconds")]
    pub interval: Duration,
    /// What a pass that could not start within its interval does.
    pub at_deadline: AtDeadline,
    /// Copies not used for this long are removed.
    #[serde(with = "crate::scheduling::timestamp::seconds")]
    pub idle_after: Duration,
    /// Whether copies whose workspace the calling member deleted are removed.
    pub remove_deleted_workspaces: bool,
    /// Free space below the larger of these is disk pressure.
    pub min_free_percent: u8,
    pub min_free_bytes: u64,
    /// A referenced object store is compacted once it holds this many packs; 0
    /// never compacts.
    pub compact_store_packs: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(60 * 60),
            at_deadline: AtDeadline::Skip,
            idle_after: Duration::from_secs(14 * 24 * 60 * 60),
            remove_deleted_workspaces: true,
            min_free_percent: 10,
            min_free_bytes: 50_000_000_000,
            compact_store_packs: 20,
        }
    }
}

impl Settings {
    /// The free space a volume of `total` bytes keeps: the larger of the share and
    /// the amount, but never more than the whole volume.
    pub fn threshold(&self, total: u64) -> u64 {
        let share = (total as u128 * self.min_free_percent as u128 / 100) as u64;
        share.max(self.min_free_bytes).min(total)
    }
}

/// Parses a size such as `50GB`, `500 MB` or `1TB`, in decimal units as disks
/// are sold and Finder counts.
pub fn parse_size(text: &str) -> Option<u64> {
    let text = text.trim();
    let split = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    let number: u64 = number.parse().ok()?;
    let factor: u64 = match unit.trim().to_ascii_uppercase().as_str() {
        "" | "B" => 1,
        "KB" => 1_000,
        "MB" => 1_000_000,
        "GB" => 1_000_000_000,
        "TB" => 1_000_000_000_000,
        _ => return None,
    };
    number.checked_mul(factor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_threshold_is_the_larger_of_share_and_amount() {
        let settings = Settings::default();
        assert_eq!(settings.threshold(1_000_000_000_000), 100_000_000_000);
        assert_eq!(settings.threshold(250_000_000_000), 50_000_000_000);
        assert_eq!(settings.threshold(10_000_000_000), 10_000_000_000);
        let none = Settings {
            min_free_percent: 0,
            min_free_bytes: 0,
            ..Settings::default()
        };
        assert_eq!(none.threshold(1_000), 0);
    }

    #[test]
    fn sizes_parse_in_decimal_units() {
        assert_eq!(parse_size("50GB"), Some(50_000_000_000));
        assert_eq!(parse_size("500 mb"), Some(500_000_000));
        assert_eq!(parse_size("1TB"), Some(1_000_000_000_000));
        assert_eq!(parse_size("12"), Some(12));
        for invalid in ["", "GB", "1.5GB", "-1GB", "5 PB", "99999999999999TB"] {
            assert_eq!(parse_size(invalid), None, "{invalid}");
        }
    }
}
