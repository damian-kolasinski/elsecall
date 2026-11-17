//! When a member was overloaded: how far past its CPUs, how much of that load
//! Elsecall admitted, which other members had room meanwhile, and the Elsecall
//! work that ran there then, by how it came to run there. Enough to decide
//! whether to move work elsewhere, make labels placeable, or bring outside work
//! under admission.
use crate::{
    load_log::buckets::Bucket,
    reporting::{
        facts::{Fact, PlacementKind},
        load::{has_room, overloaded, peak_load_per_cpu, weighted_mean},
    },
    scheduling::{request::Kind, timestamp::Timestamp},
};
use serde::Serialize;
use std::{collections::BTreeMap, time::Duration};

/// At most this many labels, those with the most requests first.
pub const LABELS: usize = 5;

#[derive(Debug, Serialize, PartialEq)]
pub struct Overload {
    /// Seconds of fine buckets in which the member was overloaded.
    pub seconds: u64,
    /// The highest one-minute load average per logical CPU in those buckets.
    pub peak_load_per_cpu: Option<f64>,
    pub logical_cpus: Option<u32>,
    /// Threads running or waiting for a CPU, on average over those buckets…
    pub load_mean: f64,
    /// …the weighted cores Elsecall's grants held meanwhile…
    pub elsecall_cores_mean: f64,
    /// …and the share of the load beyond them: work Elsecall did not admit, or
    /// admitted work using more than its size.
    pub outside_share: f64,
    /// Other members that had room during those buckets, and for how long.
    pub while_room: Vec<Room>,
    /// The Elsecall requests that ran here during those buckets.
    pub work: OverloadWork,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct Room {
    pub member: String,
    pub seconds: u64,
}

/// How a request came to run on the member.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum How {
    /// A run pinned to its calling member.
    Pinned,
    /// A lease, always held on its calling member.
    Lease,
    /// A placeable run that placement put here.
    Placeable,
    /// A run that required this member.
    Required,
}

#[derive(Debug, Default, Serialize, PartialEq)]
pub struct OverloadWork {
    pub pinned: usize,
    pub leases: usize,
    pub placeable: usize,
    pub required: usize,
    /// The labels with the most of those requests, at most [`LABELS`].
    pub labels: Vec<LabelWork>,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct LabelWork {
    pub label: Option<String>,
    pub how: How,
    pub requests: usize,
}

fn how(fact: &Fact) -> How {
    match (fact.kind, fact.placement) {
        (Kind::Lease, _) => How::Lease,
        (Kind::Run, PlacementKind::Pinned) => How::Pinned,
        (Kind::Run, PlacementKind::Placeable) => How::Placeable,
        (Kind::Run, PlacementKind::Required) => How::Required,
    }
}

/// When `fact` held its member, from its start to before its end, or to
/// `until` while it lasts; `None` if it never started. A run too short to
/// measure holds it for a millisecond.
fn ran(fact: &Fact, until: Timestamp) -> Option<(Timestamp, Timestamp)> {
    let start = fact
        .requested_at
        .after(Duration::from_millis(fact.waited_ms?));
    let end = if fact.unfinished {
        until
    } else {
        start.after(Duration::from_millis(
            fact.sync_ms.unwrap_or(0) + fact.run_ms.unwrap_or(0),
        ))
    };
    Some((start, end.max(start.after(Duration::from_millis(1)))))
}

/// `member`'s overload among `loads`, every member's fine buckets of `width`,
/// with the `facts` that ran there; `None` when it was never overloaded.
pub fn overload(
    member: &str,
    loads: &[(&str, &[Bucket])],
    facts: &[&Fact],
    width: Duration,
    until: Timestamp,
) -> Option<Overload> {
    let (_, own) = loads.iter().find(|(name, _)| *name == member)?;
    let hot: Vec<&Bucket> = own.iter().filter(|b| overloaded(b)).collect();
    let seconds: u64 = hot.iter().map(|b| b.seconds).sum();
    if seconds == 0 {
        return None;
    }
    let load_mean = weighted_mean(hot.iter().copied(), |b| b.load_mean).unwrap_or(0.0);
    let elsecall_cores_mean =
        weighted_mean(hot.iter().copied(), |b| Some(b.in_use_mean)).unwrap_or(0.0);
    let while_room = loads
        .iter()
        .filter(|(name, _)| *name != member)
        .filter_map(|(name, buckets)| {
            let seconds: u64 = hot
                .iter()
                .filter(|h| buckets.iter().any(|b| b.at == h.at && has_room(b)))
                .map(|h| h.seconds)
                .sum();
            (seconds > 0).then(|| Room {
                member: (*name).to_owned(),
                seconds,
            })
        })
        .collect();
    let during = |fact: &&Fact| {
        ran(fact, until)
            .is_some_and(|(start, end)| hot.iter().any(|b| start < b.at.after(width) && end > b.at))
    };
    let mut work = OverloadWork::default();
    let mut labels: BTreeMap<(Option<String>, How), usize> = BTreeMap::new();
    for fact in facts
        .iter()
        .copied()
        .filter(|f| !f.nested && !f.internal && f.member.as_deref() == Some(member))
        .filter(during)
    {
        let how = how(fact);
        *match how {
            How::Pinned => &mut work.pinned,
            How::Lease => &mut work.leases,
            How::Placeable => &mut work.placeable,
            How::Required => &mut work.required,
        } += 1;
        *labels.entry((fact.label.clone(), how)).or_default() += 1;
    }
    let mut labels: Vec<LabelWork> = labels
        .into_iter()
        .map(|((label, how), requests)| LabelWork {
            label,
            how,
            requests,
        })
        .collect();
    labels.sort_by(|a, b| {
        b.requests
            .cmp(&a.requests)
            .then(a.label.cmp(&b.label))
            .then(a.how.cmp(&b.how))
    });
    labels.truncate(LABELS);
    work.labels = labels;
    Some(Overload {
        seconds,
        peak_load_per_cpu: hot
            .iter()
            .filter_map(|b| peak_load_per_cpu(b))
            .reduce(f64::max),
        logical_cpus: hot.iter().rev().find_map(|b| b.logical_cpus),
        load_mean,
        elsecall_cores_mean,
        outside_share: if load_mean > 0.0 {
            ((load_mean - elsecall_cores_mean) / load_mean).clamp(0.0, 1.0)
        } else {
            0.0
        },
        while_room,
        work,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        reporting::fixtures::{self, MINUTE, T0, bucket},
        scheduling::{record::Record, request::Placement},
    };

    /// A run on `member` requested at `at` ms that held it for `run` seconds.
    fn ran(n: u64, label: &str, member: &str, at: u64, run: f64) -> Record {
        let mut record = fixtures::pinned(n, label, member, run);
        record.requested_at = Timestamp::from_millis(at);
        record.ended_at = Some(record.requested_at.after(Duration::from_secs_f64(run)));
        record
    }

    /// A fine bucket of a 14-CPU member at `cpu`, `load` threads and `in_use`
    /// weighted cores of grants.
    fn loaded(at_min: u64, cpu: f64, load: f64, in_use: f64) -> Bucket {
        Bucket {
            load_mean: Some(load),
            load_max: Some(load * 1.5),
            logical_cpus: Some(14),
            ..bucket(at_min, cpu, in_use, 0, 0, 1.0)
        }
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn overload_needs_full_cpus_and_a_queue_for_them() {
        assert!(overloaded(&loaded(0, 1.0, 28.0, 0.0)), "two threads a CPU");
        assert!(
            !overloaded(&loaded(0, 1.0, 27.9, 0.0)),
            "a full machine with a short queue is busy, not overloaded"
        );
        assert!(
            !overloaded(&loaded(0, 0.5, 60.0, 0.0)),
            "threads waiting on I/O, CPUs half idle"
        );
        assert!(
            !overloaded(&bucket(0, 1.0, 9.0, 0, 0, 1.0)),
            "a member that kept no load average"
        );
        let mut unknown = loaded(0, 1.0, 60.0, 0.0);
        unknown.logical_cpus = None;
        assert!(!overloaded(&unknown));
        assert!(has_room(&loaded(0, 0.25, 13.0, 0.0)));
        let mut pressured = loaded(0, 0.25, 13.0, 0.0);
        pressured.memory_pressure_seconds = 10;
        assert!(!has_room(&pressured));
    }

    #[test]
    fn an_overloaded_member_names_the_room_elsewhere_and_the_work_it_ran() {
        // The mini runs 9 cores of grants under ~200 threads for two minutes,
        // then calms down; the macbook has room in the first minute only.
        let mini = vec![
            loaded(0, 1.0, 210.0, 9.0),
            loaded(1, 1.0, 190.0, 9.0),
            loaded(2, 0.4, 10.0, 2.0),
        ];
        let macbook = vec![
            loaded(0, 0.2, 13.0, 0.0),
            loaded(1, 0.95, 20.0, 8.0),
            loaded(2, 0.1, 5.0, 0.0),
        ];
        let at = |min: u64, s: u64| T0 + min * MINUTE + s * 1000;
        // Ran during the overload: a pinned check, a lease still held, two
        // placeable tests placed here, and one required run.
        let pinned = ran(1, "check", "mini", at(0, 10), 30.0);
        let mut lease = ran(2, "warm", "mini", at(0, 5), 0.0);
        lease.kind = Kind::Lease;
        lease.ended_at = None;
        lease.run_ms = None;
        let placeable = |n| {
            let mut run = ran(n, "test", "mini", at(1, 0), 20.0);
            run.placement = Placement::Placeable;
            run
        };
        let mut required = ran(5, "build", "mini", at(1, 30), 10.0);
        required.placement = Placement::Member("mini".into());
        // Not during it: before, after, elsewhere, nested, internal, never started.
        let before = ran(11, "check", "mini", T0 - MINUTE, 60.0);
        let after = ran(6, "check", "mini", at(2, 0), 5.0);
        let elsewhere = ran(7, "check", "macbook", at(0, 10), 30.0);
        let mut nested = ran(8, "check", "mini", at(0, 10), 30.0);
        nested.nested_in = Some(pinned.run_id.clone());
        let mut internal = ran(9, "elsecall housekeeping", "mini", at(0, 10), 1.0);
        internal.internal = true;
        let mut queued = fixtures::base(10, "check", "mini");
        queued.requested_at = Timestamp::from_millis(at(0, 30));
        queued.ended_at = None;
        let facts: Vec<Fact> = [
            pinned,
            lease,
            placeable(3),
            placeable(4),
            required,
            before,
            after,
            elsewhere,
            nested,
            internal,
            queued,
        ]
        .iter()
        .filter_map(Fact::of)
        .collect();
        let facts: Vec<&Fact> = facts.iter().collect();
        let loads: [(&str, &[Bucket]); 2] = [("macbook", &macbook), ("mini", &mini)];
        let until = Timestamp::from_millis(at(5, 0));
        let overload = overload("mini", &loads, &facts, Duration::from_secs(60), until).unwrap();
        assert_eq!(overload.seconds, 120);
        assert_eq!(overload.logical_cpus, Some(14));
        assert!(close(
            overload.peak_load_per_cpu.unwrap(),
            210.0 * 1.5 / 14.0
        ));
        assert!(close(overload.load_mean, 200.0));
        assert!(close(overload.elsecall_cores_mean, 9.0));
        assert!(close(overload.outside_share, 191.0 / 200.0));
        assert_eq!(
            overload.while_room,
            [Room {
                member: "macbook".into(),
                seconds: 60
            }]
        );
        let work = &overload.work;
        assert_eq!(
            (work.pinned, work.leases, work.placeable, work.required),
            (1, 1, 2, 1),
            "{work:?}"
        );
        assert_eq!(
            work.labels[0],
            LabelWork {
                label: Some("test".into()),
                how: How::Placeable,
                requests: 2
            }
        );
        assert_eq!(work.labels.len(), 4);
        assert_eq!(
            super::overload("macbook", &loads, &facts, Duration::from_secs(60), until),
            None,
            "busy, never overloaded"
        );
    }
}
