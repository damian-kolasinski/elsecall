//! Wall-clock instants for records and deadlines.
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Milliseconds since the Unix epoch, written as RFC 3339 in UTC.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(u64);

impl Timestamp {
    pub fn now() -> Self {
        Self::from(SystemTime::now())
    }
    pub fn from_millis(millis: u64) -> Self {
        Self(millis)
    }
    pub fn millis(self) -> u64 {
        self.0
    }
    pub fn after(self, duration: Duration) -> Self {
        Self(self.0.saturating_add(duration.as_millis() as u64))
    }
    /// Time from `earlier` to this instant; zero if `earlier` is later.
    pub fn since(self, earlier: Timestamp) -> Duration {
        Duration::from_millis(self.0.saturating_sub(earlier.0))
    }
    /// Parses RFC 3339, such as `2026-11-02T18:00:00Z`.
    pub fn parse_rfc3339(text: &str) -> Option<Self> {
        humantime::parse_rfc3339_weak(text).ok().map(Self::from)
    }
}
impl From<SystemTime> for Timestamp {
    fn from(time: SystemTime) -> Self {
        Self(
            time.duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_millis() as u64),
        )
    }
}
impl std::fmt::Display for Timestamp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let time = UNIX_EPOCH + Duration::from_millis(self.0);
        write!(f, "{}", humantime::format_rfc3339_millis(time))
    }
}
impl Serialize for Timestamp {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}
impl<'de> Deserialize<'de> for Timestamp {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::parse_rfc3339(&text)
            .ok_or_else(|| serde::de::Error::custom("expected an RFC 3339 timestamp"))
    }
}

/// Durations as fractional seconds in JSON.
pub mod seconds {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::Duration;

    pub fn serialize<S: Serializer>(value: &Duration, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_f64(value.as_secs_f64())
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Duration, D::Error> {
        Duration::try_from_secs_f64(f64::deserialize(deserializer)?)
            .map_err(serde::de::Error::custom)
    }

    pub mod optional {
        use serde::{Deserialize, Deserializer, Serializer};
        use std::time::Duration;

        pub fn serialize<S: Serializer>(
            value: &Option<Duration>,
            serializer: S,
        ) -> Result<S::Ok, S::Error> {
            match value {
                Some(value) => super::serialize(value, serializer),
                None => serializer.serialize_none(),
            }
        }
        pub fn deserialize<'de, D: Deserializer<'de>>(
            deserializer: D,
        ) -> Result<Option<Duration>, D::Error> {
            Option::<f64>::deserialize(deserializer)?
                .map(|s| Duration::try_from_secs_f64(s).map_err(serde::de::Error::custom))
                .transpose()
        }
    }
}

/// Durations as whole milliseconds in JSON, for inputs that must read back
/// exactly as they were used.
pub mod millis {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::Duration;

    pub fn serialize<S: Serializer>(value: &Duration, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u64(value.as_millis().min(u128::from(u64::MAX)) as u64)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Duration, D::Error> {
        Ok(Duration::from_millis(u64::deserialize(deserializer)?))
    }

    pub mod optional {
        use serde::{Deserialize, Deserializer, Serializer};
        use std::time::Duration;

        pub fn serialize<S: Serializer>(
            value: &Option<Duration>,
            serializer: S,
        ) -> Result<S::Ok, S::Error> {
            match value {
                Some(value) => super::serialize(value, serializer),
                None => serializer.serialize_none(),
            }
        }
        pub fn deserialize<'de, D: Deserializer<'de>>(
            deserializer: D,
        ) -> Result<Option<Duration>, D::Error> {
            Ok(Option::<u64>::deserialize(deserializer)?.map(Duration::from_millis))
        }
    }
}

/// `duration` rounded to whole milliseconds.
pub fn whole_millis(duration: Duration) -> Duration {
    Duration::from_millis(
        ((duration.as_nanos() + 500_000) / 1_000_000).min(u128::from(u64::MAX)) as u64,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_rfc3339() {
        let time = Timestamp::from_millis(1_793_000_000_123);
        let text = serde_json::to_string(&time).unwrap();
        assert_eq!(text, "\"2026-10-26T07:33:20.123Z\"");
        assert_eq!(serde_json::from_str::<Timestamp>(&text).unwrap(), time);
        assert_eq!(
            Timestamp::parse_rfc3339("2026-10-26T07:33:20Z").unwrap(),
            Timestamp::from_millis(1_793_000_000_000)
        );
        assert!(Timestamp::parse_rfc3339("tomorrow").is_none());
        assert_eq!(
            time.after(Duration::from_secs(2)).since(time),
            Duration::from_secs(2)
        );
    }
}
