//! Validate peer trace projections before they cross into workload modelling.
use crate::{
    failure::Result,
    report_protocol::Cursor,
    reporting::{
        facts::is_code,
        window::{Window, invalid},
    },
    scheduling::{
        record::MAXIMUM_CPU_MS,
        request::{MAXIMUM_HOLD_LIMIT, Placement, Urgency},
        run_id::RunId,
    },
    workload::trace::{PAGE, Page, TraceOutcome, sanitize_decision},
};
fn name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 255
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}
fn id(s: &str) -> bool {
    RunId::parse(s).is_some()
}
// Decision strings are only names, enums and timestamps after diagnostics have
// been stripped. Bound collections and numbers before H2 consumes them.
fn decision_value(value: &serde_json::Value, key: &str) -> bool {
    match value {
        serde_json::Value::Object(o) => o.iter().all(|(k, v)| decision_value(v, k)),
        serde_json::Value::Array(a) => a.len() <= 4096 && a.iter().all(|v| decision_value(v, key)),
        serde_json::Value::String(s) => {
            if matches!(key, "member" | "winner") {
                name(s)
            } else {
                s.len() <= 255 && !s.chars().any(char::is_control)
            }
        }
        serde_json::Value::Number(n) => n
            .as_f64()
            .is_some_and(|v| v.is_finite() && (0.0..=1e15).contains(&v)),
        _ => true,
    }
}
pub fn page(mut page: Page, window: Window, before: Option<&Cursor>, member: &str) -> Result<Page> {
    let bad = || invalid("A member returned an invalid workload trace page.");
    if page.requests.len() > PAGE as usize || page.leftovers.len() > PAGE as usize {
        return Err(bad());
    }
    let upper = before.map_or((window.until.millis(), ""), |c| {
        (c.requested_at_ms, c.run_id.as_str())
    });
    for r in &mut page.requests {
        let f = &r.fact;
        if !f.believable()
            || r.execution_reference.is_some() // Frozen references are local projections, never peer telemetry.
            || r.synthetic_workspace // Scratch grouping is local, never historical telemetry.
            || (f.requested_at.millis(), f.run_id.as_str()) >= upper
            || !name(&f.calling_member)
            || f.member.as_deref().is_some_and(|m| !name(m))
            || r.nested_in.as_deref().is_some_and(|s| !id(s))
            || r.shares_capacity_of.as_deref().is_some_and(|s| !id(s))
            || f.resources.as_ref().is_some_and(|v| {
                v.started_at.is_some_and(|t| {
                    t > crate::scheduling::timestamp::Timestamp::now()
                        .after(std::time::Duration::from_secs(60))
                }) || v.ended_at.is_some_and(|t| {
                    t > crate::scheduling::timestamp::Timestamp::now()
                        .after(std::time::Duration::from_secs(60))
                })
            })
            || matches!(&r.outcome,Some(TraceOutcome::Failed{code}) if !is_code(code))
            || matches!(&r.placement_request,Placement::Member(m) if !name(m))
            || r.hold_limit.is_some_and(|h| h.seconds > MAXIMUM_HOLD_LIMIT)
            || r.workspace.as_ref().is_some_and(|w| {
                [&w.workspace, &w.repository]
                    .iter()
                    .any(|id| id.len() != 64 || !id.bytes().all(|b| b.is_ascii_hexdigit()))
            })
            || r.pre_run_attempts_complete
                && r.pre_run_attempts.as_ref().is_none_or(|attempts| {
                    attempts.iter().any(|attempt| attempt.started_at.is_none())
                })
            || r.pre_run_attempts.as_ref().is_some_and(|attempts| {
                attempts.len() > crate::scheduling::pre_run_sync::History::MAXIMUM_ATTEMPTS
                    || attempts.iter().any(|attempt| {
                        !attempt.believable()
                            || attempt.started_at.is_some_and(|at| at < f.requested_at)
                    })
            })
            || matches!(r.urgency,Urgency::Interactive{hold_limit_ms:Some(ms)} if ms>MAXIMUM_HOLD_LIMIT.as_millis() as u64)
        {
            return Err(bad());
        }
        if let Urgency::Deferrable { deadline, .. } = r.urgency
            && (deadline.since(f.requested_at) > crate::scheduling::request::MAXIMUM_DEADLINE
                || r.placement_request != Placement::Pinned)
        {
            return Err(bad());
        }
        if let Some(d) = r.decision.take() {
            let d = sanitize_decision(d);
            if d.candidates.len() > 256
                || d.arrival.as_ref().is_some_and(|a| a.candidates.len() > 256)
                || d.parameters.as_ref().is_some_and(|p| p.validate().is_err())
                || !decision_value(&serde_json::to_value(&d).expect("decision serializes"), "")
            {
                return Err(bad());
            }
            r.decision = Some(d);
        }
    }
    if page.scheduler_settings.len() > 1024
        || before.is_some() && !page.scheduler_settings.is_empty()
        || page
            .scheduler_settings
            .windows(2)
            .any(|pair| pair[0].at >= pair[1].at)
        || page.scheduler_settings.iter().enumerate().any(|(i, s)| {
            s.at >= window.until
                || i > 0 && s.at < window.since
                || s.parameters.validate().is_err()
                || s.capacity.capacity
                    != s.capacity
                        .weighted_cores
                        .saturating_sub(s.capacity.reserved)
        })
    {
        return Err(bad());
    }
    for l in &page.leftovers {
        if !id(&l.run_id)
            || l.member != member
            || l.since > window.since.after(crate::reporting::window::MAXIMUM_PERIOD)
            || l.since
                > crate::scheduling::timestamp::Timestamp::now()
                    .after(std::time::Duration::from_secs(60))
            || l.cap_ms > 86_400_000
            || l.cpu_ms.is_some_and(|ms| ms > MAXIMUM_CPU_MS)
            || matches!(&l.ending,Some(crate::scheduling::leftovers::Ending::NextRun{run_id}) if !id(run_id))
        {
            return Err(bad());
        }
    }
    if let Some(next) = &page.next
        && (!id(&next.run_id) || (next.requested_at_ms, next.run_id.as_str()) >= upper)
    {
        return Err(bad());
    }
    Ok(page)
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        reporting::fixtures::{self, Run, T0},
        scheduling::timestamp::Timestamp,
        workload::trace::Request,
    };
    #[test]
    fn rejects_peer_measurements_and_codes_and_strips_diagnostics() {
        let window = Window::new(
            Timestamp::from_millis(T0),
            Timestamp::from_millis(T0 + 3_600_000),
            Timestamp::from_millis(T0 + 3_600_000),
        )
        .unwrap();
        let mut r = Request::of(&fixtures::placed(Run {
            member: "devbox",
            ..Run::default()
        }))
        .unwrap();
        r.decision.as_mut().unwrap().reason = "PRIVATE_SENTINEL".into();
        let make = |r| Page {
            requests: vec![r],
            ..Page::default()
        };
        let accepted = page(make(r.clone()), window, None, "mini").unwrap();
        assert!(
            !serde_json::to_string(&accepted)
                .unwrap()
                .contains("PRIVATE_SENTINEL")
        );
        let mut frozen = r.clone();
        frozen.execution_reference = Some(crate::workload::trace::ExecutionReference::Unknown);
        assert!(page(make(frozen), window, None, "mini").is_err());
        let mut bad = r.clone();
        bad.fact.resources.as_mut().unwrap().cpu_ms = Some(u64::MAX);
        assert!(page(make(bad), window, None, "mini").is_err());
        let mut bad = r;
        bad.outcome = Some(TraceOutcome::Failed {
            code: "PRIVATE_SENTINEL".into(),
        });
        assert!(page(make(bad), window, None, "mini").is_err());
    }

    #[test]
    fn settings_and_workspace_identities_are_validated_at_the_peer_boundary() {
        use crate::scheduling::{
            capacity::{Capacity, Cores},
            parameters::Parameters,
            records::SchedulerSettings,
        };
        let window = Window::new(
            Timestamp::from_millis(T0),
            Timestamp::from_millis(T0 + 60_000),
            Timestamp::from_millis(T0 + 60_000),
        )
        .unwrap();
        let settings = SchedulerSettings {
            at: Timestamp::from_millis(T0 - 1000),
            capacity: Capacity {
                weighted_cores: Cores::from_millis(4000),
                reserved: Cores::ONE,
                capacity: Cores::from_millis(3000),
            },
            parameters: Parameters::default(),
        };
        let make = |s| Page {
            scheduler_settings: vec![s],
            ..Page::default()
        };
        assert!(page(make(settings.clone()), window, None, "mini").is_ok());
        let mut bad = settings.clone();
        bad.capacity.capacity = Cores::ONE;
        assert!(page(make(bad), window, None, "mini").is_err());
        let mut bad = settings.clone();
        bad.at = window.until;
        assert!(page(make(bad), window, None, "mini").is_err());
        assert!(
            page(
                make(settings),
                window,
                Some(&Cursor {
                    requested_at_ms: T0,
                    run_id: "r-000000000001".into()
                }),
                "mini"
            )
            .is_err()
        );
        let mut request = Request::of(&fixtures::placed(Run::default())).unwrap();
        request.workspace = Some(crate::scheduling::record::WorkspaceIdentity {
            workspace: "/private/raw/path".into(),
            repository: "a".repeat(64),
        });
        assert!(
            page(
                Page {
                    requests: vec![request],
                    ..Page::default()
                },
                window,
                None,
                "mini"
            )
            .is_err()
        );
    }
    #[test]
    fn peer_accepts_bounded_post_cutoff_leftover_evidence() {
        let start = Timestamp::from_millis(T0);
        let cutoff = Timestamp::from_millis(T0 + 60_000);
        let window = Window::new(start, cutoff, cutoff).unwrap();
        let make = |since| Page {
            leftovers: vec![crate::workload::trace::Leftover {
                member: "mini".into(),
                run_id: "r-000000000001".into(),
                since,
                cap_ms: 60_000,
                duration_ms: Some(1000),
                cpu_ms: Some(100),
                ending: Some(crate::scheduling::leftovers::Ending::Finished),
            }],
            ..Page::default()
        };
        assert!(
            page(
                make(Timestamp::from_millis(T0 + 120_000)),
                window,
                None,
                "mini"
            )
            .is_ok()
        );
        assert!(
            page(
                make(Timestamp::from_millis(T0 + 31 * 86_400_000)),
                window,
                None,
                "mini"
            )
            .is_err()
        );
    }
}
