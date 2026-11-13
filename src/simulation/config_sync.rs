//! Recorded pre-run configuration I/O at the real scheduler's sync boundary.
use super::world::World;
use crate::{
    failure::{Failure, Result},
    scheduling::{
        pre_run_sync::{PreRunSync, Synced},
        run_id::RunId,
    },
};
use futures_util::future::BoxFuture;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

pub(super) struct ConfigurationSync {
    pub(super) world: Arc<Mutex<World>>,
    pub(super) caller: String,
}

/// A detached pre-run operation can outlive its request's hold timer. Completing
/// an old operation must not erase a newer request's I/O deadline on this member.
struct PendingIo {
    world: Arc<Mutex<World>>,
    key: (String, String),
    deadline: u64,
}
impl Drop for PendingIo {
    fn drop(&mut self) {
        let mut world = self.world.lock().unwrap();
        if world.pending_pre_run.get(&self.key) == Some(&self.deadline) {
            world.pending_pre_run.remove(&self.key);
        }
    }
}
impl PreRunSync for ConfigurationSync {
    fn bring_up_to_date(&self, _: &str) -> BoxFuture<'static, Result<Synced>> {
        Box::pin(async { Ok(Synced::NothingSynced) })
    }

    fn bring_up_to_date_for(
        &self,
        member: &str,
        run_id: &RunId,
    ) -> BoxFuture<'static, Result<Synced>> {
        let id = run_id.to_string();
        let (duration, cause, deadline) = {
            let mut world = self.world.lock().unwrap();
            let key = (id.clone(), member.to_owned());
            let use_index = *world.pre_run_uses.entry(key.clone()).or_default();
            *world.pre_run_uses.get_mut(&key).unwrap() += 1;
            let complete = world
                .trace
                .requests
                .iter()
                .find(|r| r.fact.run_id == id)
                .is_some_and(|r| r.pre_run_attempts_complete);
            if !complete {
                world.missing_pre_run.insert(format!("requests[{id}].pre_run_attempts complete history unavailable; legacy final/fallback summaries are partial"));
            }
            let attempts: Vec<_> = world
                .trace
                .requests
                .iter()
                .find(|r| r.fact.run_id == id)
                .and_then(|r| r.pre_run_attempts.as_ref())
                .into_iter()
                .flatten()
                .filter(|a| a.member == member)
                .cloned()
                .collect();
            let attempt = attempts.get(use_index).or_else(|| attempts.last());
            if attempt.is_none() {
                world.missing_pre_run.insert(format!("requests[{id}].pre_run_attempts[{member}] missing; exploratory zero-latency successful configuration I/O used"));
            } else if use_index >= attempts.len() {
                world.missing_pre_run.insert(format!("requests[{id}].pre_run_attempts[{member}] intermediate retry history missing; last observed attempt reused"));
            }
            let duration = attempt.map_or(0, |a| a.duration_ms);
            let cause = attempt.and_then(|a| a.cause.clone());
            if cause.as_deref() == Some("hold_limit_passed") {
                world.missing_pre_run.insert(format!("requests[{id}].pre_run_attempts[{member}].duration_ms is right-censored at the recorded hold limit"));
            }
            let deadline = world.clock.now().millis().saturating_add(duration);
            world
                .pending_pre_run
                .insert((self.caller.clone(), member.to_owned()), deadline);
            (duration, cause, deadline)
        };
        let guard = PendingIo {
            world: self.world.clone(),
            key: (self.caller.clone(), member.to_owned()),
            deadline,
        };
        // Freeze the timer when the operation is dispatched, before first poll.
        let until = tokio::time::Instant::now() + Duration::from_millis(duration);
        Box::pin(async move {
            if duration > 0 {
                tokio::time::sleep_until(until).await;
            }
            drop(guard);
            match cause {
                None => Ok(Synced::NothingSynced),
                Some(cause) => Err(Failure::new(
                    &cause,
                    "Recorded configuration sync failure.",
                    "",
                )),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        reporting::facts::PlacementKind,
        scheduling::{placed::Warmth, record::WorkspaceIdentity, request::Placement},
        simulation::{Options, simulate},
        workload::trace::{PreRunAttempt, Trace, TraceOutcome},
    };
    fn trace() -> Trace {
        let mut trace = crate::simulation::engine::tests::trace();
        trace.requests.truncate(1);
        let request = &mut trace.requests[0];
        request.placement_request = Placement::Member("b".into());
        request.fact.placement = PlacementKind::Required;
        request.fact.member = Some("b".into());
        request.workspace = Some(WorkspaceIdentity {
            workspace: "a".repeat(64),
            repository: "b".repeat(64),
        });
        request.workspace_recorded = true;
        request.fact.warmth = Some(Warmth::Warm);
        request.fact.waited_ms = Some(500);
        request.fact.sync_ms = Some(0);
        request.fact.resources.as_mut().unwrap().started_at =
            Some(trace.since.after(Duration::from_millis(500)));
        request.fact.resources.as_mut().unwrap().ended_at =
            Some(trace.since.after(Duration::from_millis(1_500)));
        request.pre_run_attempts_complete = true;
        request.pre_run_attempts = Some(vec![PreRunAttempt {
            member: "b".into(),
            started_at: Some(trace.since),
            duration_ms: 500,
            cause: None,
        }]);
        trace
    }
    #[test]
    fn real_offer_waits_for_recorded_pre_run_io_and_obeys_the_real_hold_timer() {
        let baseline = simulate(trace(), Options::default()).unwrap();
        assert_eq!(baseline["outcomes"][0]["member"], "b");
        assert_eq!(baseline["outcomes"][0]["waited_ms"], 500);
        assert_eq!(baseline["outcomes"][0]["run_ms"], 1_000);
        let shortened = simulate(
            trace(),
            Options {
                settings: vec!["hold_limit=250ms".into()],
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(shortened["outcomes"][0]["failure"], "config_sync_failed");
        assert_eq!(shortened["outcomes"][0]["waited_ms"], 250);
        assert!(shortened["outcomes"][0]["member"].is_null());
    }
    #[test]
    fn recorded_failure_cause_reaches_the_real_pre_run_refusal_path() {
        let mut trace = trace();
        let request = &mut trace.requests[0];
        request.pre_run_attempts.as_mut().unwrap()[0].cause = Some("permission_denied".into());
        request.fact.member = None;
        request.fact.failure = Some("config_sync_failed".into());
        request.fact.run_ms = None;
        request.fact.resources.as_mut().unwrap().started_at = None;
        request.fact.resources.as_mut().unwrap().ended_at =
            Some(trace.since.after(Duration::from_millis(500)));
        request.outcome = Some(TraceOutcome::Failed {
            code: "config_sync_failed".into(),
        });
        let output = simulate(trace, Options::default()).unwrap();
        assert_eq!(output["outcomes"][0]["failure"], "config_sync_failed");
        assert_eq!(output["outcomes"][0]["waited_ms"], 500);
        assert!(output["outcomes"][0]["member"].is_null());
    }
    #[tokio::test(start_paused = true)]
    async fn successful_pre_run_retries_are_recorded_before_busy_and_acceptance() {
        use crate::{
            scheduling::{
                capacity::Cores,
                clock::{Clock, VirtualClock},
                messages::RunEnded,
                placed::WorkspaceRef,
                request::{AdmissionRequest, Kind, Urgency},
            },
            simulation::{
                adapters::Registry,
                member::{FleetAdapters, Member},
                model::Envelope,
                settings::Settings,
            },
        };
        let mut trace = trace();
        let id = RunId::parse(&trace.requests[0].fact.run_id).unwrap();
        trace.requests[0].pre_run_attempts = Some(
            (0..3)
                .map(|i| PreRunAttempt {
                    member: "b".into(),
                    started_at: Some(trace.since.after(Duration::from_millis(i * 500))),
                    duration_ms: 0,
                    cause: None,
                })
                .collect(),
        );
        trace.requests[0].pre_run_attempts_complete = true;
        let trace = Arc::new(trace);
        let clock: Arc<dyn Clock> = Arc::new(VirtualClock::new(trace.since));
        let world = Arc::new(Mutex::new(World::new(
            trace.clone(),
            clock.clone(),
            Envelope::Point,
        )));
        let registry: Registry = Arc::default();
        let adapters = FleetAdapters {
            world,
            registry,
            clock: clock.clone(),
            master: Some("a".into()),
        };
        let candidates = Settings::parse(&trace, &[]).unwrap();
        let temp = tempfile::tempdir().unwrap();
        let mut members = Vec::new();
        for (i, name) in ["a", "b"].into_iter().enumerate() {
            let baseline = &trace.members[i].scheduler_settings[0];
            let member = Member::start(
                name,
                baseline.capacity,
                baseline.parameters.clone(),
                &candidates,
                &adapters,
                temp.path(),
                i,
            )
            .unwrap();
            member.scheduler.tick().unwrap();
            members.push(member);
        }
        let [a, b] = members.as_slice() else {
            unreachable!()
        };
        b.processes.insert(301, 1);
        b.processes.insert(300, 301);
        b.ids
            .0
            .lock()
            .unwrap()
            .push_back(RunId::parse("r-000000000099").unwrap());
        let pinned = AdmissionRequest {
            workload: None,
            kind: Kind::Run,
            holder_pid: None,
            size: Some(Cores::ONE),
            label: None,
            placement: Placement::Pinned,
            urgency: Urgency::Interactive {
                hold_limit_ms: Some(5_000),
            },
            workspace: None,
            settings: None,
        };
        let holding = b.scheduler.request(300, pinned.clone()).await.unwrap();
        a.processes.insert(101, 1);
        a.processes.insert(100, 101);
        a.ids.0.lock().unwrap().push_back(id.clone());
        let requested = AdmissionRequest {
            placement: Placement::Member("b".into()),
            workspace: Some(WorkspaceRef {
                root: "/virtual/a".into(),
                common_dir: "/virtual/b".into(),
            }),
            ..pinned
        };
        let task = tokio::spawn({
            let scheduler = a.scheduler.clone();
            async move { scheduler.request(100, requested).await }
        });
        loop {
            tokio::task::yield_now().await;
            if a.ids.0.lock().unwrap().is_empty() && !a.has_ready_scheduler_work() {
                break;
            }
        }
        assert_eq!(
            a.scheduler
                .record(&id)
                .unwrap()
                .pre_run_history
                .unwrap()
                .attempts
                .len(),
            1
        );
        tokio::time::advance(Duration::from_millis(500)).await;
        loop {
            tokio::task::yield_now().await;
            if !a.has_ready_scheduler_work() {
                break;
            }
        }
        assert_eq!(
            a.scheduler
                .record(&id)
                .unwrap()
                .pre_run_history
                .unwrap()
                .attempts
                .len(),
            2
        );
        b.scheduler
            .run_ended(
                300,
                &holding.run_id,
                RunEnded::Exited {
                    plan_observation: None,
                    exit_code: 0,
                    cpu_ms: None,
                    run_phases: None,
                },
            )
            .unwrap();
        tokio::time::advance(Duration::from_millis(500)).await;
        while !task.is_finished() {
            tokio::task::yield_now().await;
        }
        let grant = task.await.unwrap().unwrap();
        assert_eq!(grant.waited_ms, 1_000);
        let history = a.scheduler.record(&id).unwrap().pre_run_history.unwrap();
        assert!(history.complete && !history.truncated);
        assert_eq!(history.attempts.len(), 3);
        assert!(
            history
                .attempts
                .iter()
                .all(|a| a.member == "b" && a.cause.is_none())
        );
        assert_eq!(
            history.attempts[2].started_at.millis() - history.attempts[0].started_at.millis(),
            1_000
        );
    }
}
