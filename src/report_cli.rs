//! `elsecall report`: its arguments, checked here, and the call to the service.
use crate::{
    client,
    failure::Result,
    paths::Paths,
    report_api::ReportRequest,
    reporting::window::{MAXIMUM_PERIOD, invalid},
    scheduling::timestamp::Timestamp,
};
use clap::Args;
use std::time::Duration;

#[derive(Args, Debug)]
pub struct ReportArgs {
    /// The period's start: a duration back from now, such as 7d or 12h, or an
    /// RFC 3339 time; at most 30 days ago
    #[arg(long, default_value = "7d")]
    pub since: String,
    /// Only requests this member made or ran, and only its load
    #[arg(long, value_name = "NAME")]
    pub member: Option<String>,
    /// Only requests whose label matches this glob (* and ?), such as "test *"
    #[arg(long, value_name = "PATTERN")]
    pub label: Option<String>,
    /// The width of each point of a member's load series [default: a 24th of the
    /// period]
    #[arg(long, value_name = "DURATION")]
    pub bucket: Option<String>,
}

/// The period's start for `--since`, at `now`.
pub fn since(text: &str, now: Timestamp) -> Result<Timestamp> {
    let since = match humantime::parse_duration(text) {
        Ok(back) => Timestamp::from_millis(now.millis().saturating_sub(back.as_millis() as u64)),
        Err(_) => Timestamp::parse_rfc3339(text).ok_or_else(|| {
            invalid("--since must be a duration such as 7d, or an RFC 3339 time.")
        })?,
    };
    if since >= now || now.since(since) > MAXIMUM_PERIOD {
        return Err(invalid(
            "--since must be in the past and at most 30 days ago, as long as records and load history are kept.",
        ));
    }
    Ok(since)
}

pub fn request(args: ReportArgs, now: Timestamp) -> Result<ReportRequest> {
    let series_ms = match &args.bucket {
        None => None,
        Some(text) => Some(
            humantime::parse_duration(text)
                .ok()
                .filter(|d| *d >= Duration::from_secs(60) && *d <= MAXIMUM_PERIOD)
                .ok_or_else(|| invalid("--bucket must be a duration from 1m to 30d, such as 1h."))?
                .as_millis() as u64,
        ),
    };
    Ok(ReportRequest {
        since_ms: since(&args.since, now)?.millis(),
        member: args.member,
        label: args.label,
        series_ms,
    })
}

pub async fn run(paths: &Paths, args: ReportArgs) -> Result<serde_json::Value> {
    let request = request(args, Timestamp::now())?;
    client::post_large(&paths.socket(), "/v1/report", &request).await
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: u64 = 86_400_000;

    #[test]
    fn since_takes_a_duration_or_a_time_within_retention() {
        let now = Timestamp::from_millis(40 * DAY);
        assert_eq!(since("7d", now).unwrap(), Timestamp::from_millis(33 * DAY));
        assert_eq!(
            since("1970-02-05T00:00:00Z", now).unwrap(),
            Timestamp::from_millis(35 * DAY)
        );
        for bad in ["31d", "0s", "yesterday", "1970-03-01T00:00:00Z"] {
            assert_eq!(
                since(bad, now).unwrap_err().code,
                "invalid_request",
                "{bad}"
            );
        }
    }

    #[test]
    fn the_bucket_is_a_duration_of_at_least_a_minute() {
        let args = |bucket: &str| ReportArgs {
            since: "1d".into(),
            member: Some("devbox".into()),
            label: None,
            bucket: Some(bucket.into()),
        };
        let now = Timestamp::from_millis(40 * DAY);
        let request = request(args("1h"), now).unwrap();
        assert_eq!(request.series_ms, Some(3_600_000));
        assert_eq!(request.since_ms, 39 * DAY);
        assert_eq!(request.member.as_deref(), Some("devbox"));
        for bad in ["30s", "1y", "soon"] {
            assert_eq!(
                super::request(args(bad), now).unwrap_err().code,
                "invalid_request"
            );
        }
    }
}
