//! Metrics of the requests themselves: how they ended, `fleet_busy`, where they
//! ran, how long they queued, and how much work each member took.
use crate::{
    reporting::{
        facts::{Fact, PlacementKind},
        statistics::{Distribution, seconds},
    },
    scheduling::{record::Verdict, request::Kind},
};
use serde::Serialize;
use std::collections::BTreeMap;

/// Requests by kind and by how they ended.
#[derive(Debug, Default, Serialize, PartialEq)]
pub struct Requests {
    pub total: usize,
    pub runs: usize,
    pub leases: usize,
    /// Ran inside another grant; counted nowhere else.
    pub nested: usize,
    pub started: usize,
    pub unfinished: usize,
    /// Requests Elsecall failed, by failure code, and `cancelled` ones; past
    /// [`MAXIMUM_CODES`] codes, the least frequent are summed as `other`.
    pub failed: BTreeMap<String, usize>,
}

/// The most failure codes listed apart.
pub const MAXIMUM_CODES: usize = 20;

/// `counts` with at most [`MAXIMUM_CODES`] keys, the rest summed as `other`.
fn capped(counts: BTreeMap<String, usize>) -> BTreeMap<String, usize> {
    if counts.len() <= MAXIMUM_CODES {
        return counts;
    }
    let mut ranked: Vec<(String, usize)> = counts.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let rest: usize = ranked
        .split_off(MAXIMUM_CODES - 1)
        .iter()
        .map(|(_, n)| n)
        .sum();
    let mut kept: BTreeMap<String, usize> = ranked.into_iter().collect();
    *kept.entry("other".into()).or_default() += rest;
    kept
}

pub fn requests(facts: &[Fact]) -> Requests {
    let mut requests = Requests::default();
    for fact in facts {
        requests.total += 1;
        match fact.kind {
            Kind::Run => requests.runs += 1,
            Kind::Lease => requests.leases += 1,
        }
        requests.nested += usize::from(fact.nested);
        requests.started += usize::from(fact.member.is_some());
        requests.unfinished += usize::from(fact.unfinished);
        if let Some(code) = &fact.failure {
            *requests.failed.entry(code.clone()).or_default() += 1;
        }
    }
    requests.failed = capped(requests.failed);
    requests
}

/// The facts admission decided on its own: everything but nested requests.
fn admitted(facts: &[Fact]) -> impl Iterator<Item = &Fact> {
    facts.iter().filter(|f| !f.nested)
}

#[derive(Debug, Serialize, PartialEq)]
pub struct FleetBusy {
    pub count: usize,
    /// Interactive requests that were decided.
    pub of: usize,
    pub rate: Option<f64>,
}

pub fn fleet_busy(facts: &[Fact]) -> FleetBusy {
    let decided: Vec<&Fact> = admitted(facts)
        .filter(|f| !f.deferrable && f.verdict.is_some())
        .collect();
    let count = decided
        .iter()
        .filter(|f| f.verdict == Some(Verdict::FleetBusy))
        .count();
    FleetBusy {
        count,
        of: decided.len(),
        rate: (!decided.is_empty()).then(|| count as f64 / decided.len() as f64),
    }
}

#[derive(Debug, Default, Serialize, PartialEq)]
pub struct PlacementMix {
    pub pinned: usize,
    pub required: usize,
    pub placeable: Placeable,
}

/// Placeable requests that started.
#[derive(Debug, Default, Serialize, PartialEq)]
pub struct Placeable {
    pub started: usize,
    pub on_calling_member: usize,
    pub elsewhere: usize,
    pub by_member: BTreeMap<String, usize>,
    /// Those whose run estimate where they ran was the prior, not history.
    pub run_prior: usize,
}

pub fn placement(facts: &[Fact]) -> PlacementMix {
    let mut mix = PlacementMix::default();
    for fact in admitted(facts) {
        match fact.placement {
            PlacementKind::Pinned => mix.pinned += 1,
            PlacementKind::Required => mix.required += 1,
            PlacementKind::Placeable => {
                let Some(member) = &fact.member else { continue };
                let placeable = &mut mix.placeable;
                placeable.started += 1;
                if *member == fact.calling_member {
                    placeable.on_calling_member += 1;
                } else {
                    placeable.elsewhere += 1;
                }
                *placeable.by_member.entry(member.clone()).or_default() += 1;
                placeable.run_prior +=
                    usize::from(fact.predicted.as_ref().is_some_and(|p| p.run_prior));
            }
        }
    }
    mix
}

/// Time from request to start, of requests that started. Deferrable work waits
/// for idle capacity by design, so it is kept apart.
#[derive(Debug, Default, Serialize, PartialEq)]
pub struct Waits {
    pub interactive: Distribution,
    pub deferrable: Distribution,
}

/// Waits of the requests that started on `member`, or anywhere.
pub fn waits(facts: &[Fact], member: Option<&str>) -> Waits {
    let of = |deferrable: bool| {
        Distribution::of(
            admitted(facts)
                .filter(|f| f.deferrable == deferrable)
                .filter(|f| member.is_none_or(|m| f.member.as_deref() == Some(m)))
                .filter_map(|f| f.waited_ms.map(seconds))
                .collect(),
        )
    };
    Waits {
        interactive: of(false),
        deferrable: of(true),
    }
}

/// Elsecall work that ran on a member.
#[derive(Debug, Default, Serialize, PartialEq)]
pub struct Work {
    pub runs: usize,
    pub leases: usize,
    /// Size × run time of its completed work.
    pub core_seconds: f64,
    /// Its share of every member's core-seconds.
    pub share: Option<f64>,
}

/// The work that ran on each of `members`, with shares of their total.
pub fn work(facts: &[Fact], members: &[&str]) -> BTreeMap<String, Work> {
    let mut work: BTreeMap<String, Work> = members
        .iter()
        .map(|m| ((*m).to_owned(), Work::default()))
        .collect();
    for fact in admitted(facts) {
        let Some(entry) = fact.member.as_ref().and_then(|m| work.get_mut(m)) else {
            continue;
        };
        match fact.kind {
            Kind::Run => entry.runs += 1,
            Kind::Lease => entry.leases += 1,
        }
        if fact.completed
            && let Some(run) = fact.run_ms
        {
            entry.core_seconds += fact.size.as_f64() * seconds(run);
        }
    }
    let total: f64 = work.values().map(|w| w.core_seconds).sum();
    for entry in work.values_mut() {
        entry.share = (total > 0.0).then(|| entry.core_seconds / total);
    }
    work
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        reporting::fixtures::{self, Run},
        scheduling::{
            placement::Source,
            record::{Outcome, Verdict},
            request::{AtDeadline, Placement, Urgency},
            run_id::RunId,
            timestamp::Timestamp,
        },
    };

    fn facts() -> Vec<Fact> {
        let mut records = vec![
            fixtures::placed(Run {
                n: 1,
                member: "devbox",
                waited: 4.0,
                actual_run: 5.0,
                ..Run::default()
            }),
            fixtures::placed(Run {
                n: 2,
                member: "mini",
                waited: 1.0,
                run_from: Source::Prior,
                ..Run::default()
            }),
            fixtures::pinned(3, "test", "devbox", 2.0),
        ];
        let mut busy = fixtures::base(4, "test", "mini");
        busy.placement = Placement::Placeable;
        busy.outcome = Some(Outcome::Failed {
            code: "fleet_busy".into(),
        });
        busy.decision = Some(fixtures::decision(Verdict::FleetBusy, Vec::new(), None));
        records.push(busy);
        let mut lease = fixtures::pinned(5, "hold", "mini", 10.0);
        lease.kind = Kind::Lease;
        lease.outcome = Some(Outcome::Released);
        records.push(lease);
        let mut nested = fixtures::pinned(6, "inner", "mini", 100.0);
        nested.nested_in = Some(RunId::parse("r-000000000005").unwrap());
        nested.decision.as_mut().unwrap().verdict = Verdict::Nested;
        records.push(nested);
        let mut deferred = fixtures::pinned(7, "elsecall housekeeping", "mini", 1.0);
        deferred.urgency = Urgency::Deferrable {
            deadline: Timestamp::from_millis(0),
            at_deadline: AtDeadline::Skip,
        };
        deferred.waited_ms = Some(600_000);
        records.push(deferred);
        let mut required = fixtures::pinned(8, "test", "devbox", 1.0);
        required.placement = Placement::Member("devbox".into());
        required.calling_member = "mini".into();
        records.push(required);
        let mut queued = fixtures::base(9, "test", "mini");
        queued.ended_at = None;
        queued.outcome = None;
        records.push(queued);
        records.iter().filter_map(Fact::of).collect()
    }

    #[test]
    fn requests_are_counted_by_kind_and_end() {
        let requests = requests(&facts());
        assert_eq!(
            requests,
            Requests {
                total: 9,
                runs: 8,
                leases: 1,
                nested: 1,
                started: 7,
                unfinished: 1,
                failed: BTreeMap::from([("fleet_busy".into(), 1)]),
            }
        );
    }

    #[test]
    fn many_failure_codes_are_capped() {
        let mut facts = facts();
        let template = facts[3].clone();
        // fleet_busy twice, 30 other codes once each.
        facts.push(template.clone());
        for n in 0..30 {
            let mut fact = template.clone();
            fact.failure = Some(format!("code_{}", "x".repeat(n)));
            facts.push(fact);
        }
        let failed = requests(&facts).failed;
        assert_eq!(failed.len(), MAXIMUM_CODES);
        assert_eq!(failed["fleet_busy"], 2, "the most frequent stay");
        assert_eq!(failed.values().sum::<usize>(), 32);
        assert_eq!(failed["other"], 12);
    }

    #[test]
    fn fleet_busy_is_out_of_decided_interactive_requests() {
        // Decided, interactive, not nested: 1, 2, 3, 4 (busy), 5, 8.
        assert_eq!(
            fleet_busy(&facts()),
            FleetBusy {
                count: 1,
                of: 6,
                rate: Some(1.0 / 6.0),
            }
        );
        assert_eq!(fleet_busy(&[]).rate, None);
    }

    #[test]
    fn the_placement_mix_counts_where_placeable_work_started() {
        let mix = placement(&facts());
        assert_eq!(mix.pinned, 4, "3, 5, 7 and 9");
        assert_eq!(mix.required, 1);
        assert_eq!(
            mix.placeable,
            Placeable {
                started: 2,
                on_calling_member: 1,
                elsewhere: 1,
                by_member: BTreeMap::from([("devbox".into(), 1), ("mini".into(), 1)]),
                run_prior: 1,
            }
        );
    }

    #[test]
    fn waits_keep_deferrable_work_apart() {
        let all = waits(&facts(), None);
        // Interactive starts: 4 s, 1 s, 0 (pinned), 0 (lease), 0 (required).
        assert_eq!(all.interactive.count, 5);
        assert_eq!(all.interactive.p50_seconds, Some(0.0));
        assert_eq!(all.interactive.p90_seconds, Some(4.0));
        assert_eq!(all.interactive.max_seconds, Some(4.0));
        assert_eq!(all.deferrable.count, 1);
        assert_eq!(all.deferrable.max_seconds, Some(600.0));
        let devbox = waits(&facts(), Some("devbox"));
        assert_eq!(devbox.interactive.count, 3);
        assert_eq!(devbox.deferrable.count, 0);
    }

    #[test]
    fn work_is_size_times_run_time_of_completed_work() {
        let work = work(&facts(), &["devbox", "mini"]);
        // Size 2: devbox ran 5 + 2 + 1 s, mini 10 + 10 (lease) + 1 s.
        assert_eq!(work["devbox"].runs, 3);
        assert_eq!(work["devbox"].core_seconds, 16.0);
        assert_eq!(work["mini"].runs, 2);
        assert_eq!(work["mini"].leases, 1);
        assert_eq!(work["mini"].core_seconds, 42.0);
        assert_eq!(work["devbox"].share, Some(16.0 / 58.0));
    }
}
