//! The report as one document: the period, the filters, fleet and member
//! metrics, estimate accuracy and findings, from what was gathered.
use crate::{
    failure::Failure,
    label_pattern,
    load_log::buckets::Bucket,
    reporting::{
        accuracy::{self, Accuracy},
        blocked::{self, Blocked, BlockedQueue},
        facts::Fact,
        findings::{self, Finding},
        leftovers::{self, LeftoverFact, LeftoverMetrics},
        load::{self, Balance, LoadTotals, SeriesPoint, SpeedDrift},
        requests::{self, FleetBusy, PlacementMix, Requests, Waits, Work},
        run_phases::{self, PhaseMetrics},
        window::Window,
    },
    scheduling::{parameters::Parameters, timestamp::Timestamp},
};
use serde::Serialize;
use std::{collections::BTreeMap, time::Duration};

/// The version of the metric definitions in README "Fleet reports". A change
/// to any definition changes it, so agents tracking a metric can tell.
pub const DEFINITIONS: u32 = 7;

/// What a report covers.
#[derive(Clone, Debug)]
pub struct Query {
    pub window: Window,
    /// The width of each series point; a multiple of the window's fine width.
    pub series_width: Duration,
    /// Only requests this member made or ran, and only its load.
    pub member: Option<String>,
    /// Only requests whose label matches this glob.
    pub label: Option<String>,
    /// The reporting member's parameters, which the backtest and learned speed
    /// ratios use.
    pub parameters: Parameters,
}

/// What one member's history answered.
#[derive(Clone, Debug)]
pub struct MemberInput {
    pub member: String,
    /// Fine buckets of its load.
    pub load: Result<Vec<Bucket>, Failure>,
    pub records: Result<RecordsRead, Failure>,
    /// The blocked episodes it recorded, which name outside processes, and its
    /// early warnings about outside processes.
    pub outside: Result<crate::reporting::outside::OutsideHistory, Failure>,
}

/// How many of a member's requests were read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
pub struct RecordsRead {
    pub requests: usize,
    /// More were made in the period than one report reads.
    pub truncated: bool,
    /// Requests another member sent that named no member of the fleet.
    #[serde(skip_serializing_if = "is_zero")]
    pub dropped: usize,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

/// Everything gathered from the fleet for one report.
#[derive(Clone, Debug)]
pub struct Gathered {
    pub this_member: String,
    pub master: Option<String>,
    /// Why the fleet's members could not be listed, when this member is enrolled.
    pub fleet_error: Option<Failure>,
    /// Every member asked, in name order.
    pub members: Vec<MemberInput>,
    /// Every member's requests, each counted once.
    pub facts: Vec<Fact>,
    /// What placed commands left running, each run's once, from the members
    /// that hosted them.
    pub leftovers: Vec<LeftoverFact>,
}

#[derive(Debug, Serialize)]
pub struct Document {
    pub definitions: u32,
    pub period: Period,
    pub filters: Filters,
    #[serde(flatten)]
    pub metrics: Metrics,
    pub findings: Vec<Finding>,
}

#[derive(Debug, Serialize)]
pub struct Period {
    pub since: Timestamp,
    pub until: Timestamp,
    /// The width members' load is compared in.
    pub fine_seconds: u64,
    pub series_seconds: u64,
}

#[derive(Debug, Serialize)]
pub struct Filters {
    pub member: Option<String>,
    pub label: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Metrics {
    pub fleet: FleetMetrics,
    pub members: Vec<MemberMetrics>,
    pub accuracy: Accuracy,
    pub leftovers: LeftoverMetrics,
    pub demand: Vec<crate::workload::demand::Demand>,
    pub demand_omitted: usize,
}

#[derive(Debug, Serialize)]
pub struct FleetMetrics {
    pub this_member: String,
    pub master: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fleet_error: Option<Failure>,
    pub requests: Requests,
    pub fleet_busy: FleetBusy,
    pub placement: PlacementMix,
    pub waits: Waits,
    pub run_phases: PhaseMetrics,
    /// Always over every member, whatever the filters.
    pub balance: Balance,
}

#[derive(Debug, Serialize)]
pub struct MemberMetrics {
    pub member: String,
    pub master: bool,
    pub this_member: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub records: Option<RecordsRead>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub records_error: Option<Failure>,
    pub load: Option<LoadTotals>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub load_error: Option<Failure>,
    pub speed: Option<SpeedDrift>,
    /// The longest stretch in which outside pressure kept the gate closed while
    /// Elsecall held no work there.
    pub blocked: Option<Blocked>,
    /// Labels whose requests queued here for long behind a gate that stayed
    /// closed, of the requests the label filter keeps.
    pub blocked_queues: Vec<BlockedQueue>,
    /// Why the member could not say which outside processes held its gate
    /// closed, so `blocked` and `blocked_queues` name none and
    /// `outside_warnings` is empty.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outside_error: Option<Failure>,
    /// Early warnings about outside processes that held in the period, the
    /// longest-held first.
    pub outside_warnings: Vec<crate::reporting::outside_warnings::PeriodWarning>,
    pub work: Work,
    pub waits: Waits,
    pub run_phases: PhaseMetrics,
    pub series: Vec<SeriesPoint>,
}

impl Query {
    fn keeps_label(&self, fact: &Fact) -> bool {
        self.keeps(fact.label.as_deref())
    }

    fn keeps(&self, label: Option<&str>) -> bool {
        self.label
            .as_deref()
            .is_none_or(|pattern| label.is_some_and(|label| label_pattern::matches(pattern, label)))
    }

    fn keeps_leftovers(&self, fact: &LeftoverFact) -> bool {
        self.window.contains(fact.requested_at)
            && self.keeps(fact.label.as_deref())
            && self
                .member
                .as_deref()
                .is_none_or(|member| fact.member == member || fact.calling_member == member)
    }

    fn keeps_member(&self, fact: &Fact) -> bool {
        self.member.as_deref().is_none_or(|member| {
            fact.calling_member == member || fact.member.as_deref() == Some(member)
        })
    }
}

/// The report of `gathered` that `query` asks for.
pub fn document(query: &Query, gathered: &Gathered) -> Document {
    let demand_context = &gathered.facts;
    let mut gathered = gathered.clone();
    // Machine load remains actual fleet load. Request metrics and learned
    // backtests exclude synthetic labels, including when --label is supplied.
    gathered
        .facts
        .retain(|f| !f.synthetic && !crate::rehearsal::synthetic(f.label.as_deref()));
    gathered
        .leftovers
        .retain(|f| !crate::rehearsal::synthetic(f.label.as_deref()));
    // Requests of the period and label; a member's work and waits are the
    // requests that ran there, which the member filter always keeps.
    let labelled: Vec<Fact> = gathered
        .facts
        .iter()
        .filter(|f| query.window.contains(f.requested_at) && query.keeps_label(f))
        .cloned()
        .collect();
    let kept: Vec<Fact> = labelled
        .iter()
        .filter(|f| query.keeps_member(f))
        .cloned()
        .collect();
    let empty = Vec::new();
    let loads: Vec<(&str, &[Bucket])> = gathered
        .members
        .iter()
        .map(|m| {
            (
                m.member.as_str(),
                m.load.as_ref().unwrap_or(&empty).as_slice(),
            )
        })
        .collect();
    let speeds: BTreeMap<String, f64> = loads
        .iter()
        .filter_map(|(member, buckets)| Some(((*member).to_owned(), load::mean_speed(buckets)?)))
        .collect();
    let names: Vec<&str> = loads.iter().map(|(member, _)| *member).collect();
    let mut work = requests::work(&labelled, &names);
    let members = gathered
        .members
        .iter()
        .filter(|m| query.member.as_deref().is_none_or(|f| f == m.member))
        .map(|m| {
            let buckets = m.load.as_deref().unwrap_or_default();
            let episodes = m
                .outside
                .as_ref()
                .map(|history| history.episodes.as_slice())
                .unwrap_or_default();
            MemberMetrics {
                member: m.member.clone(),
                master: gathered.master.as_deref() == Some(m.member.as_str()),
                this_member: gathered.this_member == m.member,
                records: m.records.as_ref().ok().copied(),
                records_error: m.records.as_ref().err().cloned(),
                load: load::totals(buckets),
                load_error: m.load.as_ref().err().cloned(),
                speed: load::speed_drift(buckets),
                blocked: blocked::longest(buckets, query.window, episodes),
                blocked_queues: blocked::queues(
                    &labelled,
                    &m.member,
                    buckets,
                    query.window,
                    episodes,
                ),
                outside_error: m.outside.as_ref().err().cloned(),
                outside_warnings: m
                    .outside
                    .as_ref()
                    .map(|history| {
                        crate::reporting::outside_warnings::in_period(history, query.window)
                    })
                    .unwrap_or_default(),
                work: work.remove(&m.member).unwrap_or_default(),
                waits: requests::waits(&labelled, Some(&m.member)),
                run_phases: run_phases::metrics(&labelled, Some(&m.member)),
                series: load::series(buckets, query.series_width),
            }
        })
        .collect();
    let mut demand = crate::workload::demand::estimate_at_width(
        demand_context,
        &loads,
        query.window.fine_width().as_millis() as u64,
    );
    let kept_families: std::collections::BTreeSet<_> = kept
        .iter()
        .filter_map(|f| Some((f.member.as_deref()?, crate::workload::demand::family(f)?)))
        .collect();
    demand.retain(|d| {
        query.member.as_deref().is_none_or(|m| m == d.member)
            && kept_families.contains(&(d.member.as_str(), d.family.as_str()))
    });
    demand.sort_by(|a, b| {
        b.runs
            .cmp(&a.runs)
            .then(a.member.cmp(&b.member))
            .then(a.family.cmp(&b.family))
    });
    let demand_omitted = demand.len().saturating_sub(200);
    demand.truncate(200);
    let metrics = Metrics {
        demand,
        demand_omitted,
        fleet: FleetMetrics {
            this_member: gathered.this_member.clone(),
            master: gathered.master.clone(),
            fleet_error: gathered.fleet_error.clone(),
            requests: requests::requests(&kept),
            fleet_busy: requests::fleet_busy(&kept),
            placement: requests::placement(&kept),
            waits: requests::waits(&kept, None),
            run_phases: run_phases::metrics(&kept, None),
            balance: load::balance(&loads),
        },
        members,
        accuracy: accuracy::accuracy(
            &kept,
            &accuracy::Context {
                history: &gathered.facts,
                master: gathered.master.as_deref(),
                speeds: &speeds,
                parameters: &query.parameters,
            },
        ),
        leftovers: leftovers::leftovers(
            &gathered
                .leftovers
                .iter()
                .filter(|f| query.keeps_leftovers(f))
                .cloned()
                .collect::<Vec<_>>(),
        ),
    };
    let findings = findings::findings(&metrics, &gathered);
    Document {
        definitions: DEFINITIONS,
        period: Period {
            since: query.window.since,
            until: query.window.until,
            fine_seconds: query.window.fine_width().as_secs(),
            series_seconds: query.series_width.as_secs(),
        },
        filters: Filters {
            member: query.member.clone(),
            label: query.label.clone(),
        },
        metrics,
        findings,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reporting::fixtures::{self, MINUTE, Run, T0, bucket};

    fn gathered() -> Gathered {
        let mut records = vec![
            fixtures::placed(Run {
                n: 1,
                member: "devbox",
                ..Run::default()
            }),
            fixtures::pinned(2, "test", "mini", 1.0),
            fixtures::pinned(3, "build", "devbox", 1.0),
        ];
        // Before the period.
        let mut old = fixtures::pinned(0, "build", "mini", 1.0);
        old.requested_at = Timestamp::from_millis(T0 - MINUTE);
        records.push(old);
        Gathered {
            this_member: "mini".into(),
            master: Some("mini".into()),
            fleet_error: None,
            members: vec![
                MemberInput {
                    member: "devbox".into(),
                    outside: Ok(Default::default()),
                    load: Ok(vec![bucket(0, 0.1, 0.0, 0, 0, 0.7)]),
                    records: Err(Failure::new("member_unavailable", "gone", "check")),
                },
                MemberInput {
                    member: "mini".into(),
                    outside: Ok(Default::default()),
                    load: Ok(vec![bucket(0, 0.9, 2.0, 1, 0, 1.0)]),
                    records: Ok(RecordsRead {
                        requests: 3,
                        truncated: false,
                        dropped: 0,
                    }),
                },
            ],
            facts: records.iter().filter_map(Fact::of).collect(),
            leftovers: Vec::new(),
        }
    }

    fn query(member: Option<&str>, label: Option<&str>) -> Query {
        let window = Window::new(
            Timestamp::from_millis(T0),
            Timestamp::from_millis(T0 + 60 * MINUTE),
            Timestamp::from_millis(T0 + 60 * MINUTE),
        )
        .unwrap();
        Query {
            window,
            series_width: window.series_width(None),
            member: member.map(str::to_owned),
            label: label.map(str::to_owned),
            parameters: Default::default(),
        }
    }

    #[test]
    fn the_period_and_filters_select_requests_but_balance_stays_fleet_wide() {
        let all = document(&query(None, None), &gathered());
        assert_eq!(all.definitions, DEFINITIONS);
        assert_eq!(all.metrics.fleet.requests.total, 3, "not the one before");
        assert_eq!(all.metrics.members.len(), 2);
        assert_eq!(all.metrics.fleet.balance.periods, 1);
        assert_eq!(all.period.series_seconds, 180);
        let devbox = &all.metrics.members[0];
        assert_eq!(
            devbox.records_error.as_ref().unwrap().code,
            "member_unavailable"
        );
        assert_eq!(devbox.work.runs, 2);
        assert!(all.metrics.members[1].master && all.metrics.members[1].this_member);

        let builds = document(&query(None, Some("bu*")), &gathered());
        assert_eq!(builds.metrics.fleet.requests.total, 2);
        assert_eq!(builds.metrics.members.len(), 2, "load is not per label");

        let devbox_only = document(&query(Some("devbox"), None), &gathered());
        assert_eq!(devbox_only.metrics.members.len(), 1);
        assert_eq!(
            devbox_only.metrics.fleet.requests.total, 2,
            "made or ran there"
        );
        assert_eq!(devbox_only.metrics.fleet.balance.periods, 1);
        assert_eq!(
            devbox_only.metrics.members[0].work.share, all.metrics.members[0].work.share,
            "a share of the whole fleet's work"
        );
        let json = serde_json::to_value(&devbox_only).unwrap();
        assert_eq!(json["filters"]["member"], "devbox");
        assert!(json["fleet"]["balance"].is_object());
        assert!(json["accuracy"]["run"]["by_label"].is_array());
        assert_eq!(json["findings"][0]["kind"], "incomplete");
    }
}
