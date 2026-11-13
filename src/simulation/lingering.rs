//! A copy is free to admit the next run while leftovers linger. That run settles
//! them before sync, using the production executor's end-to-end hold budget.
use super::{adapters::World, initial, model::Envelope};
use crate::{
    remote_run::{leftovers::STOPPING, runner::leftovers_wait_budget},
    scheduling::leftovers::Ending,
    workload::trace::{Request, Trace},
};
use std::time::Duration;

pub fn observed_wait(trace: &Trace, request: &Request) -> u64 {
    let Some(admitted) = initial::admitted_at(request) else {
        return 0;
    };
    trace
        .leftovers
        .iter()
        .filter(|l| {
            matches!(&l.ending,Some(Ending::NextRun{run_id}) if run_id==&request.fact.run_id)
                || (request.workspace.as_ref().is_some_and(|workspace| {
                    trace.requests.iter().any(|prior| {
                        prior.fact.run_id == l.run_id && prior.workspace.as_ref() == Some(workspace)
                    })
                }) && request.fact.member.as_ref() == Some(&l.member))
        })
        .filter_map(|l| {
            let end = l.since.millis() + l.duration_ms?;
            (l.since <= admitted && admitted.millis() < end).then_some(end - admitted.millis())
        })
        .max()
        .unwrap_or(0)
}

pub fn settle(
    world: &mut World,
    member: &str,
    workspace: &str,
    now: u64,
    remaining: Duration,
) -> (u64, bool) {
    let budget = leftovers_wait_budget(remaining).as_millis() as u64;
    let ids: Vec<_> = world
        .leftover_copies
        .iter()
        .filter(|(_, key)| key.0 == member && key.1 == workspace)
        .map(|(id, _)| id.clone())
        .collect();
    let mut wait = 0;
    let mut uncertain = false;
    for id in ids {
        let Some((_, _, natural_end)) = world.leftovers.get(&id).cloned() else {
            continue;
        };
        if natural_end <= now {
            continue;
        }
        let mut end = natural_end;
        if natural_end - now > budget {
            let latency = world
                .trace
                .leftovers
                .iter()
                .find(|l| l.run_id == id)
                .and_then(|l| {
                    let Some(Ending::NextRun { run_id }) = &l.ending else {
                        return None;
                    };
                    let next = world
                        .trace
                        .requests
                        .iter()
                        .find(|r| r.fact.run_id == *run_id)?;
                    let admitted = initial::admitted_at(next)?;
                    let hold = next.hold_limit?.seconds;
                    let remaining = next.fact.requested_at.after(hold).since(admitted);
                    let delay =
                        (l.since.millis() + l.duration_ms?).saturating_sub(admitted.millis());
                    Some(
                        delay
                            .saturating_sub(leftovers_wait_budget(remaining).as_millis() as u64)
                            .min(STOPPING.as_millis() as u64),
                    )
                });
            let stopping = match world.envelope {
                Envelope::Upper => STOPPING.as_millis() as u64,
                Envelope::Lower => 0,
                Envelope::Point => latency.unwrap_or(0),
            };
            uncertain |= latency.is_none() || world.envelope != Envelope::Point;
            end = end.min(now + budget + stopping);
            world.leftovers.get_mut(&id).unwrap().2 = end;
        }
        wait = wait.max(end - now);
    }
    (wait, uncertain)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduling::{clock::VirtualClock, timestamp::Timestamp};
    use std::sync::Arc;
    #[tokio::test(start_paused = true)]
    async fn next_run_claims_copy_then_settles_leftovers_with_real_hold_budget() {
        let trace = Arc::new(crate::simulation::engine::tests::trace());
        let mut world = World::new(
            trace,
            Arc::new(VirtualClock::new(Timestamp::from_millis(0))),
            Envelope::Point,
        );
        world
            .leftovers
            .insert("old".into(), ("b".into(), 1.0, 60000));
        world
            .leftover_copies
            .insert("old".into(), ("b".into(), "workspace".into()));
        assert_eq!(
            settle(&mut world, "b", "workspace", 0, Duration::from_secs(60)),
            (10000, true)
        );
        assert_eq!(world.leftovers["old"].2, 10000);
        let mut upper = world;
        upper.envelope = Envelope::Upper;
        upper.leftovers.get_mut("old").unwrap().2 = 60000;
        assert_eq!(
            settle(&mut upper, "b", "workspace", 0, Duration::from_secs(5)),
            (8000, true)
        );
    }
}
