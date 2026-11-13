//! Request properties and caller choices as callers write them on the command line.
use crate::{
    consumer_configuration::WhenUnavailable,
    failure::Result,
    scheduling::{
        capacity::Cores,
        request::{AtDeadline, invalid},
        timestamp::Timestamp,
    },
};
use clap::{Args, ValueEnum};
use std::{path::PathBuf, time::Duration};

/// Options shared by every work request. Values are parsed here rather than by
/// Clap so that every mistake is reported as `invalid_request` with a reason.
/// Each one overrides the matching label rule of `.elsecall.toml`.
#[derive(Args, Debug, Clone, Default)]
pub struct RequestOptions {
    /// Cores the work occupies, weighted by core type, e.g. 4 or 0.5 [default: the
    /// label rule's, else 1]
    #[arg(long, value_name = "CORES")]
    pub size: Option<String>,
    /// What kind of work this is, e.g. "test FeatureModule"; selects the first label
    /// rule of .elsecall.toml that matches it
    #[arg(long, value_name = "TEXT")]
    pub label: Option<String>,
    /// Optional bounded JSON workload descriptor, separate from the display label
    #[arg(long, value_name = "JSON")]
    pub workload: Option<String>,
    /// Longest wait for capacity before failing with fleet_busy, e.g. 90s or 5m
    /// [default: ELSECALL_HOLD_LIMIT, else the label rule's, else 10 minutes minus
    /// the label's expected run time, at least 30s]
    #[arg(long, value_name = "DURATION")]
    pub hold_limit: Option<String>,
    /// Wait as interactive work, even if the label rule makes it deferrable
    #[arg(long, conflicts_with = "deferrable")]
    pub interactive: bool,
    /// Start only when the machine has room and no interactive work waits;
    /// needs --deadline and --at-deadline unless the label rule sets them
    #[arg(long)]
    pub deferrable: bool,
    /// When deferrable work starts or skips: a duration from now (30m) or an
    /// RFC 3339 time (2026-11-02T18:00:00Z)
    #[arg(long, value_name = "WHEN")]
    pub deadline: Option<String>,
    /// What deferrable work does at its deadline: start ahead of interactive
    /// work (still never overcommitting), or skip with deadline_skipped
    #[arg(long, value_name = "ACTION")]
    pub at_deadline: Option<DeadlineAction>,
}

/// What the caller wants beyond the request itself.
#[derive(Args, Debug, Clone, Default)]
pub struct CallerOptions {
    /// When the local service cannot be reached: fail with service_unavailable,
    /// or run without admission [default: ELSECALL_WHEN_UNAVAILABLE, else
    /// [admission] when_unavailable in .elsecall.toml, else fail]
    #[arg(long, value_name = "ACTION")]
    pub when_unavailable: Option<UnavailableAction>,
    /// Write the run ID and a newline to this file once admission decides, for
    /// your own telemetry; the file is empty when there is no record
    #[arg(long, value_name = "PATH")]
    pub run_id_file: Option<PathBuf>,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeadlineAction {
    Start,
    Skip,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnavailableAction {
    /// Fail with service_unavailable; the command does not run
    Fail,
    /// Run the command directly (a lease is not held), after one notice line
    Run,
}
impl From<UnavailableAction> for WhenUnavailable {
    fn from(action: UnavailableAction) -> Self {
        match action {
            UnavailableAction::Fail => WhenUnavailable::Fail,
            UnavailableAction::Run => WhenUnavailable::Run,
        }
    }
}

/// Which urgency the caller or a rule chose.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UrgencyChoice {
    Interactive,
    Deferrable,
}

/// What the command line declares, parsed; anything absent is left to the
/// environment, the label rule and defaults.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Declared {
    pub size: Option<Cores>,
    pub label: Option<String>,
    pub workload: Option<crate::workload_metadata::Workload>,
    pub urgency: Option<UrgencyChoice>,
    pub hold_limit: Option<Duration>,
    pub deadline: Option<Timestamp>,
    pub at_deadline: Option<AtDeadline>,
}

fn duration(text: &str, option: &str) -> Result<Duration> {
    humantime::parse_duration(text)
        .map_err(|_| invalid(format!("{option} takes a duration such as 90s, 10m or 1h.")))
}

impl RequestOptions {
    pub fn declared(&self, now: Timestamp) -> Result<Declared> {
        let size = self
            .size
            .as_deref()
            .map(|size| {
                Cores::parse(size)
                    .ok_or_else(|| invalid("--size takes a number of cores such as 2 or 0.5."))
            })
            .transpose()?;
        let hold_limit = self
            .hold_limit
            .as_deref()
            .map(|limit| duration(limit, "--hold-limit"))
            .transpose()?;
        let deadline = self
            .deadline
            .as_deref()
            .map(|deadline| {
                Timestamp::parse_rfc3339(deadline)
                    .map(Ok)
                    .unwrap_or_else(|| duration(deadline, "--deadline").map(|d| now.after(d)))
            })
            .transpose()?;
        let urgency = match (self.interactive, self.deferrable) {
            (true, _) => Some(UrgencyChoice::Interactive),
            (false, true) => Some(UrgencyChoice::Deferrable),
            (false, false) => None,
        };
        Ok(Declared {
            size,
            label: self.label.clone(),
            workload: self
                .workload
                .as_deref()
                .map(crate::workload_metadata::parse_workload)
                .transpose()?,
            urgency,
            hold_limit,
            deadline,
            at_deadline: self.at_deadline.map(|action| match action {
                DeadlineAction::Start => AtDeadline::Start,
                DeadlineAction::Skip => AtDeadline::Skip,
            }),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn declared(options: RequestOptions) -> Result<Declared> {
        options.declared(Timestamp::from_millis(0))
    }

    #[test]
    fn parses_what_the_command_line_declares() {
        let interactive = declared(RequestOptions {
            size: Some("2.5".into()),
            hold_limit: Some("90s".into()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(interactive.size, Some(Cores::from_millis(2500)));
        assert_eq!(interactive.hold_limit, Some(Duration::from_secs(90)));
        assert_eq!(interactive.urgency, None, "a hold limit chooses no urgency");
        let deferrable = declared(RequestOptions {
            deferrable: true,
            deadline: Some("30m".into()),
            at_deadline: Some(DeadlineAction::Skip),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(deferrable.urgency, Some(UrgencyChoice::Deferrable));
        assert_eq!(
            deferrable.deadline,
            Some(Timestamp::from_millis(30 * 60 * 1000))
        );
        assert_eq!(deferrable.at_deadline, Some(AtDeadline::Skip));
        let at = declared(RequestOptions {
            deadline: Some("1970-01-01T00:00:10Z".into()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(at.deadline, Some(Timestamp::from_millis(10_000)));
    }

    #[test]
    fn rejects_unparsable_values_as_invalid_requests() {
        for options in [
            RequestOptions {
                size: Some("lots".into()),
                ..Default::default()
            },
            RequestOptions {
                hold_limit: Some("soon".into()),
                ..Default::default()
            },
            RequestOptions {
                deadline: Some("whenever".into()),
                ..Default::default()
            },
        ] {
            assert_eq!(declared(options).unwrap_err().code, "invalid_request");
        }
    }
}
