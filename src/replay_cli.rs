//! `elsecall replay`: this member's recorded placement decisions scored again
//! under changed parameters.
use crate::{
    client,
    failure::Result,
    paths::Paths,
    replay_api::{ReplayRequest, invalid},
    scheduling::{replay::Change, timestamp::Timestamp},
};
use clap::Args;
use serde_json::Value;

#[derive(Args)]
#[command(
    after_help = "Prints which recorded decisions flip under the changed values, their expected completion difference and aggregate effects. Queue effects of flipped decisions are not simulated. Change config.toml and run elsecall service restart to apply a change."
)]
pub struct ReplayArgs {
    /// Only decisions requested this long ago or later, such as 24h or 7d, or
    /// since an RFC 3339 time; by default every retained record (30 days)
    #[arg(long, value_name = "DURATION|TIME")]
    pub since: Option<String>,
    /// A changed parameter, such as keep_local_margin=0.2 or
    /// admission.saturated_utilization=0.95; repeat for several. Without any,
    /// replay checks that every decision reproduces
    #[arg(long = "set", value_name = "KEY=VALUE")]
    pub set: Vec<String>,
}

/// The instant `--since` names: `now` minus a duration, or an RFC 3339 time.
pub fn since(text: &str, now: Timestamp) -> Result<Timestamp> {
    let text = text.trim();
    if let Ok(ago) = humantime::parse_duration(text) {
        let ago = u64::try_from(ago.as_millis()).unwrap_or(u64::MAX);
        return Ok(Timestamp::from_millis(now.millis().saturating_sub(ago)));
    }
    Timestamp::parse_rfc3339(text).ok_or_else(|| {
        invalid("--since must be a duration such as 24h or 7d, or an RFC 3339 time.")
    })
}

pub async fn run(paths: &Paths, args: ReplayArgs) -> Result<Value> {
    let since = args
        .since
        .as_deref()
        .map(|text| since(text, Timestamp::now()))
        .transpose()?;
    // Checked here too, so that a mistyped key fails without the service.
    Change::parse(&args.set).map_err(invalid)?;
    client::post_large(
        &paths.socket(),
        "/v1/replay",
        &ReplayRequest {
            since,
            set: args.set,
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn since_takes_a_duration_ago_or_a_time() {
        let now = Timestamp::parse_rfc3339("2026-11-03T12:00:00Z").unwrap();
        assert_eq!(
            since("24h", now).unwrap(),
            Timestamp::parse_rfc3339("2026-11-02T12:00:00Z").unwrap()
        );
        assert_eq!(
            since("7d", now).unwrap(),
            Timestamp::parse_rfc3339("2026-10-27T12:00:00Z").unwrap()
        );
        assert_eq!(
            since("2026-11-03T08:30:00Z", now).unwrap(),
            Timestamp::parse_rfc3339("2026-11-03T08:30:00Z").unwrap()
        );
        let error = since("yesterday", now).unwrap_err();
        assert_eq!(error.code, "invalid_request");
    }
}
