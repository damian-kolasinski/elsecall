//! Explicit projections: never serialize a Record wholesale into a trace.
use crate::{
    load_log::buckets::Bucket,
    reporting::facts::Fact,
    scheduling::{
        leftovers::{Ending, Leftovers},
        record::{Decision, HoldLimitRecord, Outcome, Record},
        request::{Placement, Urgency},
        timestamp::Timestamp,
    },
    workload::{contention::Curve, demand::Demand},
};
use serde::{Deserialize, Serialize};

pub const VERSION: u32 = 1;
pub const PAGE: u32 = 10;

/// Frozen source physics for a selected arrival window. Unknown stays unknown
/// after context is trimmed; this is model input, never a measured ideal.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExecutionReference {
    Unknown,
    Known {
        milliseconds: f64,
        identified: bool,
        member_speed: Option<f64>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Request {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_reference: Option<ExecutionReference>,
    #[serde(flatten)]
    pub fact: Fact,
    pub placement_request: Placement,
    pub urgency: Urgency,
    pub hold_limit: Option<HoldLimitRecord>,
    pub nested_in: Option<String>,
    pub shares_capacity_of: Option<String>,
    #[serde(default)]
    pub workspace: Option<crate::scheduling::record::WorkspaceIdentity>,
    /// Known absence is distinct from historical missing workspace evidence.
    #[serde(default)]
    pub workspace_recorded: bool,
    /// A rehearsal workload may group legacy work into scratch repositories.
    /// Such an identity is never evidence about a historical workspace.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub synthetic_workspace: bool,
    /// None distinguishes older exports with no attempt evidence.
    #[serde(default)]
    pub pre_run_attempts: Option<Vec<PreRunAttempt>>,
    #[serde(default)]
    pub pre_run_attempts_complete: bool,
    pub outcome: Option<TraceOutcome>,
    /// Numeric decision inputs, settings, queues and estimates; no workspace paths.
    pub decision: Option<Decision>,
}

/// Configuration pre-run projection: timing and cause only, never synced paths,
/// generations, warning text or file contents.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreRunAttempt {
    pub member: String,
    #[serde(default)]
    pub started_at: Option<Timestamp>,
    pub duration_ms: u64,
    pub cause: Option<String>,
}
impl PreRunAttempt {
    fn of(record: &crate::scheduling::pre_run_sync::PreRunSyncRecord) -> Self {
        Self {
            member: record.member.clone(),
            started_at: None,
            duration_ms: record.duration_ms,
            cause: match &record.outcome {
                crate::scheduling::pre_run_sync::Recorded::Failed { cause } => Some(cause.clone()),
                _ => None,
            },
        }
    }
    pub fn believable(&self) -> bool {
        !self.member.is_empty()
            && self.member.len() <= 255
            && self
                .member
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
            && self.duration_ms <= 86_400_000
            && self.cause.as_ref().is_none_or(|cause| {
                !cause.is_empty()
                    && cause.len() <= 48
                    && cause.starts_with(|c: char| c.is_ascii_lowercase())
                    && cause
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
            })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TraceOutcome {
    Exited { exit_code: i32 },
    Signaled { signal: i32 },
    Released,
    HolderExited,
    Failed { code: String },
    Cancelled,
}
impl Request {
    /// Terminal refusals have no execution interval. Their recorded decision
    /// still proves that they stopped holding/waiting for capacity.
    pub fn terminal_at(&self) -> Option<Timestamp> {
        self.fact
            .resources
            .as_ref()
            .and_then(|v| v.ended_at)
            .or_else(|| {
                self.decision
                    .as_ref()
                    .filter(|d| {
                        !self.fact.unfinished
                            && self.fact.member.is_none()
                            && d.verdict != crate::scheduling::record::Verdict::Started
                            && matches!(
                                self.outcome,
                                Some(TraceOutcome::Failed { .. } | TraceOutcome::Cancelled)
                            )
                    })
                    .map(|d| d.decided_at)
            })
    }
    pub fn of(record: &Record) -> Option<Self> {
        Some(Self {
            execution_reference: None,
            fact: Fact::of(record)?,
            placement_request: record.placement.clone(),
            urgency: record.urgency,
            hold_limit: record.hold_limit,
            nested_in: record.nested_in.as_ref().map(ToString::to_string),
            shares_capacity_of: record.shares_capacity_of.as_ref().map(ToString::to_string),
            workspace: record.workspace.clone(),
            workspace_recorded: record.workspace_recorded,
            synthetic_workspace: false,
            pre_run_attempts: record
                .pre_run_history
                .as_ref()
                .map(|history| {
                    history
                        .attempts
                        .iter()
                        .map(|attempt| PreRunAttempt {
                            member: attempt.member.clone(),
                            started_at: Some(attempt.started_at),
                            duration_ms: attempt.duration_ms,
                            cause: attempt.cause.clone(),
                        })
                        .collect()
                })
                .or_else(|| {
                    (record.config_sync.is_some() || !record.fallbacks.is_empty()).then(|| {
                        record
                            .fallbacks
                            .iter()
                            .map(|fallback| PreRunAttempt::of(&fallback.failed_sync))
                            .chain(record.config_sync.iter().map(PreRunAttempt::of))
                            .collect()
                    })
                }),
            pre_run_attempts_complete: record
                .pre_run_history
                .as_ref()
                .is_some_and(|history| history.complete && !history.truncated),
            outcome: record.outcome.as_ref().map(|outcome| match outcome {
                Outcome::Exited { exit_code } => TraceOutcome::Exited {
                    exit_code: *exit_code,
                },
                Outcome::Signaled { signal } => TraceOutcome::Signaled { signal: *signal },
                Outcome::Released => TraceOutcome::Released,
                Outcome::HolderExited => TraceOutcome::HolderExited,
                Outcome::Failed { code } => TraceOutcome::Failed { code: code.clone() },
                Outcome::Cancelled { .. } => TraceOutcome::Cancelled,
            }),
            decision: record.decision.clone().map(sanitize_decision),
        })
    }
}

/// Free-form diagnostics are unnecessary for simulation and are never exported.
pub fn sanitize_decision(mut d: Decision) -> Decision {
    d.reason.clear();
    for c in &mut d.candidates {
        c.why.clear();
    }
    for u in &mut d.unreachable {
        u.why.clear();
    }
    d
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Leftover {
    pub run_id: String,
    pub member: String,
    pub since: Timestamp,
    pub cap_ms: u64,
    pub duration_ms: Option<u64>,
    pub cpu_ms: Option<u64>,
    pub ending: Option<Ending>,
}
impl Leftover {
    pub fn of(record: &Record) -> Option<Self> {
        let placed = record.placed.as_ref()?;
        let (since, cap_ms, duration_ms, cpu_ms, ending) = match placed.leftovers.as_ref()? {
            Leftovers::Running { since, cap_ms, .. } => (*since, *cap_ms, None, None, None),
            Leftovers::Ended {
                since,
                cap_ms,
                duration_ms,
                cpu_ms,
                ending,
            } => (*since, *cap_ms, *duration_ms, *cpu_ms, Some(ending.clone())),
        };
        Some(Self {
            run_id: record.run_id.to_string(),
            member: placed.member.clone(),
            since,
            cap_ms,
            duration_ms,
            cpu_ms,
            ending,
        })
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Page {
    pub requests: Vec<Request>,
    pub leftovers: Vec<Leftover>,
    pub next: Option<crate::report_protocol::Cursor>,
    /// First-page member settings, independent of whether any requests exist.
    #[serde(default)]
    pub scheduler_settings: Vec<crate::scheduling::records::SchedulerSettings>,
}

impl Page {
    pub fn of_records(records: &[Record]) -> Self {
        // Read at most PAGE raw rows, but return a smaller prefix when rich
        // attempt history would overflow the signed member-channel envelope.
        // A single oversized row is still emitted: the channel reports its
        // explicit size error instead of claiming empty successful history.
        const PAYLOAD_BUDGET: usize = 750 * 1024;
        let mut page = Self::default();
        let mut bytes = 256usize;
        let mut consumed = 0usize;
        for record in records {
            let request = Request::of(record);
            let leftover = Leftover::of(record);
            let added = request.as_ref().map_or(0, |r| {
                serde_json::to_vec(r).expect("request serializes").len() + 1
            }) + leftover.as_ref().map_or(0, |l| {
                serde_json::to_vec(l).expect("leftover serializes").len() + 1
            });
            if consumed > 0 && bytes + added > PAYLOAD_BUDGET {
                break;
            }
            page.requests.extend(request);
            page.leftovers.extend(leftover);
            bytes += added;
            consumed += 1;
            if bytes > PAYLOAD_BUDGET {
                break;
            }
        }
        if consumed > 0 && (consumed < records.len() || records.len() == PAGE as usize) {
            let record = &records[consumed - 1];
            page.next = Some(crate::report_protocol::Cursor {
                requested_at_ms: record.requested_at.millis(),
                run_id: record.run_id.to_string(),
            });
        }
        page
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Member {
    pub member: String,
    /// One-minute observations include weighted cores, usable capacity (reserve
    /// is their difference), speed range, queues, pressure and outside-load proxy.
    pub load: Vec<Bucket>,
    pub errors: Vec<String>,
    #[serde(default)]
    pub scheduler_settings: Vec<crate::scheduling::records::SchedulerSettings>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Trace {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement_replay: Option<crate::replay_placement::Provenance>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::rehearsal::recording::deserialize"
    )]
    pub rehearsal: Option<crate::rehearsal::recording::Recording>,
    pub trace_version: u32,
    /// Full-fleet collection completeness, including deliberate member subsets;
    /// fidelity and model identification are separate.
    pub incomplete: bool,
    pub since: Timestamp,
    pub until: Timestamp,
    pub master: Option<String>,
    pub members: Vec<Member>,
    /// Arrivals in the window, plus retained carry-in work active at its start.
    pub requests: Vec<Request>,
    pub leftovers: Vec<Leftover>,
    pub demand: Vec<Demand>,
    pub contention: Vec<Curve>,
    pub limits: Vec<String>,
}
impl Trace {
    pub fn model(&mut self) {
        self.incomplete |= self.members.iter().any(|m| !m.errors.is_empty());
        self.members.sort_by(|a, b| a.member.cmp(&b.member));
        self.leftovers.sort_by(|a, b| {
            a.since
                .cmp(&b.since)
                .then(a.member.cmp(&b.member))
                .then(a.run_id.cmp(&b.run_id))
        });
        self.requests.sort_by(|a, b| {
            a.fact
                .requested_at
                .cmp(&b.fact.requested_at)
                .then(a.fact.run_id.cmp(&b.fact.run_id))
        });
        let facts: Vec<_> = self.requests.iter().map(|r| r.fact.clone()).collect();
        let loads: Vec<_> = self
            .members
            .iter()
            .map(|m| (m.member.as_str(), m.load.as_slice()))
            .collect();
        self.demand = super::demand::estimate(&facts, &loads);
        self.contention = super::contention::derive(&facts, &loads);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        reporting::fixtures::{self, Run},
        scheduling::placed::Side,
    };
    #[test]
    fn projection_covers_replay_fields_and_excludes_paths_processes_and_free_text() {
        let mut record = fixtures::placed(Run {
            member: "devbox",
            ..Run::default()
        });
        record.warnings.push("SECRET_SENTINEL".into());
        let workspace = crate::scheduling::placed::WorkspaceRef {
            root: "/private/SECRET_SENTINEL/tree".into(),
            common_dir: "/private/SECRET_SENTINEL/repository".into(),
        };
        record.workspace = Some(crate::scheduling::record::WorkspaceIdentity::of(
            "mini", &workspace,
        ));
        let placed = record.placed.as_mut().unwrap();
        placed.caller_workspace = "/private/SECRET_SENTINEL".into();
        placed.copy = Some("/SECRET_SENTINEL".into());
        placed.repository = Some("/SECRET_SENTINEL".into());
        record.outcome = Some(Outcome::Cancelled {
            reason: "SECRET_SENTINEL".into(),
        });
        let r = Request::of(&record).unwrap();
        let json = serde_json::to_string(&r).unwrap();
        assert!(!json.contains("SECRET_SENTINEL"));
        let reread: Request = serde_json::from_str(&json).unwrap();
        assert_eq!(reread.fact, r.fact);
        assert_eq!(reread.urgency, record.urgency);
        assert_eq!(reread.placement_request, record.placement);
        assert_eq!(reread.fact.sync_ms, Some(1000));
        assert!(reread.decision.is_some());
        assert_eq!(reread.workspace, record.workspace);
        assert_eq!(
            record.workspace,
            Some(crate::scheduling::record::WorkspaceIdentity::of(
                "mini", &workspace
            ))
        );
        assert_ne!(
            record.workspace,
            Some(crate::scheduling::record::WorkspaceIdentity::of(
                "other", &workspace
            ))
        );
        record.placed.as_mut().unwrap().side = Side::Member;
        assert!(Request::of(&record).is_none());
    }
    #[test]
    fn pre_run_projection_keeps_timings_and_causes_without_synced_paths_or_reasons() {
        use crate::scheduling::pre_run_sync::{Fallback, PreRunSyncRecord, Recorded};
        let mut record = fixtures::placed(Run::default());
        record.pre_run_history = None;
        record.config_sync = Some(PreRunSyncRecord {
            member: "devbox".into(),
            duration_ms: 500,
            outcome: Recorded::Current {
                generation: 17,
                paths: vec!["/SECRET_SENTINEL".into()],
            },
        });
        record.fallbacks.push(Fallback {
            failed_sync: PreRunSyncRecord::failed("other", 250, "permission_denied".into()),
            decided_at: record.requested_at,
            moved_to: Some("devbox".into()),
            reason: "SECRET_SENTINEL".into(),
        });
        let request = Request::of(&record).unwrap();
        let attempts = request.pre_run_attempts.as_ref().unwrap();
        assert_eq!(attempts.len(), 2);
        assert_eq!(attempts[0].cause.as_deref(), Some("permission_denied"));
        assert_eq!(attempts[1].duration_ms, 500);
        let json = serde_json::to_string(&request).unwrap();
        assert!(!json.contains("SECRET_SENTINEL"));
        assert!(!json.contains("generation"));
        assert!(attempts.iter().all(PreRunAttempt::believable));
        record.config_sync = None;
        record.fallbacks.clear();
        record.workspace_recorded = false;
        assert!(Request::of(&record).unwrap().pre_run_attempts.is_none());
    }

    #[test]
    fn large_attempt_histories_paginate_by_bytes_without_losing_rows() {
        use crate::scheduling::pre_run_sync::{Attempt, History};
        let mut records = Vec::new();
        for n in [2, 1] {
            let mut record = fixtures::pinned(n, "large", "mini", 1.0);
            record.requested_at = Timestamp::from_millis(1_000 + n);
            record.pre_run_history = Some(History {
                attempts: (0..History::MAXIMUM_ATTEMPTS)
                    .map(|_| Attempt {
                        member: "devbox".into(),
                        started_at: record.requested_at,
                        duration_ms: 500,
                        cause: Some("permission_denied".into()),
                    })
                    .collect(),
                complete: true,
                truncated: false,
            });
            records.push(record);
        }
        let first = Page::of_records(&records);
        assert_eq!(first.requests.len(), 1);
        assert!(serde_json::to_vec(&first).unwrap().len() < 750 * 1024);
        let cursor = first.next.unwrap();
        assert_eq!(cursor.run_id, records[0].run_id.to_string());
        let rest = Page::of_records(&records[1..]);
        assert_eq!(rest.requests.len(), 1);
        assert_eq!(rest.requests[0].fact.run_id, records[1].run_id.to_string());
        assert!(rest.next.is_none());
    }

    #[test]
    fn command_ended_before_window_keeps_its_overlapping_leftovers() {
        use crate::scheduling::{
            leftovers::{Ending, Leftovers},
            records::Records,
            timestamp::Timestamp,
        };
        let temp = tempfile::tempdir().unwrap();
        let db = Records::open(&temp.path().join("records.sqlite")).unwrap();
        let mut r = fixtures::placed(Run {
            member: "devbox",
            ..Run::default()
        });
        r.requested_at = Timestamp::from_millis(1_000);
        r.ended_at = Some(Timestamp::from_millis(2_000));
        r.placed.as_mut().unwrap().leftovers = Some(Leftovers::Ended {
            since: Timestamp::from_millis(2_000),
            cap_ms: 10_000,
            duration_ms: Some(8_000),
            cpu_ms: Some(3000),
            ending: Ending::Finished,
        });
        db.save(&r).unwrap();
        let kept = db
            .overlapping(
                Timestamp::from_millis(5_000),
                Timestamp::from_millis(20_000),
                None,
                PAGE,
            )
            .unwrap();
        assert_eq!(kept.len(), 1);
        assert_eq!(
            Page::of_records(&kept).leftovers[0].duration_ms,
            Some(8_000)
        );
        assert!(
            db.overlapping(
                Timestamp::from_millis(11_000),
                Timestamp::from_millis(20_000),
                None,
                PAGE
            )
            .unwrap()
            .is_empty()
        );
    }

    #[test]
    fn carry_in_and_equal_time_pagination_read_without_mutating_history() {
        use crate::scheduling::{records::Records, timestamp::Timestamp};
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("records.sqlite");
        let db = Records::open(&path).unwrap();
        for n in 1..=12 {
            let mut r = fixtures::pinned(n, "a", "mini", 10.0);
            r.requested_at = Timestamp::from_millis(1000);
            r.started_at = Some(Timestamp::from_millis(1000));
            r.ended_at = Some(Timestamp::from_millis(if n == 1 { 2000 } else { 20_000 }));
            db.save(&r).unwrap();
        }
        let reader = Records::open_read_only(&path).unwrap();
        let first = reader
            .overlapping(
                Timestamp::from_millis(5000),
                Timestamp::from_millis(30_000),
                None,
                PAGE,
            )
            .unwrap();
        assert_eq!(first.len(), 10);
        let next = Page::of_records(&first).next.unwrap();
        let rest = reader
            .overlapping(
                Timestamp::from_millis(5000),
                Timestamp::from_millis(30_000),
                Some((Timestamp::from_millis(next.requested_at_ms), &next.run_id)),
                PAGE,
            )
            .unwrap();
        assert_eq!(rest.len(), 1);
        assert_ne!(rest[0].run_id, first[0].run_id);
        assert_eq!(db.recent(100).unwrap().len(), 12);
    }
    #[test]
    fn workspace_recording_distinguishes_known_outside_tree_from_legacy_missing_data() {
        let record = fixtures::pinned(1, "outside", "mini", 1.0);
        assert!(record.workspace_recorded);
        assert!(record.workspace.is_none());
        let request = Request::of(&record).unwrap();
        assert!(request.workspace_recorded);
        assert!(request.workspace.is_none());
        let mut old_record_json = serde_json::to_value(&record).unwrap();
        old_record_json
            .as_object_mut()
            .unwrap()
            .remove("workspace_recorded");
        let old_record: Record = serde_json::from_value(old_record_json).unwrap();
        assert!(!old_record.workspace_recorded);
        assert!(!Request::of(&old_record).unwrap().workspace_recorded);
        let mut old_trace_json = serde_json::to_value(request).unwrap();
        old_trace_json
            .as_object_mut()
            .unwrap()
            .remove("workspace_recorded");
        let old_request: Request = serde_json::from_value(old_trace_json).unwrap();
        assert!(!old_request.workspace_recorded);
    }
}
