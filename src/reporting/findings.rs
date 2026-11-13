//! The report's most notable problems, each a sentence an agent can act on with
//! the metric and threshold behind it. Pure, from the metrics alone; README
//! "Fleet reports" lists the kinds and thresholds.
use crate::{
    outside_pressure::{
        blocked::OutsideProcess,
        remedy::{self, Act},
        trend,
        warning::Concern,
    },
    reporting::{
        accuracy::Agreement,
        blocked::{Blocked, BlockedQueue, Pressure, QUEUED_SECONDS, QueueEnd},
        document::{Gathered, MemberInput, MemberMetrics, Metrics},
        leftovers::LabelLeftovers,
        load::{BUSY_CPU, IDLE_CPU},
        outside_warnings::PeriodWarning,
    },
    scheduling::timestamp::Timestamp,
};
use serde::Serialize;

/// At most this many findings, the most severe first.
pub const MAXIMUM: usize = 10;
/// Estimates off by this factor either way…
pub const ESTIMATE_FACTOR: f64 = 1.5;
/// …over at least this many samples…
pub const ESTIMATE_SAMPLES: usize = 3;
/// …whose medians differ by at least this many seconds: a few seconds either way
/// change no decision worth an agent's time.
pub const ESTIMATE_SECONDS: f64 = 2.0;
pub const FLEET_BUSY_RATE: f64 = 0.05;
pub const SATURATED_BUSY_SHARE: f64 = 0.1;
pub const MASTER_SATURATED_SHARE: f64 = 0.1;
pub const IMBALANCED_SHARE: f64 = 0.2;
pub const IDLE_WHILE_QUEUED_SECONDS: u64 = 300;
pub const SPEED_DRIFT: f64 = 0.15;
pub const SPEED_CONVERSION: f64 = 1.2;
/// A label's leftovers are heavy at this much CPU time after their commands
/// exited, over the period: five core-minutes of load admission did not admit…
pub const LEFTOVERS_CPU_SECONDS: f64 = 300.0;
/// …or when the cap stopped at least this many of its runs' leftovers…
pub const LEFTOVERS_CAPPED_RUNS: usize = 2;
/// …and at least this share of them.
pub const LEFTOVERS_CAPPED_SHARE: f64 = 0.1;
/// A member that admitted nothing because of outside pressure for this long is
/// lost to the fleet: placement skips it and work pinned there waits…
pub const BLOCKED_SECONDS: u64 = 3600;
/// …and is high from this long, as is a request queued that long behind it.
pub const BLOCKED_HIGH_SECONDS: u64 = 4 * 3600;
/// An outside process spinning or growing is high once it has for a day.
pub const OUTSIDE_WARNING_HIGH_HOURS: f64 = 24.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Low,
    Medium,
    High,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct Finding {
    pub kind: &'static str,
    pub severity: Severity,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub member: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// The report field the finding comes from, its value and the threshold it
    /// reached.
    pub metric: &'static str,
    pub value: f64,
    pub threshold: f64,
    /// How far past its threshold, for ordering within a severity.
    #[serde(skip)]
    weight: f64,
}

impl Finding {
    fn new(
        kind: &'static str,
        severity: Severity,
        message: String,
        metric: &'static str,
        value: f64,
        threshold: f64,
    ) -> Self {
        Finding {
            kind,
            severity,
            message,
            member: None,
            label: None,
            metric,
            value,
            threshold,
            weight: if threshold > 0.0 {
                value / threshold
            } else {
                1.0
            },
        }
    }
    fn member(mut self, member: &str) -> Self {
        self.member = Some(member.to_owned());
        self
    }
    fn label(mut self, label: Option<&String>) -> Self {
        self.label = label.cloned();
        self
    }
    fn weight(mut self, weight: f64) -> Self {
        self.weight = weight;
        self
    }
}

fn percent(share: f64) -> String {
    format!("{:.0}%", share * 100.0)
}

/// A finding when `agreement`'s estimates were notably off.
fn estimate(kind: &'static str, what: &str, agreement: &Agreement) -> Option<Finding> {
    let difference = agreement.actual_median_seconds - agreement.predicted_median_seconds;
    if agreement.samples < ESTIMATE_SAMPLES || difference.abs() < ESTIMATE_SECONDS {
        return None;
    }
    let off = agreement
        .ratio_median
        .filter(|r| *r > 0.0)
        .map_or(f64::INFINITY, |r| r.max(1.0 / r));
    if off < ESTIMATE_FACTOR {
        return None;
    }
    let mut subject = format!("{what} estimates");
    if let Some(warmth) = agreement.warmth {
        let warmth = serde_json::to_value(warmth).expect("warmth serializes");
        subject = format!("{} {subject}", warmth.as_str().unwrap_or_default());
    }
    if let Some(label) = &agreement.label {
        subject = format!("Label {label:?} {subject}");
    } else {
        subject = format!("{}{}", subject[..1].to_uppercase(), &subject[1..]);
    }
    let direction = if difference > 0.0 {
        "too low"
    } else {
        "too high"
    };
    let message = format!(
        "{subject} on {} are {}{direction}: median actual {:.1} s against {:.1} s predicted, over {} samples{}.",
        agreement.member,
        if off.is_finite() {
            format!("{off:.1}× ")
        } else {
            String::new()
        },
        agreement.actual_median_seconds,
        agreement.predicted_median_seconds,
        agreement.samples,
        if agreement.priors > 0 {
            format!(", {} of them priors", agreement.priors)
        } else {
            String::new()
        }
    );
    let severity = if off >= 2.0 {
        Severity::High
    } else {
        Severity::Medium
    };
    let found = match agreement.ratio_median.filter(|r| *r > 0.0) {
        Some(ratio) => Finding::new(
            kind,
            severity,
            message,
            "ratio_median",
            ratio,
            if ratio >= 1.0 {
                ESTIMATE_FACTOR
            } else {
                1.0 / ESTIMATE_FACTOR
            },
        )
        .weight(off / ESTIMATE_FACTOR),
        None => Finding::new(
            kind,
            severity,
            message,
            "median_difference_seconds",
            difference,
            ESTIMATE_SECONDS,
        )
        .weight(difference.abs() / ESTIMATE_SECONDS),
    };
    Some(
        found
            .member(&agreement.member)
            .label(agreement.label.as_ref()),
    )
}

/// Why the report lacks some of `member`'s history, if it does.
fn incomplete(member: &MemberInput) -> Option<Finding> {
    let missing: Vec<&str> = [
        member.records.as_ref().err().map(|_| "requests"),
        member.load.as_ref().err().map(|_| "load history"),
    ]
    .into_iter()
    .flatten()
    .collect();
    let read = member.records.as_ref().ok().copied().unwrap_or_default();
    let message = if !missing.is_empty() {
        format!(
            "{}'s {} could not be read, so the report leaves them out; see the member's {}.",
            member.member,
            missing.join(" and "),
            if member.records.is_err() {
                "records_error"
            } else {
                "load_error"
            }
        )
    } else if read.truncated {
        format!(
            "Only the latest {} of {}'s requests in the period were read; the report covers those.",
            read.requests, member.member
        )
    } else if read.dropped > 0 {
        format!(
            "{} of {}'s requests were left out: they named no member of this fleet.",
            read.dropped, member.member
        )
    } else {
        return None;
    };
    Some(
        Finding::new("incomplete", Severity::High, message, "members", 1.0, 0.0)
            .member(&member.member)
            .weight(f64::INFINITY),
    )
}

fn hours(seconds: u64) -> String {
    format!("{:.1} h", seconds as f64 / 3600.0)
}

/// `at` to the minute, as `2026-11-10T04:48Z`, or `04:48Z` when on `day`'s date.
fn minute(at: Timestamp, day: Option<Timestamp>) -> String {
    let text = at.to_string();
    let same_day = day.is_some_and(|day| day.to_string()[..10] == text[..10]);
    format!("{}Z", &text[if same_day { 11 } else { 0 }..16])
}

fn blocked_severity(seconds: u64) -> Severity {
    if seconds >= BLOCKED_HIGH_SECONDS {
        Severity::High
    } else {
        Severity::Medium
    }
}

/// The outside process that led, as a clause after the cause, if the member
/// named one, and the sentence saying what the operator does on that member.
struct Remedy {
    led_by: String,
    action: String,
}

fn remedy_for(pressure: Pressure, outside: &[OutsideProcess]) -> Remedy {
    let Some(leading) = outside.first() else {
        return Remedy {
            led_by: String::new(),
            action: match pressure {
                Pressure::MemoryPressure => "Free memory on that member.",
                Pressure::CpuSaturation => "Stop or lower the outside CPU load on that member.",
            }
            .to_owned(),
        };
    };
    let name = &leading.name;
    let (measure, still) = match (pressure, leading.memory.footprint_bytes) {
        (Pressure::MemoryPressure, Some(bytes)) => (
            format!("{} footprint", remedy::bytes_text(bytes)),
            "that large",
        ),
        (Pressure::MemoryPressure, None) => (
            format!(
                "{} resident",
                remedy::bytes_text(leading.memory.resident_bytes)
            ),
            "that large",
        ),
        (Pressure::CpuSaturation, _) => (format!("{:.1} cores", leading.cpu_cores), "that busy"),
    };
    Remedy {
        led_by: format!(", led by `{name}` ({measure}),"),
        action: action(name, remedy::act_on(leading), still),
    }
}

/// The sentence saying what the operator does about `name` on its member, if
/// it is still `still`.
fn action(name: &str, act: Act, still: &str) -> String {
    match act {
        Act::Restart => format!("Restart `{name}` on that member if it is still {still}."),
        Act::Quit => format!("Quit `{name}` on that member if it is still {still}."),
        Act::AskItsUserToQuit => format!(
            "Ask the user who runs `{name}` on that member to quit it if it is still {still}."
        ),
        Act::CloseAppsOrReboot => format!(
            "`{name}` cannot be restarted without ending the session: close the apps that use the most memory or CPU on that member, or reboot it."
        ),
    }
}

/// A finding when outside pressure kept `member`'s gate closed for long.
fn outside_pressure(member: &str, blocked: &Blocked) -> Option<Finding> {
    if blocked.seconds < BLOCKED_SECONDS {
        return None;
    }
    let Remedy { led_by, action } = remedy_for(blocked.cause, &blocked.outside);
    let cause = match (blocked.cause, blocked.outside_mean) {
        (Pressure::MemoryPressure, _) => format!(
            "memory pressure from outside Elsecall{led_by} closed its gate while Elsecall held no work there"
        ),
        (Pressure::CpuSaturation, Some(outside)) => format!(
            "CPU saturation from outside Elsecall (outside load averaged {}){led_by} closed its gate",
            percent(outside)
        ),
        (Pressure::CpuSaturation, None) => {
            format!("CPU saturation from outside Elsecall{led_by} closed its gate")
        }
    };
    Some(
        Finding::new(
            "outside_pressure_blocked",
            blocked_severity(blocked.seconds),
            format!(
                "{member} admitted nothing for {} ({} to {}): {cause}. {action} While the pressure lasts, placement skips the member and work pinned there waits.",
                hours(blocked.seconds),
                minute(blocked.since, None),
                minute(blocked.until, Some(blocked.since)),
            ),
            "members.blocked.seconds",
            blocked.seconds as f64,
            BLOCKED_SECONDS as f64,
        )
        .member(member),
    )
}

/// One finding for the requests that queued on `member` for long while outside
/// pressure kept its gate closed: about the longest-waiting label, `queues`'
/// first, with how many others there were. They share one cause, so one finding
/// leaves room for the rest of the report.
fn queued_on_closed_gate(member: &str, queues: &[BlockedQueue]) -> Option<Finding> {
    let queue = queues.first()?;
    let subject = match &queue.label {
        Some(label) => format!("Label {label:?}"),
        None => "Unlabelled work".into(),
    };
    let ended = match &queue.ended {
        QueueEnd::Started => "until it started".to_owned(),
        QueueEnd::Queued => "and was still queued when the period ended".to_owned(),
        QueueEnd::Failed { code } if code == "cancelled" => "until it was cancelled".to_owned(),
        QueueEnd::Failed { code } => format!("until it failed with {code}"),
    };
    let mut more = String::new();
    if queue.requests > 1 {
        more += &format!(
            "; {} of its requests queued an hour or more there",
            queue.requests
        );
    }
    match queues.len() - 1 {
        0 => (),
        1 => more += "; 1 other label did too",
        n => more += &format!("; {n} other labels did too"),
    }
    let Remedy { led_by, action } = remedy_for(queue.cause, &queue.outside);
    let cause = match queue.cause {
        Pressure::MemoryPressure => "memory pressure",
        Pressure::CpuSaturation => "CPU saturation",
    };
    Some(
        Finding::new(
            "queued_on_closed_gate",
            blocked_severity(queue.longest_wait_seconds),
            format!(
                "{subject} queued {} on {member} (from {}, {ended}) while {cause} from outside Elsecall{led_by} kept that member's gate closed{more}. {action} If the work may run on another member, request it placeable.",
                hours(queue.longest_wait_seconds),
                minute(queue.requested_at, None),
            ),
            "members.blocked_queues.longest_wait_seconds",
            queue.longest_wait_seconds as f64,
            QUEUED_SECONDS as f64,
        )
        .member(member)
        .label(queue.label.as_ref()),
    )
}

/// A finding for an early warning about an outside process on `member`. Its
/// threshold is the hours the warning takes to raise; who can restart the
/// process decides the remedy.
fn outside_warning(member: &str, period: &PeriodWarning) -> Finding {
    let warning = &period.warning;
    let gib = |bytes: u64| bytes as f64 / f64::from(1u32 << 30);
    let span = match period.until {
        Some(until) => format!(
            "from {} to {}",
            minute(warning.since, None),
            minute(until, Some(warning.since))
        ),
        None => format!(
            "from {}, still at the end of the period",
            minute(warning.since, None)
        ),
    };
    let name = &warning.process;
    let subject = format!(
        "Outside process `{name}` (pid {}, {}) on {member}",
        warning.pid,
        warning.owner.spoken()
    );
    let still = match warning.concern {
        Concern::Spinning { .. } => "that busy",
        Concern::Growing { .. } | Concern::Oversized { .. } => "that large",
    };
    let remedy = action(name, remedy::act(name, warning.owner), still);
    let hours = period.hours;
    let (kind, severity, message, threshold) = match warning.concern {
        Concern::Spinning { cpu_cores } => (
            "outside_process_spinning",
            if hours >= OUTSIDE_WARNING_HIGH_HOURS {
                Severity::High
            } else {
                Severity::Medium
            },
            format!(
                "{subject} kept about {cpu_cores:.1} cores busy for {hours:.1} h without a break ({span}), so Elsecall could admit that much less there. {remedy}"
            ),
            trend::SPIN_WINDOW,
        ),
        Concern::Growing {
            memory_bytes,
            growth_bytes_per_hour,
        } => (
            "outside_process_growing",
            if hours >= OUTSIDE_WARNING_HIGH_HOURS {
                Severity::High
            } else {
                Severity::Medium
            },
            format!(
                "{subject} grew to {:.1} GB by {:.1} GB an hour for {hours:.1} h ({span}); kept up, that ends in memory pressure that closes the member's gate. {remedy}",
                gib(memory_bytes),
                gib(growth_bytes_per_hour)
            ),
            trend::GROWTH_WINDOW,
        ),
        Concern::Oversized {
            memory_bytes,
            share_of_memory,
        } => (
            "outside_process_growing",
            Severity::High,
            format!(
                "{subject} held {:.1} GB, {} of the member's memory, for {hours:.1} h ({span}); a little more and memory pressure closes the member's gate. {remedy}",
                gib(memory_bytes),
                percent(share_of_memory)
            ),
            trend::OVERSIZED_FOR,
        ),
    };
    Finding::new(
        kind,
        severity,
        message,
        "members.outside_warnings.hours",
        hours,
        threshold.as_secs_f64() / 3600.0,
    )
    .member(member)
}

fn member_findings(member: &MemberMetrics, found: &mut Vec<Finding>) {
    found.extend(
        member
            .blocked
            .as_ref()
            .and_then(|b| outside_pressure(&member.member, b)),
    );
    found.extend(queued_on_closed_gate(
        &member.member,
        &member.blocked_queues,
    ));
    found.extend(
        member
            .outside_warnings
            .iter()
            .map(|warning| outside_warning(&member.member, warning)),
    );
    if let Some(load) = &member.load {
        if let Some(share) = load
            .saturated_busy_share
            .filter(|s| *s >= SATURATED_BUSY_SHARE)
        {
            found.push(
                Finding::new(
                    "work_while_saturated",
                    Severity::Medium,
                    format!(
                        "Elsecall work on {} ran {} of its time while outside load saturated the member ({} s).",
                        member.member,
                        percent(share),
                        load.saturated_busy_seconds
                    ),
                    "load.saturated_busy_share",
                    share,
                    SATURATED_BUSY_SHARE,
                )
                .member(&member.member),
            );
        }
        if member.master && load.saturated_share >= MASTER_SATURATED_SHARE {
            found.push(
                Finding::new(
                    "master_saturated",
                    Severity::High,
                    format!(
                        "The master, {}, was saturated by outside load {} of the time.",
                        member.member,
                        percent(load.saturated_share)
                    ),
                    "load.saturated_share",
                    load.saturated_share,
                    MASTER_SATURATED_SHARE,
                )
                .member(&member.member),
            );
        }
        if member.master && load.memory_pressure_seconds > 0 {
            found.push(
                Finding::new(
                    "master_memory_pressure",
                    Severity::High,
                    format!(
                        "The master, {}, was under memory pressure for {} s, so it admitted nothing new.",
                        member.member, load.memory_pressure_seconds
                    ),
                    "load.memory_pressure_seconds",
                    load.memory_pressure_seconds as f64,
                    0.0,
                )
                .member(&member.member),
            );
        }
    }
    if let Some(speed) = member.speed.as_ref().filter(|s| s.drift >= SPEED_DRIFT) {
        found.push(
            Finding::new(
                "speed_drift",
                Severity::Low,
                format!(
                    "{}'s measured speed ranged {:.2} to {:.2} ({} drift); estimates converted by speed move with it.",
                    member.member,
                    speed.min,
                    speed.max,
                    percent(speed.drift)
                ),
                "speed.drift",
                speed.drift,
                SPEED_DRIFT,
            )
            .member(&member.member),
        );
    }
}

/// A finding when a label's leftovers used notable CPU time after their
/// commands exited, or the cap often stopped them.
fn heavy_leftovers(label: &LabelLeftovers) -> Option<Finding> {
    let heavy_cpu = label.cpu_seconds >= LEFTOVERS_CPU_SECONDS;
    let often_capped =
        label.capped >= LEFTOVERS_CAPPED_RUNS && label.capped_share() >= LEFTOVERS_CAPPED_SHARE;
    if !heavy_cpu && !often_capped {
        return None;
    }
    let subject = match &label.label {
        Some(label) => format!("Label {label:?}"),
        None => "Unlabelled placed work".into(),
    };
    let message = format!(
        "{subject} leaves heavy work running after its command exits: {} run{} left processes behind, which used {:.0} s of CPU time afterwards outside admission{}. Wait for that work before exiting, or request it as work of its own.",
        label.runs,
        if label.runs == 1 { "" } else { "s" },
        label.cpu_seconds,
        if label.capped > 0 {
            format!(
                ", and the cap stopped {} of them ({})",
                label.capped,
                percent(label.capped_share())
            )
        } else {
            String::new()
        },
    );
    let severity = if heavy_cpu && often_capped {
        Severity::High
    } else {
        Severity::Medium
    };
    let found = if heavy_cpu {
        Finding::new(
            "heavy_leftovers",
            severity,
            message,
            "leftovers.by_label.cpu_seconds",
            label.cpu_seconds,
            LEFTOVERS_CPU_SECONDS,
        )
    } else {
        Finding::new(
            "heavy_leftovers",
            severity,
            message,
            "leftovers.by_label.capped",
            label.capped as f64,
            LEFTOVERS_CAPPED_RUNS as f64,
        )
    };
    Some(found.label(label.label.as_ref()))
}

/// The findings of `metrics`, and of what `gathered` lacks whatever the
/// filters: a member outside them still feeds the rest of the report.
pub fn findings(metrics: &Metrics, gathered: &Gathered) -> Vec<Finding> {
    let mut found: Vec<Finding> = gathered.members.iter().filter_map(incomplete).collect();
    if gathered.fleet_error.is_some() {
        found.push(
            Finding::new(
                "incomplete",
                Severity::High,
                "This member could not read the fleet's members, so the report covers this member alone; see fleet.fleet_error.".into(),
                "fleet",
                1.0,
                0.0,
            )
            .weight(f64::INFINITY),
        );
    }
    found.extend(metrics.demand.iter().filter(|d| crate::workload::demand::disagreement(d)).map(|d| {
        let value=d.effective_cores.unwrap();
        Finding::new("effective_demand", Severity::Medium,
            format!("Family {:?} on {} declares {:.2} cores but effective demand is {:.2} cores (interval {:?}; process-tree CPU {:?} cores). Check sizes against external service work before changing them.", d.family,d.member,d.declared_cores.unwrap(),value,d.interval_cores.unwrap(),d.process_tree_cores),
            "demand.effective_cores",value,d.declared_cores.unwrap())
            .member(&d.member).label(Some(&d.family))
    }));
    for member in &metrics.members {
        member_findings(member, &mut found);
    }
    let busy = &metrics.fleet.fleet_busy;
    if let Some(rate) = busy.rate.filter(|r| *r >= FLEET_BUSY_RATE) {
        found.push(Finding::new(
            "fleet_busy",
            if rate >= 2.0 * FLEET_BUSY_RATE {
                Severity::High
            } else {
                Severity::Medium
            },
            format!(
                "{} of {} interactive requests ({}) failed with fleet_busy.",
                busy.count,
                busy.of,
                percent(rate)
            ),
            "fleet.fleet_busy.rate",
            rate,
            FLEET_BUSY_RATE,
        ));
    }
    let balance = &metrics.fleet.balance;
    if let Some(share) = balance.imbalanced_share.filter(|s| *s >= IMBALANCED_SHARE) {
        found.push(Finding::new(
            "imbalance",
            Severity::Medium,
            format!(
                "In {} of periods one member was at {} CPU or more while another was at {} or less.",
                percent(share),
                percent(BUSY_CPU),
                percent(IDLE_CPU)
            ),
            "fleet.balance.imbalanced_share",
            share,
            IMBALANCED_SHARE,
        ));
    }
    if balance.fastest_idle_while_queued_seconds >= IDLE_WHILE_QUEUED_SECONDS
        && let Some(fastest) = &balance.fastest_member
    {
        found.push(
            Finding::new(
                "fastest_idle_while_queued",
                Severity::High,
                format!(
                    "The fastest member, {fastest}, ran no Elsecall work for {} s while requests queued on other members.",
                    balance.fastest_idle_while_queued_seconds
                ),
                "fleet.balance.fastest_idle_while_queued_seconds",
                balance.fastest_idle_while_queued_seconds as f64,
                IDLE_WHILE_QUEUED_SECONDS as f64,
            )
            .member(fastest),
        );
    }
    found.extend(
        metrics
            .leftovers
            .by_label
            .iter()
            .filter_map(heavy_leftovers),
    );
    let accuracy = &metrics.accuracy;
    let estimates = [
        ("wait_estimate", "wait", &accuracy.wait.by_member),
        ("sync_estimate", "sync", &accuracy.sync.by_member),
        ("run_estimate", "run", &accuracy.run.by_label),
    ];
    for (kind, what, groups) in estimates {
        found.extend(groups.iter().filter_map(|a| estimate(kind, what, a)));
    }
    for conversion in &accuracy.speed_conversion {
        let off = conversion.error.max(1.0 / conversion.error);
        // How far conversion would put the slower member's run from what it took.
        let [faster, slower] = conversion.median_seconds;
        let missed = (faster * conversion.converted_ratio - slower).abs();
        if off >= SPEED_CONVERSION && missed >= ESTIMATE_SECONDS {
            let by = match &conversion.learned {
                Some(learned) => format!(
                    "the ratio learned from {} other label{} says {:.2}× (measured speeds {:.2}×)",
                    learned.labels,
                    if learned.labels == 1 { "" } else { "s" },
                    learned.ratio,
                    conversion.measured_ratio
                ),
                None => format!(
                    "their measured speeds say {:.2}×",
                    conversion.measured_ratio
                ),
            };
            found.push(
                Finding::new(
                    "speed_conversion",
                    Severity::Low,
                    format!(
                        "Label {:?} runs {:.2}× as long on {} as on {}, but {by}.",
                        conversion.label,
                        conversion.observed_ratio,
                        conversion.slower,
                        conversion.faster,
                    ),
                    "accuracy.speed_conversion.error",
                    conversion.error,
                    SPEED_CONVERSION,
                )
                .label(Some(&conversion.label))
                .weight(off / SPEED_CONVERSION),
            );
        }
    }
    found.sort_by(|a, b| {
        b.severity
            .cmp(&a.severity)
            .then(b.weight.total_cmp(&a.weight))
            .then(a.kind.cmp(b.kind))
            .then(a.member.cmp(&b.member))
            .then(a.label.cmp(&b.label))
    });
    found.truncate(MAXIMUM);
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        outside_pressure::sampling::Owner,
        reporting::{
            document::{Gathered, MemberInput, Query, RecordsRead, document},
            facts::Fact,
            fixtures::{self, MINUTE, Run, T0, bucket},
            leftovers::LeftoverFact,
            window::Window,
        },
        scheduling::{
            placement::Source,
            record::{Outcome, Verdict},
            request::Placement,
            timestamp::Timestamp,
        },
    };

    /// A fleet with one of every problem.
    fn troubled() -> Gathered {
        let mut records = Vec::new();
        // "build" on the devbox: predicted 10 s on the prior, took 30 s.
        for n in 1..=3 {
            records.push(fixtures::placed(Run {
                n,
                member: "devbox",
                run_from: Source::Prior,
                actual_run: 30.0,
                ..Run::default()
            }));
        }
        // Waits on the mini predicted 0 s, and 20 s each.
        for n in 4..=6 {
            records.push(fixtures::placed(Run {
                n,
                predicted_wait: 0.0,
                waited: 20.0,
                ..Run::default()
            }));
        }
        // One fleet_busy of eight decided.
        let mut busy = fixtures::base(7, "test", "mini");
        busy.placement = Placement::Placeable;
        busy.outcome = Some(Outcome::Failed {
            code: "fleet_busy".into(),
        });
        busy.decision = Some(fixtures::decision(Verdict::FleetBusy, Vec::new(), None));
        records.push(busy);
        records.push(fixtures::pinned(8, "test", "mini", 4.0));
        // The mini is saturated and busy, the faster devbox idle while it queues.
        let mini: Vec<_> = (0..10)
            .map(|m| {
                let mut b = bucket(m, 0.95, 4.0, 2, 30, 1.0);
                b.memory_pressure_seconds = u64::from(m == 0);
                b
            })
            .collect();
        let devbox: Vec<_> = (0..10)
            .map(|m| bucket(m, 0.1, 0.0, 0, 0, if m == 0 { 1.0 } else { 1.5 }))
            .collect();
        Gathered {
            this_member: "mini".into(),
            master: Some("mini".into()),
            fleet_error: None,
            members: vec![
                MemberInput {
                    member: "devbox".into(),
                    outside: Ok(Default::default()),
                    load: Ok(devbox),
                    records: Ok(RecordsRead {
                        requests: 0,
                        truncated: false,
                        dropped: 0,
                    }),
                },
                MemberInput {
                    member: "mini".into(),
                    outside: Ok(Default::default()),
                    load: Ok(mini),
                    records: Ok(RecordsRead {
                        requests: 8,
                        truncated: true,
                        dropped: 0,
                    }),
                },
            ],
            facts: records.iter().filter_map(Fact::of).collect(),
            leftovers: Vec::new(),
        }
    }

    fn leftover(n: u64, label: &str, cpu_seconds: u64, capped: bool) -> LeftoverFact {
        LeftoverFact {
            run_id: format!("r-{n:012x}"),
            label: Some(label.into()),
            member: "devbox".into(),
            calling_member: "mini".into(),
            requested_at: Timestamp::from_millis(T0 + MINUTE),
            duration_ms: Some(1_000),
            cpu_ms: Some(cpu_seconds * 1000),
            capped,
        }
    }

    #[test]
    fn labels_that_leave_heavy_work_running_are_findings() {
        let mut gathered = troubled();
        gathered.facts.clear();
        for member in &mut gathered.members {
            member.load = Ok(vec![bucket(0, 0.2, 0.0, 0, 0, 1.0)]);
            member.records = Ok(RecordsRead::default());
        }
        gathered.leftovers = vec![
            // "ingest" used 400 s of CPU time afterwards.
            leftover(1, "ingest", 250, false),
            leftover(2, "ingest", 150, false),
            // The cap stopped two of three "watch" runs' leftovers.
            leftover(3, "watch", 1, true),
            leftover(4, "watch", 1, true),
            leftover(5, "watch", 1, false),
            // Light, and capped once only.
            leftover(6, "test", 20, true),
            leftover(7, "test", 2, false),
        ];
        let findings = document(&query(), &gathered).findings;
        let labels: Vec<_> = findings
            .iter()
            .map(|f| (f.kind, f.label.as_deref(), f.metric))
            .collect();
        assert_eq!(
            labels,
            [
                (
                    "heavy_leftovers",
                    Some("ingest"),
                    "leftovers.by_label.cpu_seconds"
                ),
                (
                    "heavy_leftovers",
                    Some("watch"),
                    "leftovers.by_label.capped"
                ),
            ],
            "{findings:#?}"
        );
        assert_eq!(
            findings[0].message,
            "Label \"ingest\" leaves heavy work running after its command exits: 2 runs left processes behind, which used 400 s of CPU time afterwards outside admission. Wait for that work before exiting, or request it as work of its own."
        );
        assert!(
            findings[1]
                .message
                .contains("the cap stopped 2 of them (67%)"),
            "{}",
            findings[1].message
        );
        // The label filter applies to them too.
        let mut only_test = query();
        only_test.label = Some("te*".into());
        let report = document(&only_test, &gathered);
        assert_eq!(report.metrics.leftovers.runs, 2);
        assert!(report.findings.is_empty(), "{:#?}", report.findings);
    }

    #[test]
    fn outside_processes_that_spun_or_grew_are_findings_with_their_remedy() {
        use crate::{
            outside_pressure::warning::{Warning, WarningEpisode},
            reporting::outside::OutsideHistory,
        };
        const HOUR: u64 = 60 * MINUTE;
        let at = |ms: u64| Timestamp::from_millis(ms);
        // An hour two days after T0.
        let start = T0 + 48 * HOUR;
        let window = Window::new(at(start), at(start + HOUR), at(start + HOUR)).unwrap();
        let mut query = query();
        query.window = window;
        query.series_width = window.fine_width();
        let episode =
            |process: &str, owner, concern, since: u64, until: Option<u64>| WarningEpisode {
                warning: Warning {
                    concern,
                    process: process.into(),
                    pid: 412,
                    started: at(T0),
                    owner,
                    since: at(since),
                    observed_at: at(until.unwrap_or(start + 55 * MINUTE)),
                },
                until: until.map(at),
            };
        let mut gathered = troubled();
        gathered.facts.clear();
        for member in &mut gathered.members {
            member.load = Ok(vec![]);
            member.records = Ok(RecordsRead::default());
        }
        gathered.members[0].outside = Ok(OutsideHistory {
            warnings: vec![
                episode(
                    "fseventsd",
                    Owner::Root,
                    Concern::Spinning { cpu_cores: 1.02 },
                    start - 30 * HOUR,
                    None,
                ),
                episode(
                    "node",
                    Owner::ThisUser,
                    Concern::Growing {
                        memory_bytes: 9 << 30,
                        growth_bytes_per_hour: 1 << 30,
                    },
                    start - 7 * HOUR,
                    Some(start + 30 * MINUTE),
                ),
                // Ended before the period.
                episode(
                    "old",
                    Owner::Root,
                    Concern::Spinning { cpu_cores: 1.0 },
                    T0,
                    Some(T0 + 10 * HOUR),
                ),
            ],
            ..Default::default()
        });
        gathered.members[1].outside = Ok(OutsideHistory {
            warnings: vec![episode(
                "mds_stores",
                Owner::OtherUser,
                Concern::Oversized {
                    memory_bytes: 20 << 30,
                    share_of_memory: 0.31,
                },
                start - 2 * HOUR,
                None,
            )],
            ..Default::default()
        });
        let report = document(&query, &gathered);
        let found: Vec<(&str, Severity, Option<&str>, f64, f64)> = report
            .findings
            .iter()
            .map(|f| {
                (
                    f.kind,
                    f.severity,
                    f.member.as_deref(),
                    f.value,
                    f.threshold,
                )
            })
            .collect();
        assert_eq!(
            found,
            [
                (
                    "outside_process_spinning",
                    Severity::High,
                    Some("devbox"),
                    31.0,
                    6.0
                ),
                (
                    "outside_process_growing",
                    Severity::High,
                    Some("mini"),
                    3.0,
                    1.0
                ),
                (
                    "outside_process_growing",
                    Severity::Medium,
                    Some("devbox"),
                    7.5,
                    6.0
                ),
            ],
            "{:#?}",
            report.findings
        );
        let messages: Vec<&str> = report.findings.iter().map(|f| f.message.as_str()).collect();
        assert_eq!(
            messages,
            [
                "Outside process `fseventsd` (pid 412, root) on devbox kept about 1.0 cores busy for 31.0 h without a break \
                 (from 1970-01-02T10:40Z, still at the end of the period), so Elsecall could admit that much less there. \
                 Restart `fseventsd` on that member if it is still that busy.",
                "Outside process `mds_stores` (pid 412, another user) on mini held 20.0 GB, 31% of the member's memory, for 3.0 h \
                 (from 1970-01-03T14:40Z, still at the end of the period); a little more and memory pressure closes the member's gate. \
                 Ask the user who runs `mds_stores` on that member to quit it if it is still that large.",
                "Outside process `node` (pid 412, this user) on devbox grew to 9.0 GB by 1.0 GB an hour for 7.5 h \
                 (from 1970-01-03T09:40Z to 17:10Z); kept up, that ends in memory pressure that closes the member's gate. \
                 Quit `node` on that member if it is still that large.",
            ]
        );
        let devbox = serde_json::to_value(&report.metrics.members[0]).unwrap();
        assert_eq!(
            devbox["outside_warnings"][0],
            serde_json::json!({
                "concern": "spinning", "cpu_cores": 1.02, "process": "fseventsd", "pid": 412,
                "started": "1970-01-01T16:40:00.000Z", "owner": "root",
                "since": "1970-01-02T10:40:00.000Z", "observed_at": "1970-01-03T17:35:00.000Z",
                "hours": 31.0
            })
        );
        assert_eq!(devbox["outside_warnings"].as_array().unwrap().len(), 2);
        assert!(devbox.get("outside_error").is_none());
    }

    fn query() -> Query {
        let until = Timestamp::from_millis(T0 + 60 * MINUTE);
        let window = Window::new(Timestamp::from_millis(T0), until, until).unwrap();
        Query {
            window,
            series_width: window.fine_width(),
            member: None,
            label: None,
            parameters: Default::default(),
        }
    }

    #[test]
    fn the_most_severe_come_first_and_the_list_is_short() {
        let findings = document(&query(), &troubled()).findings;
        let kinds: Vec<(&str, Severity)> = findings.iter().map(|f| (f.kind, f.severity)).collect();
        // Weights: incomplete first, waits 20 s against 2 s (10), the master
        // saturated half the time (5), 12.5% fleet_busy (2.5); 600 s idle, runs
        // 3× and syncs ⅓ of their estimates (2 each, by kind); memory pressure.
        assert_eq!(
            kinds,
            [
                ("incomplete", Severity::High),
                ("wait_estimate", Severity::High),
                ("master_saturated", Severity::High),
                ("fleet_busy", Severity::High),
                ("fastest_idle_while_queued", Severity::High),
                ("run_estimate", Severity::High),
                ("sync_estimate", Severity::High),
                ("master_memory_pressure", Severity::High),
                ("imbalance", Severity::Medium),
                ("work_while_saturated", Severity::Medium),
            ],
            "the low findings are cut at {MAXIMUM}"
        );
        let run = findings.iter().find(|f| f.kind == "run_estimate").unwrap();
        assert_eq!(run.label.as_deref(), Some("build"));
        assert_eq!(run.member.as_deref(), Some("devbox"));
        assert_eq!(
            (run.metric, run.value, run.threshold),
            ("ratio_median", 3.0, 1.5)
        );
        assert_eq!(
            run.message,
            "Label \"build\" run estimates on devbox are 3.0× too low: median actual 30.0 s against 10.0 s predicted, over 3 samples, 3 of them priors."
        );
        let wait = findings.iter().find(|f| f.kind == "wait_estimate").unwrap();
        assert_eq!(wait.metric, "median_difference_seconds");
        assert_eq!(wait.value, 20.0);
        assert!(
            wait.message
                .starts_with("Wait estimates on mini are too low")
        );
        let busy = findings.iter().find(|f| f.kind == "fleet_busy").unwrap();
        assert_eq!(busy.value, 1.0 / 8.0);
        let sync = findings.iter().find(|f| f.kind == "sync_estimate").unwrap();
        assert!(
            sync.message
                .starts_with("Warm sync estimates on devbox are 3.0× too high")
        );
        assert!(
            findings
                .iter()
                .find(|f| f.kind == "incomplete")
                .unwrap()
                .message
                .starts_with("Only the latest 8 of mini's requests")
        );
    }

    #[test]
    fn speed_findings_are_low() {
        let mut gathered = troubled();
        // Keep the devbox's builds and add the mini's, 10 s each as predicted.
        gathered.facts.truncate(3);
        for n in 10..=12 {
            gathered.facts.push(
                Fact::of(&fixtures::placed(Run {
                    n,
                    ..Run::default()
                }))
                .unwrap(),
            );
        }
        for member in &mut gathered.members {
            member.records = Ok(RecordsRead::default());
            if let Ok(load) = &mut member.load {
                for b in load.iter_mut() {
                    *b = bucket(
                        (b.at.millis() - T0) / MINUTE,
                        0.2,
                        0.0,
                        0,
                        0,
                        b.speed_min.unwrap(),
                    );
                }
            }
        }
        let findings = document(&query(), &gathered).findings;
        let low: Vec<&Finding> = findings
            .iter()
            .filter(|f| f.severity == Severity::Low)
            .collect();
        assert_eq!(low.len(), 2, "{findings:#?}");
        // The devbox measures faster (1.45 against 1.0) but ran builds 3× as long.
        assert_eq!(low[0].kind, "speed_conversion");
        assert_eq!(low[0].label.as_deref(), Some("build"));
        assert!(
            low[0]
                .message
                .contains("runs 0.33× as long on mini as on devbox"),
            "{}",
            low[0].message
        );
        assert_eq!(low[1].kind, "speed_drift");
        assert_eq!(low[1].member.as_deref(), Some("devbox"));
        assert!((low[1].value - 0.5 / 1.5).abs() < 1e-9);
    }

    #[test]
    fn speed_conversion_findings_name_the_learned_ratio_and_skip_short_runs() {
        let mut gathered = troubled();
        gathered.facts.clear();
        let mut n = 100;
        // Without predictions, so no run estimate findings crowd them out.
        let mut pinned = |label: &str, member: &str, run: f64| {
            n += 1;
            let mut fact = Fact::of(&fixtures::pinned(n, label, member, run)).unwrap();
            fact.predicted = None;
            fact
        };
        // On the mini and the devbox: "a" 10 and 20 s, "b" 10 and 22 s, "c" 10
        // and 30 s, and "quick" 0.01 and 0.05 s.
        for (label, mini, devbox) in [
            ("a", 10.0, 20.0),
            ("b", 10.0, 22.0),
            ("c", 10.0, 30.0),
            ("quick", 0.01, 0.05),
        ] {
            for _ in 0..3 {
                let facts = [pinned(label, "mini", mini), pinned(label, "devbox", devbox)];
                gathered.facts.extend(facts);
            }
        }
        gathered.master = None;
        for member in &mut gathered.members {
            member.records = Ok(RecordsRead::default());
            if let Ok(load) = &mut member.load {
                for b in load.iter_mut() {
                    let speed = b.speed_min.unwrap();
                    *b = bucket((b.at.millis() - T0) / MINUTE, 0.2, 0.0, 0, 0, speed);
                }
            }
        }
        let findings = document(&query(), &gathered).findings;
        let conversions: Vec<&Finding> = findings
            .iter()
            .filter(|f| f.kind == "speed_conversion")
            .collect();
        // The devbox measures faster. "c" is 1.43× off the 0.48× "a" and "b"
        // teach, which misses by 4.3 s; "a" is 1.29× off, missing by 2.2 s; "b"
        // is close; "quick" misses by under 2 s.
        let labels: Vec<_> = conversions.iter().map(|f| f.label.as_deref()).collect();
        assert_eq!(labels, [Some("c"), Some("a")], "{findings:#?}");
        assert!(
            conversions[0]
                .message
                .contains("runs 0.33× as long on mini as on devbox, but the ratio learned from 2 other labels says 0.48× (measured speeds"),
            "{}",
            conversions[0].message
        );
    }

    #[test]
    fn small_or_close_estimates_are_not_findings() {
        let mut gathered = troubled();
        gathered
            .facts
            .retain(|f| f.member.as_deref() != Some("devbox"));
        // Two samples only.
        gathered.facts.truncate(2);
        let findings = document(&query(), &gathered).findings;
        assert!(findings.iter().all(|f| !f.kind.ends_with("_estimate")));
    }

    #[test]
    fn a_quiet_fleet_has_none() {
        let mut gathered = troubled();
        gathered.facts.clear();
        for member in &mut gathered.members {
            member.load = Ok(vec![bucket(0, 0.2, 0.0, 0, 0, 1.0)]);
            member.records = Ok(RecordsRead::default());
        }
        assert_eq!(document(&query(), &gathered).findings, []);
    }

    /// 2026-11-10T03:00Z, eleven hours before the incident's report.
    fn incident_start() -> Timestamp {
        Timestamp::parse_rfc3339("2026-11-10T03:00:00Z").unwrap()
    }

    fn incident_at(seconds: u64) -> Timestamp {
        incident_start().after(std::time::Duration::from_secs(seconds))
    }

    /// The eleven hours from 03:00Z, in minutes.
    fn incident_query() -> Query {
        let until = incident_at(660 * 60);
        let window = Window::new(incident_start(), until, until).unwrap();
        assert_eq!(window.fine_width().as_secs(), 60);
        Query {
            window,
            series_width: window.fine_width(),
            member: None,
            label: None,
            parameters: Default::default(),
        }
    }

    /// A member's minutes over the eleven hours: `closed` says, per minute, which
    /// gate was closed all minute, and `in_use` the cores Elsecall held.
    fn minutes(
        closed: impl Fn(u64) -> Option<Pressure>,
        in_use: impl Fn(u64) -> f64,
    ) -> Vec<crate::load_log::buckets::Bucket> {
        (0..660)
            .map(|m| {
                let mut b = match closed(m) {
                    Some(Pressure::CpuSaturation) => bucket(0, 0.97, in_use(m), 0, 60, 1.0),
                    _ => bucket(0, 0.15, in_use(m), 0, 0, 1.0),
                };
                b.at = incident_at(m * 60);
                if closed(m) == Some(Pressure::MemoryPressure) {
                    b.memory_pressure_seconds = 60;
                    b.worst_memory_pressure = Some(crate::inspection::MemoryPressure::Warning);
                }
                b
            })
            .collect()
    }

    /// A deferrable request pinned to `member`, made `at` seconds after 03:00Z,
    /// that never started and ended `waited` seconds later with `outcome`.
    fn queued(
        n: u64,
        label: &str,
        member: &str,
        at: u64,
        waited: u64,
        outcome: Outcome,
    ) -> crate::scheduling::record::Record {
        let mut record = fixtures::base(n, label, member);
        record.kind = crate::scheduling::request::Kind::Lease;
        record.urgency = crate::scheduling::request::Urgency::Deferrable {
            deadline: incident_at(at + 1800),
            at_deadline: crate::scheduling::request::AtDeadline::Start,
        };
        record.requested_at = incident_at(at);
        record.ended_at = Some(incident_at(at + waited));
        record.outcome = Some(outcome);
        record
    }

    /// The 2026-11-10 incident: memory pressure closed macbook-pro's gate from
    /// 04:48Z to 13:05Z while Elsecall held nothing there, and an Acme warm
    /// lease queued behind it for 5.2 h until its caller gave up. Elsecall's own
    /// housekeeping, skipped at its deadline after an hour, is no finding.
    fn incident() -> Gathered {
        let macbook = minutes(
            |m| (108..605).contains(&m).then_some(Pressure::MemoryPressure),
            |_| 0.0,
        );
        let mini = minutes(|_| None, |_| 0.0);
        let cancelled = Outcome::Cancelled {
            reason: "The caller exited while waiting.".into(),
        };
        let mut housekeeping = queued(
            2,
            "elsecall housekeeping",
            "macbook-pro",
            2 * 3600 + 12 * 60,
            3600,
            Outcome::Failed {
                code: "deadline_skipped".into(),
            },
        );
        housekeeping.kind = crate::scheduling::request::Kind::Run;
        housekeeping.internal = true;
        let records = [
            queued(
                1,
                "acme warm Debug",
                "macbook-pro",
                4 * 3600 + 47 * 60 + 8,
                5 * 3600 + 10 * 60 + 43,
                cancelled,
            ),
            housekeeping,
        ];
        let input = |member: &str, load| MemberInput {
            member: member.into(),
            outside: Ok(Default::default()),
            load: Ok(load),
            records: Ok(RecordsRead::default()),
        };
        Gathered {
            this_member: "mac-mini".into(),
            master: Some("mac-mini".into()),
            fleet_error: None,
            members: vec![
                input("mac-mini", mini),
                MemberInput {
                    // What the member recorded: fseventsd hung at 56 GB.
                    outside: Ok(crate::reporting::outside::OutsideHistory {
                        episodes: vec![fixtures::episode(
                            incident_at(108 * 60),
                            Some(incident_at(490 * 60)),
                            &[("fseventsd", 56 << 30, 1.0), ("mds_stores", 2 << 30, 0.1)],
                        )],
                        ..Default::default()
                    }),
                    ..input("macbook-pro", macbook)
                },
            ],
            facts: records.iter().filter_map(Fact::of).collect(),
            leftovers: Vec::new(),
        }
    }

    #[test]
    fn a_member_outside_pressure_blocked_for_hours_and_what_queued_behind_it_are_findings() {
        let findings = document(&incident_query(), &incident()).findings;
        let kinds: Vec<_> = findings
            .iter()
            .map(|f| (f.kind, f.severity, f.metric, f.value, f.threshold))
            .collect();
        assert_eq!(
            kinds,
            [
                (
                    "outside_pressure_blocked",
                    Severity::High,
                    "members.blocked.seconds",
                    29_820.0,
                    3600.0
                ),
                (
                    "queued_on_closed_gate",
                    Severity::High,
                    "members.blocked_queues.longest_wait_seconds",
                    18_643.0,
                    3600.0
                ),
            ],
            "{findings:#?}"
        );
        assert_eq!(
            findings[0].message,
            "macbook-pro admitted nothing for 8.3 h (2026-11-10T04:48Z to 13:05Z): memory pressure from outside Elsecall, led by `fseventsd` (56.0 GB resident), closed its gate while Elsecall held no work there. Restart `fseventsd` on that member if it is still that large. While the pressure lasts, placement skips the member and work pinned there waits."
        );
        assert_eq!(findings[0].member.as_deref(), Some("macbook-pro"));
        assert_eq!(
            findings[1].message,
            "Label \"acme warm Debug\" queued 5.2 h on macbook-pro (from 2026-11-10T07:47Z, until it was cancelled) while memory pressure from outside Elsecall, led by `fseventsd` (56.0 GB resident), kept that member's gate closed. Restart `fseventsd` on that member if it is still that large. If the work may run on another member, request it placeable."
        );
        assert_eq!(findings[1].label.as_deref(), Some("acme warm Debug"));

        // A member that could not say what held it closed: the remedy is generic.
        let mut unnamed = incident();
        unnamed.members[1].outside = Err(crate::failure::Failure::new(
            "member_unavailable",
            "macbook-pro's outside pressure history could not be read.",
            "Check the member with elsecall fleet.",
        ));
        let unnamed_report = document(&incident_query(), &unnamed);
        assert_eq!(
            unnamed_report.metrics.members[1]
                .outside_error
                .as_ref()
                .unwrap()
                .code,
            "member_unavailable"
        );
        let findings = unnamed_report.findings;
        assert!(
            findings[0].message.ends_with("while Elsecall held no work there. Free memory on that member. While the pressure lasts, placement skips the member and work pinned there waits."),
            "{}",
            findings[0].message
        );
        assert!(
            findings[1].message.ends_with("kept that member's gate closed. Free memory on that member. If the work may run on another member, request it placeable."),
            "{}",
            findings[1].message
        );

        let report = serde_json::to_value(document(&incident_query(), &incident())).unwrap();
        let macbook = &report["members"][1];
        assert_eq!(macbook["blocked"]["since"], "2026-11-10T04:48:00.000Z");
        assert_eq!(macbook["blocked"]["cause"], "memory_pressure");
        assert_eq!(macbook["blocked"]["worst_memory_pressure"], "warning");
        assert_eq!(
            macbook["blocked_queues"][0]["ended"],
            serde_json::json!({"state": "failed", "code": "cancelled"})
        );
        assert_eq!(macbook["blocked_queues"][0]["requests"], 1);
        assert_eq!(macbook["blocked"]["outside"][0]["name"], "fseventsd");
        assert_eq!(macbook["blocked"]["outside"][1]["name"], "mds_stores");
        assert_eq!(
            macbook["blocked_queues"][0]["outside"][0]["name"],
            "fseventsd"
        );
        assert!(report["members"][0]["blocked"].is_null());
    }

    #[test]
    fn short_or_own_pressure_and_queues_on_an_open_gate_are_not_findings() {
        // A 50-minute spike, with the lease queued 5.2 h mostly on an open gate.
        let mut spike = incident();
        spike.members[1].load = Ok(minutes(
            |m| (108..158).contains(&m).then_some(Pressure::MemoryPressure),
            |_| 0.0,
        ));
        let report = document(&incident_query(), &spike);
        assert_eq!(
            report.metrics.members[1].blocked.as_ref().unwrap().seconds,
            3000
        );
        assert!(report.metrics.members[1].blocked_queues.is_empty());
        assert_eq!(report.findings, []);

        // Hours of pressure while Elsecall's own work held cores: its own, maybe.
        let mut own = incident();
        own.members[1].load = Ok(minutes(
            |m| (108..605).contains(&m).then_some(Pressure::MemoryPressure),
            |_| 4.0,
        ));
        own.facts.clear();
        assert!(
            document(&incident_query(), &own).metrics.members[1]
                .blocked
                .is_none()
        );
        assert_eq!(document(&incident_query(), &own).findings, []);

        // The same lease placeable waits for any member, not this one's gate.
        let mut placeable = incident();
        placeable.facts[0].placement = crate::reporting::facts::PlacementKind::Placeable;
        let findings = document(&incident_query(), &placeable).findings;
        assert!(
            findings.iter().all(|f| f.kind != "queued_on_closed_gate"),
            "{findings:#?}"
        );
    }

    #[test]
    fn an_hour_is_the_threshold_and_four_hours_are_high() {
        let blocked_for = |minutes_closed: u64| {
            let mut gathered = incident();
            gathered.facts.clear();
            gathered.members[1].load = Ok(minutes(
                |m| {
                    (60..60 + minutes_closed)
                        .contains(&m)
                        .then_some(Pressure::MemoryPressure)
                },
                |_| 0.0,
            ));
            document(&incident_query(), &gathered)
                .findings
                .into_iter()
                .map(|f| (f.kind, f.severity))
                .collect::<Vec<_>>()
        };
        assert_eq!(blocked_for(59), []);
        assert_eq!(
            blocked_for(60),
            [("outside_pressure_blocked", Severity::Medium)]
        );
        assert_eq!(
            blocked_for(239),
            [("outside_pressure_blocked", Severity::Medium)]
        );
        assert_eq!(
            blocked_for(240),
            [("outside_pressure_blocked", Severity::High)]
        );
    }

    #[test]
    fn every_label_queued_behind_one_member_makes_one_finding() {
        let mut gathered = incident();
        let cancelled = || Outcome::Cancelled {
            reason: "The caller exited while waiting.".into(),
        };
        let extra = [
            queued(3, "acme warm Debug", "macbook-pro", 7200, 3600, cancelled()),
            queued(
                4,
                "acme warm Release",
                "macbook-pro",
                7200,
                4000,
                cancelled(),
            ),
            queued(5, "acme test", "macbook-pro", 7200, 5000, cancelled()),
        ];
        gathered.facts.extend(extra.iter().filter_map(Fact::of));
        let findings = document(&incident_query(), &gathered).findings;
        let queued: Vec<&Finding> = findings
            .iter()
            .filter(|f| f.kind == "queued_on_closed_gate")
            .collect();
        assert_eq!(queued.len(), 1, "{findings:#?}");
        assert_eq!(queued[0].label.as_deref(), Some("acme warm Debug"));
        assert!(
            queued[0].message.contains(
                "until it was cancelled) while memory pressure from outside Elsecall, led by `fseventsd` (56.0 GB resident), kept that member's gate closed; 2 of its requests queued an hour or more there; 2 other labels did too."
            ),
            "{}",
            queued[0].message
        );
    }

    #[test]
    fn the_remedy_names_the_leading_process_and_suits_its_owner() {
        use crate::outside_pressure::sampling::Owner;
        let process = |name: &str, owner, footprint| {
            let mut episode = fixtures::episode(incident_start(), None, &[(name, 3 << 29, 2.5)]);
            let mut process = episode.blocked.outside.remove(0);
            process.owner = owner;
            process.memory.footprint_bytes = footprint;
            process
        };
        let cases = [
            (
                Pressure::MemoryPressure,
                process("fseventsd", Owner::Root, None),
                ", led by `fseventsd` (1.5 GB resident),",
                "Restart `fseventsd` on that member if it is still that large.",
            ),
            (
                Pressure::MemoryPressure,
                process("Xcode", Owner::ThisUser, Some(5 << 29)),
                ", led by `Xcode` (2.5 GB footprint),",
                "Quit `Xcode` on that member if it is still that large.",
            ),
            (
                Pressure::CpuSaturation,
                process("node", Owner::OtherUser, None),
                ", led by `node` (2.5 cores),",
                "Ask the user who runs `node` on that member to quit it if it is still that busy.",
            ),
            (
                Pressure::CpuSaturation,
                process("WindowServer", Owner::Root, None),
                ", led by `WindowServer` (2.5 cores),",
                "`WindowServer` cannot be restarted without ending the session: close the apps that use the most memory or CPU on that member, or reboot it.",
            ),
        ];
        for (pressure, process, led_by, action) in cases {
            let remedy = remedy_for(pressure, &[process]);
            assert_eq!(
                (remedy.led_by.as_str(), remedy.action.as_str()),
                (led_by, action)
            );
        }
    }

    #[test]
    fn outside_cpu_saturation_names_its_leading_process() {
        let mut saturated = incident();
        saturated.facts.clear();
        saturated.members[1].load = Ok(minutes(
            |m| (60..210).contains(&m).then_some(Pressure::CpuSaturation),
            |_| 0.0,
        ));
        let mut episode =
            fixtures::episode(incident_at(3600), None, &[("mds_stores", 1 << 30, 7.9)]);
        episode.blocked.reason = crate::outside_pressure::blocked::Reason::OutsideSaturation {
            outside_utilization: 0.97,
        };
        saturated.members[1].outside = Ok(crate::reporting::outside::OutsideHistory {
            episodes: vec![episode],
            ..Default::default()
        });
        let findings = document(&incident_query(), &saturated).findings;
        let blocked = findings
            .iter()
            .find(|f| f.kind == "outside_pressure_blocked")
            .unwrap();
        assert_eq!(
            blocked.message,
            "macbook-pro admitted nothing for 2.5 h (2026-11-10T04:00Z to 06:30Z): CPU saturation from outside Elsecall (outside load averaged 97%), led by `mds_stores` (7.9 cores), closed its gate. Restart `mds_stores` on that member if it is still that busy. While the pressure lasts, placement skips the member and work pinned there waits."
        );
    }

    #[test]
    fn outside_cpu_saturation_for_hours_is_a_finding_too() {
        let mut saturated = incident();
        saturated.members[1].load = Ok(minutes(
            |m| (60..210).contains(&m).then_some(Pressure::CpuSaturation),
            |_| 0.0,
        ));
        saturated.facts.clear();
        let findings = document(&incident_query(), &saturated).findings;
        // The mini idles meanwhile, which is imbalance as well.
        let findings: Vec<_> = findings
            .into_iter()
            .filter(|f| f.kind != "imbalance")
            .collect();
        assert_eq!(findings.len(), 1, "{findings:#?}");
        assert_eq!(findings[0].severity, Severity::Medium);
        assert_eq!(
            findings[0].message,
            "macbook-pro admitted nothing for 2.5 h (2026-11-10T04:00Z to 06:30Z): CPU saturation from outside Elsecall (outside load averaged 97%) closed its gate. Stop or lower the outside CPU load on that member. While the pressure lasts, placement skips the member and work pinned there waits."
        );
    }
}
