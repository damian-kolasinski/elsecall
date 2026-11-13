//! When outside pressure kept a member's gate closed: the longest stretch in
//! which the member admitted nothing for it, and the requests that queued for
//! long behind it.
//!
//! The gate's outside CPU saturation is attributed when sampled. Memory
//! pressure is not, so it counts as outside only where Elsecall's own grants
//! held (almost) nothing: pressure its own work may have caused is not reported
//! as outside. Time without samples, such as a sleeping laptop, proves nothing
//! either way, so too little of it sampled is not blocked.
use crate::{
    inspection::MemoryPressure,
    load_log::{
        buckets::Bucket,
        minute::{memory_rank, worst},
    },
    outside_pressure::{blocked::OutsideProcess, store::Episode},
    reporting::{
        facts::{Fact, PlacementKind},
        outside,
        window::Window,
    },
    scheduling::timestamp::Timestamp,
};
use serde::Serialize;
use std::{collections::BTreeMap, time::Duration};

/// Outside pressure closed the gate for at least this share of the seconds
/// sampled…
pub const CLOSED_SHARE: f64 = 0.9;
/// …which are at least this share of the time judged…
pub const SAMPLED_SHARE: f64 = 0.5;
/// …and memory pressure counts as outside where Elsecall work held cores for at
/// most this share of a bucket's sampled seconds.
pub const HELD_SHARE: f64 = 0.1;
/// Requests that waited at least this long count as queued behind a gate.
pub const QUEUED_SECONDS: u64 = 3600;

/// What closed the gate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Pressure {
    MemoryPressure,
    CpuSaturation,
}

/// Seconds sampled and seconds outside pressure closed the gate, over some
/// buckets, each weighted by how much of it counts.
#[derive(Default)]
struct Closed {
    sampled: f64,
    memory: f64,
    saturated: f64,
}

impl Closed {
    fn add(&mut self, bucket: &Bucket, weight: f64) {
        let held = bucket.busy_seconds as f64 > HELD_SHARE * bucket.seconds as f64;
        self.sampled += bucket.seconds as f64 * weight;
        if !held {
            self.memory += bucket.memory_pressure_seconds as f64 * weight;
        }
        self.saturated += bucket.saturated_seconds as f64 * weight;
    }

    /// Whether outside pressure kept the gate closed over `seconds` of wall time.
    fn throughout(&self, seconds: f64) -> bool {
        self.sampled > 0.0
            && self.sampled >= SAMPLED_SHARE * seconds
            && self.memory + self.saturated >= CLOSED_SHARE * self.sampled
    }

    fn pressure(&self) -> Pressure {
        if self.memory >= self.saturated {
            Pressure::MemoryPressure
        } else {
            Pressure::CpuSaturation
        }
    }
}

/// The part of `[from, to)` inside `window`, if any.
fn within(window: Window, from: Timestamp, to: Timestamp) -> Option<(Timestamp, Timestamp)> {
    let (from, to) = (from.max(window.since), to.min(window.until));
    (from < to).then_some((from, to))
}

/// The gate over `[from, to)`, each of `buckets` weighted by its overlap.
fn closed_over(buckets: &[Bucket], width: Duration, from: Timestamp, to: Timestamp) -> Closed {
    let width_ms = (width.as_millis() as u64).max(1);
    let mut closed = Closed::default();
    for bucket in buckets {
        let start = bucket.at.millis().max(from.millis());
        let end = (bucket.at.millis().saturating_add(width_ms)).min(to.millis());
        if end > start {
            closed.add(bucket, (end - start) as f64 / width_ms as f64);
        }
    }
    closed
}

/// The longest stretch of consecutive blocked fine buckets on one member.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Blocked {
    pub since: Timestamp,
    pub until: Timestamp,
    pub seconds: u64,
    pub cause: Pressure,
    pub memory_pressure_seconds: u64,
    pub saturated_seconds: u64,
    /// Mean CPU utilization outside Elsecall's grants over the stretch.
    pub outside_mean: Option<f64>,
    pub worst_memory_pressure: Option<MemoryPressure>,
    /// The outside processes the member named for the episode that overlaps the
    /// stretch the longest, leading first; empty if it named none.
    pub outside: Vec<OutsideProcess>,
}

fn stretch(run: &[&Bucket], window: Window, episodes: &[Episode]) -> Option<Blocked> {
    let width = window.fine_width();
    let (since, until) = within(window, run.first()?.at, run.last()?.at.after(width))?;
    let mut closed = Closed::default();
    for bucket in run {
        closed.add(bucket, 1.0);
    }
    let (outside, weight) = run
        .iter()
        .filter_map(|b| Some((b.outside_mean? * b.seconds as f64, b.seconds as f64)))
        .fold((0.0, 0.0), |(s, w), (v, n)| (s + v, w + n));
    Some(Blocked {
        since,
        until,
        seconds: until.since(since).as_secs(),
        cause: closed.pressure(),
        memory_pressure_seconds: run.iter().map(|b| b.memory_pressure_seconds).sum(),
        saturated_seconds: run.iter().map(|b| b.saturated_seconds).sum(),
        outside_mean: (weight > 0.0).then(|| outside / weight),
        worst_memory_pressure: run
            .iter()
            .fold(None, |w, b| worst(w, b.worst_memory_pressure, memory_rank)),
        outside: outside::behind(episodes, closed.pressure(), window, since, until),
    })
}

/// The longest stretch of `buckets`, a member's fine buckets over `window`, in
/// which outside pressure kept the gate closed, each bucket judged over its part
/// of the window. A bucket missing from the history ends a stretch. The member's
/// recorded `episodes` name the outside processes behind it.
pub fn longest(buckets: &[Bucket], window: Window, episodes: &[Episode]) -> Option<Blocked> {
    let width = window.fine_width();
    let blocked = |bucket: &Bucket| {
        within(window, bucket.at, bucket.at.after(width)).is_some_and(|(from, to)| {
            closed_over(std::slice::from_ref(bucket), width, from, to)
                .throughout(to.since(from).as_secs_f64())
        })
    };
    let mut ordered: Vec<&Bucket> = buckets.iter().collect();
    ordered.sort_by_key(|b| b.at);
    let mut longest: Option<Blocked> = None;
    let mut start = 0;
    for end in 1..=ordered.len() {
        let continues = end < ordered.len()
            && blocked(ordered[end])
            && blocked(ordered[end - 1])
            && ordered[end].at == ordered[end - 1].at.after(width);
        if continues {
            continue;
        }
        if blocked(ordered[start])
            && let Some(found) = stretch(&ordered[start..end], window, episodes)
            && longest.as_ref().is_none_or(|l| found.seconds > l.seconds)
        {
            longest = Some(found);
        }
        start = end;
    }
    longest
}

/// How a request's wait ended.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum QueueEnd {
    Started,
    /// It was still queued when the period ended.
    Queued,
    /// It ended without starting: the failure code, or `cancelled`.
    Failed {
        code: String,
    },
}

/// A label's requests that queued on one member for [`QUEUED_SECONDS`] or more
/// while outside pressure kept its gate closed, with the longest of those waits.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct BlockedQueue {
    pub label: Option<String>,
    pub requests: usize,
    pub longest_wait_seconds: u64,
    pub requested_at: Timestamp,
    pub ended: QueueEnd,
    pub cause: Pressure,
    /// The outside processes the member named over the longest wait, as for
    /// [`Blocked::outside`].
    pub outside: Vec<OutsideProcess>,
}

/// The member `fact` waited for admission on: placeable requests wait for any
/// member, so only those pinned there or required there and started.
fn queued_on(fact: &Fact) -> Option<&str> {
    match fact.placement {
        PlacementKind::Placeable => None,
        PlacementKind::Pinned => Some(fact.member.as_deref().unwrap_or(&fact.calling_member)),
        PlacementKind::Required => fact.member.as_deref(),
    }
}

/// When `fact`'s wait ended within `window`, and how: a wait that outlasted the
/// period is still queued at its end.
fn wait_end(fact: &Fact, window: Window) -> Option<(Timestamp, QueueEnd)> {
    let (end, ended) = if let Some(waited) = fact.waited_ms {
        (
            fact.requested_at.after(Duration::from_millis(waited)),
            QueueEnd::Started,
        )
    } else if fact.member.is_some() {
        return None;
    } else if fact.unfinished {
        (window.until, QueueEnd::Queued)
    } else {
        let code = fact.failure.clone()?;
        (
            fact.resources.as_ref()?.ended_at?,
            QueueEnd::Failed { code },
        )
    };
    if end > window.until {
        return Some((window.until, QueueEnd::Queued));
    }
    Some((end, ended))
}

/// The labels of `facts` that queued on `member` for [`QUEUED_SECONDS`] or more
/// within `window` while outside pressure kept its gate closed throughout, per
/// its fine `buckets`, each naming the outside processes of its member's
/// `episodes`; longest first. Elsecall's own internal work and nested
/// requests are left out.
pub fn queues(
    facts: &[Fact],
    member: &str,
    buckets: &[Bucket],
    window: Window,
    episodes: &[Episode],
) -> Vec<BlockedQueue> {
    let mut by_label: BTreeMap<Option<String>, BlockedQueue> = BTreeMap::new();
    for fact in facts {
        if fact.nested || fact.internal || queued_on(fact) != Some(member) {
            continue;
        }
        let Some((end, ended)) = wait_end(fact, window) else {
            continue;
        };
        let Some((from, to)) = within(window, fact.requested_at, end) else {
            continue;
        };
        let wait = to.since(from);
        if wait.as_secs() < QUEUED_SECONDS {
            continue;
        }
        let closed = closed_over(buckets, window.fine_width(), from, to);
        if !closed.throughout(wait.as_secs_f64()) {
            continue;
        }
        let found = BlockedQueue {
            label: fact.label.clone(),
            requests: 1,
            longest_wait_seconds: wait.as_secs(),
            requested_at: fact.requested_at,
            ended,
            cause: closed.pressure(),
            outside: outside::behind(episodes, closed.pressure(), window, from, to),
        };
        by_label
            .entry(fact.label.clone())
            .and_modify(|queue| {
                let requests = queue.requests + 1;
                if found.longest_wait_seconds > queue.longest_wait_seconds {
                    *queue = found.clone();
                }
                queue.requests = requests;
            })
            .or_insert(found);
    }
    let mut queues: Vec<BlockedQueue> = by_label.into_values().collect();
    queues.sort_by(|a, b| {
        b.longest_wait_seconds
            .cmp(&a.longest_wait_seconds)
            .then(a.label.cmp(&b.label))
    });
    queues
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reporting::fixtures::{self, MINUTE, T0, bucket};
    use crate::scheduling::record::{Outcome, Record};

    /// The 2 hours from `T0`, in minute buckets.
    fn window() -> Window {
        let until = Timestamp::from_millis(T0 + 120 * MINUTE);
        let window = Window::new(Timestamp::from_millis(T0), until, until).unwrap();
        assert_eq!(window.fine_width().as_secs(), 60);
        window
    }

    /// A minute whose gate memory pressure closed for `closed` of its 60 s,
    /// with Elsecall holding `in_use` cores.
    fn pressured(at_min: u64, closed: u64, in_use: f64) -> Bucket {
        let mut b = bucket(at_min, 0.15, in_use, 0, 0, 1.0);
        b.memory_pressure_seconds = closed;
        b
    }

    fn saturated(at_min: u64, in_use: f64) -> Bucket {
        bucket(at_min, 0.97, in_use, 0, 60, 1.0)
    }

    #[test]
    fn the_longest_stretch_counts_mostly_closed_minutes_while_elsecall_held_nothing() {
        let mut buckets: Vec<Bucket> = (0..3).map(|m| pressured(m, 60, 2.0)).collect();
        buckets.extend((3..13).map(|m| pressured(m, 54, 0.0)));
        buckets.push(pressured(13, 53, 0.0));
        buckets.extend((14..20).map(|m| pressured(m, 60, 0.0)));
        let found = longest(&buckets, window(), &[]).unwrap();
        // Minutes 0–2 Elsecall's own; 13 closed under 90% of the time.
        assert_eq!(found.since.millis(), T0 + 3 * MINUTE);
        assert_eq!(found.until.millis(), T0 + 13 * MINUTE);
        assert_eq!(found.seconds, 600);
        assert_eq!(found.cause, Pressure::MemoryPressure);
        assert_eq!(found.memory_pressure_seconds, 540);
        assert!((found.outside_mean.unwrap() - 0.15).abs() < 1e-9);
    }

    #[test]
    fn a_missing_or_barely_sampled_minute_ends_a_stretch() {
        let mut buckets: Vec<Bucket> = (0..5).map(|m| saturated(m, 0.0)).collect();
        buckets.extend((6..9).map(|m| saturated(m, 0.0)));
        let found = longest(&buckets, window(), &[]).unwrap();
        assert_eq!((found.seconds, found.cause), (300, Pressure::CpuSaturation));
        // A laptop asleep but for a few seconds a minute.
        let mut dozing: Vec<Bucket> = (0..30).map(|m| pressured(m, 20, 0.0)).collect();
        for b in &mut dozing {
            b.seconds = 20;
        }
        assert_eq!(longest(&dozing, window(), &[]), None);
        assert_eq!(
            longest(&[bucket(0, 0.1, 0.0, 0, 0, 1.0)], window(), &[]),
            None
        );
    }

    #[test]
    fn outside_cpu_saturation_counts_while_elsecall_holds_work_but_memory_pressure_does_not() {
        let busy_cpu: Vec<Bucket> = (0..10).map(|m| saturated(m, 4.0)).collect();
        assert_eq!(longest(&busy_cpu, window(), &[]).unwrap().seconds, 600);
        let busy_memory: Vec<Bucket> = (0..10).map(|m| pressured(m, 60, 4.0)).collect();
        assert_eq!(longest(&busy_memory, window(), &[]), None);
    }

    #[test]
    fn a_stretch_is_clamped_to_the_period() {
        // From half a minute in to half a minute before the last minute ends,
        // which is still filling: 30 s so far.
        let half = Window::new(
            Timestamp::from_millis(T0 + 30_000),
            Timestamp::from_millis(T0 + 119 * MINUTE + 30_000),
            Timestamp::from_millis(T0 + 120 * MINUTE),
        )
        .unwrap();
        let mut buckets: Vec<Bucket> = (0..120).map(|m| pressured(m, 60, 0.0)).collect();
        let last = buckets.last_mut().unwrap();
        (last.seconds, last.memory_pressure_seconds) = (30, 30);
        let found = longest(&buckets, half, &[]).unwrap();
        assert_eq!(found.since.millis(), T0 + 30_000);
        assert_eq!(found.until.millis(), T0 + 119 * MINUTE + 30_000);
        assert_eq!(found.seconds, 119 * 60);
    }

    /// A lease pinned to `devbox`, made `at` minutes into the period, that never
    /// started and was cancelled `waited` seconds later.
    fn cancelled(n: u64, label: &str, at: u64, waited: u64) -> Record {
        let mut record = fixtures::base(n, label, "devbox");
        record.requested_at = Timestamp::from_millis(T0 + at * MINUTE);
        record.ended_at = Some(record.requested_at.after(Duration::from_secs(waited)));
        record.outcome = Some(Outcome::Cancelled {
            reason: "The caller exited while waiting.".into(),
        });
        record
    }

    fn facts(records: &[Record]) -> Vec<Fact> {
        records.iter().filter_map(Fact::of).collect()
    }

    #[test]
    fn requests_queued_an_hour_or_more_on_a_closed_gate_are_grouped_by_label() {
        let closed: Vec<Bucket> = (0..120).map(|m| pressured(m, 60, 0.0)).collect();
        let mut started = fixtures::pinned(4, "warm", "devbox", 1.0);
        started.requested_at = Timestamp::from_millis(T0 + 5 * MINUTE);
        started.waited_ms = Some(3_700_000);
        let mut still = cancelled(5, "test", 30, 0);
        still.ended_at = None;
        still.outcome = None;
        let records = [
            cancelled(1, "warm", 0, 3600),
            cancelled(2, "warm", 10, 3599),
            started,
            still,
            // Elsewhere, placeable, internal or nested: none of this member's.
            {
                let mut r = cancelled(6, "other", 0, 7000);
                r.calling_member = "mini".into();
                r
            },
            {
                let mut r = cancelled(7, "other", 0, 7000);
                r.placement = crate::scheduling::request::Placement::Placeable;
                r
            },
            {
                let mut r = cancelled(8, "other", 0, 7000);
                r.internal = true;
                r
            },
        ];
        let queues = queues(&facts(&records), "devbox", &closed, window(), &[]);
        let found: Vec<_> = queues
            .iter()
            .map(|q| {
                (
                    q.label.as_deref(),
                    q.requests,
                    q.longest_wait_seconds,
                    q.ended.clone(),
                )
            })
            .collect();
        assert_eq!(
            found,
            [
                // From minute 30 to the period's end.
                (Some("test"), 1, 5400, QueueEnd::Queued),
                (Some("warm"), 2, 3700, QueueEnd::Started),
            ]
        );
        assert_eq!(
            serde_json::to_value(QueueEnd::Failed {
                code: "cancelled".into()
            })
            .unwrap(),
            serde_json::json!({"state": "failed", "code": "cancelled"})
        );
    }

    #[test]
    fn a_queue_on_a_gate_open_or_unsampled_for_much_of_the_wait_is_not_blocked() {
        let lease = facts(&[cancelled(1, "warm", 0, 6000)]);
        // Closed 90% of the time, the first ten minutes of each hundred open.
        let mut mostly: Vec<Bucket> = (0..100)
            .map(|m| pressured(m, if m < 10 { 0 } else { 60 }, 0.0))
            .collect();
        assert_eq!(queues(&lease, "devbox", &mostly, window(), &[]).len(), 1);
        // One more open half minute is too many.
        mostly[10].memory_pressure_seconds = 30;
        assert_eq!(queues(&lease, "devbox", &mostly, window(), &[]), []);
        // Closed while sampled, but asleep from minute 20.
        let dozing: Vec<Bucket> = (0..20).map(|m| pressured(m, 60, 0.0)).collect();
        assert_eq!(queues(&lease, "devbox", &dozing, window(), &[]), []);
        // Elsecall's own work held cores: its memory pressure, maybe.
        let own: Vec<Bucket> = (0..100).map(|m| pressured(m, 60, 2.0)).collect();
        assert_eq!(queues(&lease, "devbox", &own, window(), &[]), []);
    }
}
