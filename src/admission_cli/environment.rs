//! The caller's `ELSECALL*` controls, read once and validated as untrusted input:
//!
//! - `ELSECALL=off` bypasses Elsecall: `run` runs the command directly and leases
//!   are no-ops that succeed. `on`, empty or unset means normal operation.
//! - `ELSECALL_HOLD_LIMIT` is the hold limit of interactive requests that set none.
//! - `ELSECALL_WHEN_UNAVAILABLE` is `run` or `fail`, for when the service is down.
//!
//! Values are never echoed in failures.
use crate::{
    consumer_configuration::WhenUnavailable,
    failure::{Failure, Result},
    scheduling::request::MAXIMUM_HOLD_LIMIT,
};
use std::{ffi::OsString, time::Duration};

pub const MODE: &str = "ELSECALL";
pub const HOLD_LIMIT: &str = "ELSECALL_HOLD_LIMIT";
pub const WHEN_UNAVAILABLE: &str = "ELSECALL_WHEN_UNAVAILABLE";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    #[default]
    On,
    /// Elsecall steps aside entirely, for consumers' tests and emergencies.
    Off,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CallerEnvironment {
    pub mode: Mode,
    pub hold_limit: Option<Duration>,
    pub when_unavailable: Option<WhenUnavailable>,
}

fn invalid(name: &str, accepted: &str) -> Failure {
    Failure::new(
        "invalid_request",
        format!("The environment variable {name} has an unsupported value."),
        format!("Set {name} to {accepted}, or unset it, then rerun the same command."),
    )
}

impl CallerEnvironment {
    /// Reads this process's environment.
    pub fn current() -> Result<Self> {
        Self::from_lookup(|name| std::env::var_os(name))
    }

    pub fn from_lookup(lookup: impl Fn(&str) -> Option<OsString>) -> Result<Self> {
        // An empty value counts as unset, as shells make unsetting awkward.
        let value = |name: &str| -> Result<Option<String>> {
            match lookup(name) {
                None => Ok(None),
                Some(value) if value.is_empty() => Ok(None),
                Some(value) => value
                    .into_string()
                    .map(Some)
                    .map_err(|_| invalid(name, "a plain text value")),
            }
        };
        let mode = match value(MODE)?.as_deref() {
            None | Some("on") => Mode::On,
            Some("off") => Mode::Off,
            Some(_) => return Err(invalid(MODE, "off (to bypass Elsecall) or on")),
        };
        let hold_limit = value(HOLD_LIMIT)?
            .map(|text| {
                humantime::parse_duration(&text)
                    .ok()
                    .filter(|limit| *limit <= MAXIMUM_HOLD_LIMIT)
                    .ok_or_else(|| invalid(HOLD_LIMIT, "a duration such as 90s or 5m, at most 24h"))
            })
            .transpose()?;
        let when_unavailable = value(WHEN_UNAVAILABLE)?
            .map(|text| {
                WhenUnavailable::parse(&text)
                    .ok_or_else(|| invalid(WHEN_UNAVAILABLE, "run or fail"))
            })
            .transpose()?;
        Ok(Self {
            mode,
            hold_limit,
            when_unavailable,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(pairs: &[(&str, &str)]) -> Result<CallerEnvironment> {
        let pairs: Vec<(String, OsString)> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), OsString::from(v)))
            .collect();
        CallerEnvironment::from_lookup(|name| {
            pairs
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
        })
    }

    #[test]
    fn reads_the_controls() {
        assert_eq!(read(&[]).unwrap(), CallerEnvironment::default());
        assert_eq!(
            read(&[
                ("ELSECALL", "off"),
                ("ELSECALL_HOLD_LIMIT", "90s"),
                ("ELSECALL_WHEN_UNAVAILABLE", "run")
            ])
            .unwrap(),
            CallerEnvironment {
                mode: Mode::Off,
                hold_limit: Some(Duration::from_secs(90)),
                when_unavailable: Some(WhenUnavailable::Run),
            }
        );
        assert_eq!(
            read(&[("ELSECALL", ""), ("ELSECALL_HOLD_LIMIT", "")]).unwrap(),
            CallerEnvironment::default(),
            "empty means unset"
        );
        assert_eq!(read(&[("ELSECALL", "on")]).unwrap().mode, Mode::On);
    }

    #[test]
    fn refuses_unknown_values_without_echoing_them() {
        for pairs in [
            [("ELSECALL", "0")],
            [("ELSECALL_HOLD_LIMIT", "soon")],
            [("ELSECALL_HOLD_LIMIT", "2d")],
            [("ELSECALL_WHEN_UNAVAILABLE", "secret-value")],
        ] {
            let failure = read(&pairs).unwrap_err();
            assert_eq!(failure.code, "invalid_request");
            assert!(failure.message.contains(pairs[0].0));
            assert!(!failure.message.contains(pairs[0].1) && !failure.hint.contains(pairs[0].1));
        }
    }
}
