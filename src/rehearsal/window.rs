//! Choose a synthetic rehearsal population, without hiding competing work.
use crate::{
    failure::{Failure, Result},
    scheduling::timestamp::Timestamp,
    workload::trace::Trace,
};
use serde::Serialize;
use std::{collections::BTreeMap, num::NonZeroUsize};

#[derive(Clone, Copy, Debug)]
pub struct Busiest {
    duration_ms: u64,
    minimum_interactive: NonZeroUsize,
}
impl Busiest {
    pub fn with_minimum(mut self, minimum: NonZeroUsize) -> Self {
        self.minimum_interactive = minimum;
        self
    }
}
impl std::str::FromStr for Busiest {
    type Err = Failure;
    fn from_str(text: &str) -> Result<Self> {
        if text == "explicit" {
            return Ok(Self {
                duration_ms: 0,
                minimum_interactive: NonZeroUsize::new(40).unwrap(),
            });
        }
        let duration = text.strip_prefix("busiest:")
            .and_then(|s| humantime::parse_duration(s).ok())
            .filter(|d| d.as_millis() >= 1 && *d <= crate::reporting::window::MAXIMUM_PERIOD)
            .ok_or_else(|| crate::reporting::window::invalid(
                "--rehearsal-window must be busiest:DURATION, from 1ms to 30d (for example busiest:2h)."))?;
        Ok(Self {
            duration_ms: duration.as_millis() as u64,
            minimum_interactive: NonZeroUsize::new(40).unwrap(),
        })
    }
}
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Disposition {
    Eligible,
    Chosen,
    Rejected { reason: String },
    LowerRank,
}
#[derive(Clone, Debug, Serialize)]
pub struct Candidate {
    pub since: Timestamp,
    pub until: Timestamp,
    pub interactive_roots: usize,
    #[serde(flatten)]
    pub disposition: Disposition,
    pub blockers: Vec<String>,
}
#[derive(Debug, Serialize)]
pub struct Selection {
    pub policy: &'static str,
    pub duration_ms: u64,
    pub minimum_interactive: NonZeroUsize,
    pub source_since: Timestamp,
    pub source_until: Timestamp,
    pub chosen: Option<Candidate>,
    pub reason: &'static str,
    /// Earliest representative of every interval where score or eligibility
    /// changes. Later starts in that interval cannot improve the ranking.
    pub candidates: Vec<Candidate>,
}

/// Inclusive ranges of invalid *start times*. Sweeping them avoids re-scanning
/// every request and every load minute for each possible window.
struct Blockers {
    first: u64,
    last: u64,
    events: BTreeMap<u64, Vec<(String, bool)>>,
}
impl Blockers {
    fn add(&mut self, first: u64, last: u64, reason: String) {
        let first = first.max(self.first);
        let last = last.min(self.last);
        if first > last {
            return;
        }
        self.events
            .entry(first)
            .or_default()
            .push((reason.clone(), true));
        if last < self.last {
            self.events
                .entry(last + 1)
                .or_default()
                .push((reason, false));
        }
    }
    fn overlaps(&mut self, first: u64, until: u64, width: u64, reason: String) {
        if until > first {
            self.add(first.saturating_sub(width - 1), until - 1, reason);
        }
    }
}

pub fn select(source: &Trace, policy: Busiest) -> Result<(Trace, Selection)> {
    crate::simulation::validate_trace(source)?;
    let mut prepared = source.clone();
    prepared.requests.retain(|r| !super::internal(r));
    super::workspace::prepare(&mut prepared.requests);
    let source = &prepared;
    let first = source.since.millis();
    let end = source.until.millis();
    let width = if policy.duration_ms == 0 {
        end - first
    } else {
        policy.duration_ms
    };
    let mut report = Selection {
        policy: if policy.duration_ms == 0 {
            "explicit"
        } else {
            "busiest"
        },
        duration_ms: width,
        minimum_interactive: policy.minimum_interactive,
        source_since: source.since,
        source_until: source.until,
        chosen: None,
        reason: "No valid rehearsal window exists; candidates name a decisive rejection reason.",
        candidates: Vec::new(),
    };
    let no_window = |report: Selection| {
        Failure::new(
        "no_rehearsal_window", report.reason,
        "Choose a duration with enough interactive arrivals and an idle start, or lower --rehearsal-min-interactive. Identified execution inputs are still required.",
    ).with_details(report)
    };
    if width > end - first {
        report.reason = "Requested rehearsal duration exceeds the collected history span.";
        return Err(no_window(report));
    }
    let last = end - width;
    let mut blockers = Blockers {
        first,
        last,
        events: BTreeMap::new(),
    };
    blockers.events.entry(first).or_default();
    if source.incomplete || source.master.is_none() {
        blockers.add(
            first,
            last,
            "incomplete_fleet_collection_or_unknown_master".into(),
        );
    }
    for member in &source.members {
        for error in &member.errors {
            blockers.add(
                first,
                last,
                format!("member_error: {}: {error}", member.member),
            );
        }
    }
    let mut interactive = Vec::new();
    let inputs = super::inputs::Inputs::new(source);
    // Missing load in the arrival interval blocks every overlapping window.
    // Missing drain load additionally blocks windows containing that arrival.
    let gaps = crate::rehearsal::coverage::gaps(source);
    for gap in &gaps {
        blockers.overlaps(gap.since, gap.until, width, gap.reason());
    }
    for request in &source.requests {
        let f = &request.fact;
        let at = f.requested_at.millis();
        // Every arrival can change the planner's input contract, including
        // deferrable and nested work which does not contribute to the score.
        for change in [at.saturating_sub(width - 1), at + 1] {
            if (first..=last).contains(&change) {
                blockers.events.entry(change).or_default();
            }
        }
        let terminal = request.terminal_at();
        // Include admission/setup/queue time as well as command execution.
        // A missing end is unknown, never evidence that a member was idle.
        let terminal = terminal.map_or(end, |t| t.millis());
        for gap in &gaps {
            if gap.since < terminal && gap.until > at {
                blockers.overlaps(at, at + 1, width, format!("{}: {}", gap.reason(), f.run_id));
            }
        }
        blockers.add(
            at + 1,
            terminal.saturating_sub(1),
            format!("active_carry_in: {}", f.run_id),
        );
        let reason = if f.synthetic || super::synthetic(f.label.as_deref()) {
            Some(format!("synthetic_work: {}", f.run_id))
        } else if f.unfinished || request.outcome.is_none() {
            Some(format!("missing_terminal_outcome: {}", f.run_id))
        } else {
            None
        };
        if let Some(reason) = reason {
            blockers.overlaps(at, at + 1, width, reason);
        }
        if !f.nested && at >= first {
            for reason in inputs.problems(request, 1000.0) {
                blockers.overlaps(at, at + 1, width, reason);
            }
        }
        if !f.nested && !f.deferrable && !f.synthetic && !super::synthetic(f.label.as_deref()) {
            interactive.push(at);
        }
    }
    for (id, reason) in super::plan::nested_problems(source) {
        let at = source
            .requests
            .iter()
            .find(|r| r.fact.run_id == id)
            .expect("nested diagnostic identity")
            .fact
            .requested_at
            .millis();
        blockers.overlaps(at, at + 1, width, format!("{id}: {reason}"));
    }
    for leftover in &source.leftovers {
        blockers.overlaps(
            leftover.since.millis(),
            leftover
                .duration_ms
                .map_or(end, |ms| leftover.since.millis() + ms),
            width,
            format!(
                "unsupported_leftover_work: {}: {}",
                leftover.member, leftover.run_id
            ),
        );
    }
    interactive.sort_unstable();
    let mut active: BTreeMap<String, usize> = BTreeMap::new();
    for (at, changes) in blockers.events {
        for (reason, add) in changes {
            if add {
                *active.entry(reason).or_default() += 1;
            } else {
                let count = active.get_mut(&reason).expect("balanced blocker events");
                *count -= 1;
                if *count == 0 {
                    active.remove(&reason);
                }
            }
        }
        let count = interactive.partition_point(|a| *a < at + width)
            - interactive.partition_point(|a| *a < at);
        let disposition = match active.first_key_value() {
            Some((reason, _)) => Disposition::Rejected {
                reason: reason.clone(),
            },
            None if count < policy.minimum_interactive.get() => Disposition::Rejected {
                reason: format!(
                    "insufficient_interactive_population: {count} < {}",
                    policy.minimum_interactive
                ),
            },
            None => Disposition::Eligible,
        };
        report.candidates.push(Candidate {
            since: Timestamp::from_millis(at),
            until: Timestamp::from_millis(at + width),
            interactive_roots: count,
            blockers: active
                .keys()
                .cloned()
                .chain((count < policy.minimum_interactive.get()).then(|| {
                    format!(
                        "insufficient_interactive_population: {count} < {}",
                        policy.minimum_interactive
                    )
                }))
                .collect(),
            disposition,
        });
    }
    let mut ranked: Vec<_> = report
        .candidates
        .iter()
        .enumerate()
        .filter(|(_, c)| matches!(c.disposition, Disposition::Eligible))
        .map(|(i, _)| i)
        .collect();
    ranked.sort_by_key(|i| {
        (
            std::cmp::Reverse(report.candidates[*i].interactive_roots),
            report.candidates[*i].since,
        )
    });
    let mut selected = None;
    for i in ranked {
        let candidate = &report.candidates[i];
        let mut trace = slice(source, candidate);
        inputs.freeze(&mut trace);
        // Reuse the rehearsal's actual input contract, including nested roots,
        // demand on every eligible member and supported terminal outcomes.
        // Compression-specific runtime limits are checked again by rehearse.
        match super::plan::Plan::new(&trace, 1000.0) {
            Ok(_) => {
                report.candidates[i].disposition = Disposition::Chosen;
                report.chosen = Some(report.candidates[i].clone());
                selected = Some(trace);
                break;
            }
            Err(e) => {
                report.candidates[i].disposition = Disposition::Rejected {
                    reason: e.message.clone(),
                };
                report.candidates[i].blockers.push(e.message)
            }
        }
    }
    for c in &mut report.candidates {
        if matches!(c.disposition, Disposition::Eligible) {
            c.disposition = Disposition::LowerRank;
        }
    }
    match selected {
        Some(mut trace) => {
            report.reason = "Maximum real interactive root arrivals among idle-start windows meeting the minimum population and rehearsal input contract; ties choose the earliest start. Missing workspaces use synthetic family/caller identities; historical settings are not required. Load covers arrivals and historical drain. Internal service work stays in measured background load. Demand uses the full source history.";
            trace.limits.push(format!(
                "rehearsal window selected from {}..{}: {}; {} real interactive roots",
                source.since,
                source.until,
                report.reason,
                report.chosen.as_ref().unwrap().interactive_roots
            ));
            Ok((trace, report))
        }
        None => Err(no_window(report)),
    }
}
/// Select exactly these arrival bounds, with the same validation as automatic selection.
pub fn explicit(
    source: &Trace,
    since: Timestamp,
    until: Timestamp,
    minimum: NonZeroUsize,
) -> Result<(Trace, Selection)> {
    if since < source.since || until > source.until || since >= until {
        return Err(crate::reporting::window::invalid(
            "Explicit arrival bounds must be nonempty and inside the source history.",
        ));
    }
    // Keep load beyond until; it is evidence, not additional arrivals.
    let bounded = bound(source, since, until)?;
    select(
        &bounded,
        Busiest {
            duration_ms: until.millis() - since.millis(),
            minimum_interactive: minimum,
        },
    )
}

/// Project arrival bounds while freezing work from all retained competitors.
/// This is a synthetic-workload projection, never a fidelity input rewrite.
pub fn bound(source: &Trace, since: Timestamp, until: Timestamp) -> Result<Trace> {
    crate::simulation::validate_trace(source)?;
    if since < source.since || until > source.until || since >= until {
        return Err(crate::reporting::window::invalid(
            "Explicit arrival bounds must be nonempty and inside the source history.",
        ));
    }
    let mut prepared = source.clone();
    prepared.requests.retain(|r| !super::internal(r));
    super::workspace::prepare(&mut prepared.requests);
    let mut bounded = prepared.clone();
    super::inputs::Inputs::new(&prepared).freeze(&mut bounded);
    bounded.since = since;
    bounded.until = until;
    bounded.requests.retain(|r| r.fact.requested_at < until);
    bounded.leftovers.retain(|l| l.since < until);
    for member in &mut bounded.members {
        member
            .load
            .retain(|b| b.at.millis() + 60_000 > since.millis());
    }
    Ok(bounded)
}

fn slice(source: &Trace, window: &Candidate) -> Trace {
    let mut trace = source.clone();
    trace.since = window.since;
    trace.until = window.until;
    // Keep completed earlier history for estimates and duration references.
    // The blocker sweep guarantees none of it is active at the chosen start.
    trace
        .requests
        .retain(|r| r.fact.requested_at < window.until);
    trace.leftovers.clear();
    for member in &mut trace.members {
        member
            .load
            .retain(|b| b.at.millis() + 60_000 > window.since.millis());
        let baseline = member
            .scheduler_settings
            .partition_point(|s| s.at <= window.since)
            .saturating_sub(1);
        member.scheduler_settings = member
            .scheduler_settings
            .iter()
            .skip(baseline)
            .cloned()
            .collect();
    }
    trace
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        reporting::fixtures::{T0, base, bucket},
        scheduling::{
            capacity::{Capacity, Cores},
            parameters::Parameters,
            record::{HoldBasis, HoldLimitRecord, Outcome},
            records::SchedulerSettings,
            request::Urgency,
        },
        workload::{
            demand::Demand,
            trace::{Member, Request},
        },
    };
    use std::time::Duration;
    fn fixture() -> Trace {
        let mut requests = Vec::new();
        for (id, minute) in [(1, 2), (2, 3), (3, 8), (4, 9), (5, 10)] {
            let mut r = base(id, "build", "alpha");
            r.requested_at = Timestamp::from_millis(T0 + minute * 60_000);
            r.started_at = Some(r.requested_at);
            r.ended_at = Some(r.requested_at.after(Duration::from_secs(10)));
            r.run_ms = Some(10_000);
            r.member = Some("alpha".into());
            r.outcome = Some(Outcome::Exited { exit_code: 0 });
            r.hold_limit = Some(HoldLimitRecord {
                seconds: Duration::from_secs(10),
                basis: HoldBasis::Requested,
                expected_run_seconds: None,
            });
            requests.push(Request::of(&r).unwrap());
        }
        let capacity = Capacity {
            weighted_cores: Cores::from_millis(10_000),
            reserved: Cores::ONE,
            capacity: Cores::from_millis(9000),
        };
        Trace {
            rehearsal: None,
            placement_replay: None,
            trace_version: 1,
            incomplete: false,
            since: Timestamp::from_millis(T0),
            until: Timestamp::from_millis(T0 + 15 * 60_000),
            master: Some("alpha".into()),
            members: vec![Member {
                member: "alpha".into(),
                errors: vec![],
                load: (0..15)
                    .map(|m| {
                        let mut b = bucket(m, 0.1, 0.0, 0, 0, 1.0);
                        b.at = Timestamp::from_millis(T0 + m * 60_000);
                        b
                    })
                    .collect(),
                scheduler_settings: vec![SchedulerSettings {
                    at: Timestamp::from_millis(T0),
                    capacity,
                    parameters: Parameters::default(),
                }],
            }],
            requests,
            leftovers: vec![],
            demand: vec![Demand {
                member: "alpha".into(),
                family: "build".into(),
                runs: 5,
                fitted_runs: 5,
                declared_cores: Some(2.0),
                process_tree_cores: Some(1.0),
                effective_cores: Some(1.0),
                interval_cores: Some([0.5, 1.5]),
                fitted_minutes: 10,
                idle_minutes: 3,
                outside_baseline_cores: Some(0.0),
                residual_rmse_cores: Some(0.1),
                status: "estimated_with_idle_baseline".into(),
            }],
            contention: vec![],
            limits: vec![],
        }
    }
    fn choose(t: &Trace) -> Result<(Trace, Selection)> {
        select(
            t,
            "busiest:4m"
                .parse::<Busiest>()
                .unwrap()
                .with_minimum(NonZeroUsize::new(1).unwrap()),
        )
    }
    fn rejects(report: &Selection, text: &str) -> bool {
        report.candidates.iter().any(
            |c| matches!(&c.disposition, Disposition::Rejected { reason } if reason.contains(text)),
        )
    }
    #[test]
    fn selected_windows_preserve_work_normalized_with_later_competitors() {
        let mut t = fixture();
        t.requests.truncate(2);
        let root = &mut t.requests[0];
        root.fact.run_ms = Some(180_000);
        root.fact.resources.as_mut().unwrap().ended_at =
            Some(Timestamp::from_millis(T0 + 5 * 60_000));
        let other = &mut t.requests[1];
        other.fact.requested_at = Timestamp::from_millis(T0 + 4 * 60_000);
        other.fact.run_ms = Some(60_000);
        other.fact.resources.as_mut().unwrap().started_at = Some(other.fact.requested_at);
        other.fact.resources.as_mut().unwrap().ended_at =
            Some(Timestamp::from_millis(T0 + 5 * 60_000));
        t.demand[0].effective_cores = Some(8.0);
        t.demand[0].interval_cores = Some([8.0, 8.0]);
        let inputs = super::super::inputs::Inputs::new(&t);
        let full = inputs.arrival(&t.requests[0], 0, 1.0).unwrap();
        assert_eq!(full.request.duration_ms, 157_500);
        let window = Candidate {
            since: Timestamp::from_millis(T0 + 60_000),
            until: Timestamp::from_millis(T0 + 3 * 60_000),
            interactive_roots: 1,
            disposition: Disposition::Eligible,
            blockers: vec![],
        };
        let mut selected = slice(&t, &window);
        assert_ne!(
            super::super::plan::Plan::new(&selected, 1.0)
                .unwrap()
                .arrivals[0]
                .request
                .duration_ms,
            full.request.duration_ms
        );
        inputs.freeze(&mut selected);
        let selected: Trace =
            serde_json::from_slice(&serde_json::to_vec(&selected).unwrap()).unwrap();
        assert_eq!(
            super::super::plan::Plan::new(&selected, 1.0)
                .unwrap()
                .arrivals[0]
                .request
                .duration_ms,
            full.request.duration_ms
        );
        assert_eq!(
            crate::simulation::freeze_reference(&selected, &selected.requests[0]),
            crate::simulation::freeze_reference(&t, &t.requests[0])
        );
    }
    #[test]
    fn busiest_valid_window_wins_and_preserves_background_and_history() {
        let mut t = fixture();
        let mut background = t.requests[3].clone();
        background.fact.run_id = "r-000000000006".into();
        background.fact.deferrable = true;
        background.urgency = Urgency::Deferrable {
            deadline: t.until,
            at_deadline: crate::scheduling::request::AtDeadline::Start,
        };
        t.requests.push(background);
        let (selected, report) = choose(&t).unwrap();
        assert_eq!(report.chosen.as_ref().unwrap().interactive_roots, 3);
        // Half-open end includes the arrival at minute 10 only at +1ms.
        assert_eq!(selected.since.millis(), T0 + 6 * 60_000 + 1);
        assert_eq!(selected.until.millis(), T0 + 10 * 60_000 + 1);
        assert_eq!(selected.requests.len(), 6);
        assert!(selected.requests.iter().any(|r| r.fact.deferrable));
        assert!(
            selected
                .requests
                .iter()
                .any(|r| r.fact.requested_at < selected.since)
        );
        super::super::plan::Plan::new(&selected, 6.0).unwrap();
    }
    #[test]
    fn carry_in_on_another_member_excludes_the_denser_window() {
        let mut t = fixture();
        let mut member = t.members[0].clone();
        member.member = "bravo".into();
        t.members.push(member);
        let mut carry = t.requests[0].clone();
        carry.fact.run_id = "r-000000000006".into();
        carry.fact.calling_member = "bravo".into();
        carry.fact.member = Some("bravo".into());
        carry.fact.deferrable = true;
        carry.urgency = Urgency::Deferrable {
            deadline: t.until,
            at_deadline: crate::scheduling::request::AtDeadline::Start,
        };
        carry.fact.requested_at = Timestamp::from_millis(T0 + 4 * 60_000);
        // Admission/setup spans the boundary before command execution starts.
        carry.fact.resources.as_mut().unwrap().started_at =
            Some(Timestamp::from_millis(T0 + 8 * 60_000));
        carry.fact.resources.as_mut().unwrap().ended_at = Some(t.until);
        carry.fact.run_ms = Some(7 * 60_000);
        t.requests.push(carry);
        let (selected, report) = choose(&t).unwrap();
        assert_eq!(report.chosen.unwrap().interactive_roots, 2);
        assert_eq!(selected.since, t.since);
        assert!(rejects(
            &choose(&t).unwrap().1,
            "active_carry_in: r-000000000006"
        ));
    }
    #[test]
    fn rehearsal_uses_legacy_history_without_workspace_settings_or_load_coverage() {
        for missing in ["workspace", "settings", "load", "speed"] {
            let mut t = fixture();
            match missing {
                "workspace" => {
                    t.requests[4].workspace = None;
                    t.requests[4].workspace_recorded = false;
                }
                "settings" => {
                    t.members[0].scheduler_settings.clear();
                }
                "load" => {
                    t.members[0].load[10].seconds = 20;
                }
                "speed" => {
                    t.members[0].load[10].speed_min = None;
                }
                _ => unreachable!(),
            }
            let (selected, report) = choose(&t).unwrap();
            assert_eq!(report.chosen.unwrap().interactive_roots, 3, "{missing}");
            super::super::plan::Plan::new(&selected, 6.0).unwrap();
            assert!(selected.requests.last().unwrap().workspace.is_some());
        }
    }
    #[test]
    fn minimum_population_defaults_to_forty_and_can_be_configured() {
        let t = fixture();
        let e = select(&t, "busiest:4m".parse().unwrap()).unwrap_err();
        assert_eq!(e.code, "no_rehearsal_window");
        let report = e.details.unwrap();
        assert_eq!(report["minimum_interactive"], 40);
        assert!(report["candidates"].as_array().unwrap().iter().any(|c| {
            c["reason"]
                .as_str()
                .is_some_and(|r| r.contains("insufficient_interactive_population"))
        }));
        let policy = "busiest:4m"
            .parse::<Busiest>()
            .unwrap()
            .with_minimum(NonZeroUsize::new(3).unwrap());
        assert_eq!(
            select(&t, policy)
                .unwrap()
                .1
                .chosen
                .unwrap()
                .interactive_roots,
            3
        );
        assert!(select(&t, policy.with_minimum(NonZeroUsize::new(4).unwrap())).is_err());
        let mut populated = fixture();
        let root = populated.requests[2].clone();
        populated.requests = (1..=40)
            .map(|id| {
                let mut r = root.clone();
                r.fact.run_id = format!("r-{id:012x}");
                r
            })
            .collect();
        assert_eq!(
            select(&populated, "busiest:4m".parse().unwrap())
                .unwrap()
                .1
                .chosen
                .unwrap()
                .interactive_roots,
            40
        );
        populated.requests.pop();
        assert!(select(&populated, "busiest:4m".parse().unwrap()).is_err());
        let mut background = root.clone();
        background.fact.run_id = "r-000000000041".into();
        background.fact.deferrable = true;
        background.urgency = Urgency::Deferrable {
            deadline: populated.until,
            at_deadline: crate::scheduling::request::AtDeadline::Start,
        };
        populated.requests.push(background);
        assert!(select(&populated, "busiest:4m".parse().unwrap()).is_err());
    }
    #[test]
    fn no_valid_window_reports_candidates_and_never_invents_execution_inputs() {
        let mut t = fixture();
        t.demand[0].effective_cores = None;
        t.demand[0].interval_cores = None;
        let e = choose(&t).unwrap_err();
        assert_eq!(e.code, "no_rehearsal_window");
        let report = e.details.unwrap();
        assert!(report["chosen"].is_null());
        assert!(report["candidates"].as_array().unwrap().iter().any(|c| {
            c["reason"]
                .as_str()
                .is_some_and(|r| r.contains("effective demand"))
        }));
        for invalid in [
            "2h",
            "busiest:0s",
            "busiest:31d",
            "busiest:1ns",
            "random:1h",
        ] {
            assert!(invalid.parse::<Busiest>().is_err());
        }
        assert!(select(&fixture(), "busiest:1h".parse().unwrap()).is_err());
    }
    #[test]
    fn end_at_start_is_idle_but_unknown_end_is_not() {
        let mut t = fixture();
        let start = Timestamp::from_millis(T0 + 6 * 60_000 + 1);
        t.requests[1].fact.resources.as_mut().unwrap().ended_at = Some(start);
        assert_eq!(choose(&t).unwrap().0.since, start);
        t.requests[1].fact.resources.as_mut().unwrap().ended_at = None;
        t.requests[1].fact.unfinished = true;
        assert!(choose(&t).is_err());
    }

    #[test]
    fn event_candidates_match_exhaustive_millisecond_ranking() {
        for scenario in 0..3 {
            let mut t = fixture();
            t.until = Timestamp::from_millis(T0 + 100);
            t.members[0].load.truncate(1);
            for (r, offset) in t.requests.iter_mut().zip([10, 20, 60, 65, 70]) {
                r.fact.requested_at = Timestamp::from_millis(T0 + offset);
                let resources = r.fact.resources.as_mut().unwrap();
                resources.started_at = Some(r.fact.requested_at);
                resources.ended_at = Some(Timestamp::from_millis(T0 + offset + 2));
                r.fact.run_ms = Some(2);
            }
            if scenario == 1 {
                t.requests[4].workspace_recorded = false;
            }
            if scenario == 2 {
                t.requests[0].fact.resources.as_mut().unwrap().ended_at =
                    Some(Timestamp::from_millis(T0 + 55));
            }
            let width = 20;
            let oracle = (T0..=T0 + 100 - width)
                .filter_map(|start| {
                    let end = start + width;
                    if t.requests.iter().any(|r| {
                        let at = r.fact.requested_at.millis();
                        at < start
                            && r.fact
                                .resources
                                .as_ref()
                                .unwrap()
                                .ended_at
                                .unwrap()
                                .millis()
                                > start
                    }) {
                        return None;
                    }
                    let count = t
                        .requests
                        .iter()
                        .filter(|r| (start..end).contains(&r.fact.requested_at.millis()))
                        .count();
                    (count > 0).then_some((std::cmp::Reverse(count), start))
                })
                .min()
                .unwrap();
            let (selected, report) = select(
                &t,
                Busiest {
                    duration_ms: width,
                    minimum_interactive: NonZeroUsize::new(1).unwrap(),
                },
            )
            .unwrap();
            assert_eq!(
                (
                    std::cmp::Reverse(report.chosen.unwrap().interactive_roots),
                    selected.since.millis()
                ),
                oracle
            );
        }
    }

    #[test]
    fn older_terminal_refusal_does_not_become_unknown_carry_in() {
        let mut t = fixture();
        let mut refusal = t.requests[0].clone();
        refusal.fact.run_id = "r-000000000006".into();
        refusal.fact.member = None;
        refusal.fact.resources.as_mut().unwrap().started_at = None;
        refusal.fact.resources.as_mut().unwrap().ended_at = None;
        refusal.fact.completed = false;
        refusal.outcome = Some(crate::workload::trace::TraceOutcome::Failed {
            code: "fleet_busy".into(),
        });
        refusal.fact.failure = Some("fleet_busy".into());
        let mut decision = crate::reporting::fixtures::decision(
            crate::scheduling::record::Verdict::FleetBusy,
            vec![],
            None,
        );
        decision.decided_at = refusal.fact.requested_at.after(Duration::from_secs(1));
        refusal.decision = Some(decision);
        t.requests.push(refusal);
        let (selected, report) = choose(&t).unwrap();
        assert_eq!(report.chosen.unwrap().interactive_roots, 3);
        super::super::plan::Plan::new(&selected, 6.0).unwrap();
    }

    #[test]
    fn missing_demand_rejections_are_reported_for_a_large_history() {
        let mut t = fixture();
        let template = t.requests[0].clone();
        t.requests = (1..=2000)
            .map(|i| {
                let mut r = template.clone();
                r.fact.run_id = format!("r-{i:012x}");
                r.fact.requested_at = Timestamp::from_millis(T0 + i * 3);
                r.fact.resources.as_mut().unwrap().started_at = Some(r.fact.requested_at);
                r.fact.resources.as_mut().unwrap().ended_at =
                    Some(Timestamp::from_millis(T0 + i * 3 + 1));
                r.fact.run_ms = Some(1);
                r
            })
            .collect();
        t.until = Timestamp::from_millis(T0 + 10000);
        t.members[0].load.truncate(1);
        t.demand[0].effective_cores = None;
        t.demand[0].interval_cores = None;
        t.demand[0].status = "insufficient_idle_minutes_requires_3".into();
        let error = select(
            &t,
            "busiest:10ms"
                .parse::<Busiest>()
                .unwrap()
                .with_minimum(NonZeroUsize::new(1).unwrap()),
        )
        .unwrap_err();
        assert_eq!(error.code, "no_rehearsal_window");
        assert!(
            error.details.unwrap()["candidates"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c["reason"]
                    .as_str()
                    .is_some_and(|r| r.contains("insufficient_idle_minutes_requires_3")))
        );
    }
}
