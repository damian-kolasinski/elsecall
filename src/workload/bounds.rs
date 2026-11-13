//! Validated explicit arrival bounds. Evidence outside the arrival bounds stays.
use crate::reporting::window::invalid;
use crate::{failure::Result, scheduling::timestamp::Timestamp, workload::trace::Trace};

pub fn until(text: Option<&str>, now: Timestamp) -> Result<Timestamp> {
    match text {
        None => Ok(now),
        Some(text) => Timestamp::parse_rfc3339(text)
            .filter(|t| *t <= now)
            .ok_or_else(|| invalid("--until must be an RFC 3339 timestamp no later than now.")),
    }
}
pub fn within(
    trace: &Trace,
    since: Option<&str>,
    until: Option<&str>,
) -> Result<(Timestamp, Timestamp)> {
    let parse = |text: Option<&str>, fallback| match text {
        None => Ok(fallback),
        Some(text) => Timestamp::parse_rfc3339(text)
            .ok_or_else(|| invalid("Explicit workload bounds must be RFC 3339 timestamps.")),
    };
    let start = parse(since, trace.since)?;
    let end = parse(until, trace.until)?;
    if start < trace.since || end > trace.until || start >= end {
        return Err(invalid(
            "Explicit arrival bounds must be nonempty and inside the source history.",
        ));
    }
    Ok((start, end))
}
