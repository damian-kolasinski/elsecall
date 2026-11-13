//! Recorded days are monitored as the service's own pinned deferrable work.
//! Missing inputs are retained as not_checkable, never a successful validation.
use crate::{
    failure::Result,
    paths::Paths,
    scheduling::{
        capacity::Cores,
        messages::RunEnded,
        request::{AdmissionRequest, AtDeadline, Kind, Placement, Urgency},
        service::Scheduler,
        timestamp::Timestamp,
    },
};
use std::{sync::Arc, time::Duration};
const DAY: u64 = 86_400_000;
pub async fn monitor(paths: &Paths, scheduler: &Scheduler) -> Result<()> {
    let today = Timestamp::now().millis() / DAY * DAY;
    let days = (today.saturating_sub(29 * DAY)..today)
        .step_by(DAY as usize)
        .collect::<Vec<_>>();
    for day in days.into_iter().rev() {
        let since = Timestamp::from_millis(day);
        let until = Timestamp::from_millis(day + DAY);
        if crate::tuning_trust::monitored(paths, since)? {
            continue;
        }
        // Check only days for which this member actually recorded inputs.
        if scheduler.trace_records(since, until, None, 1)?.is_empty()
            && crate::load_log::store::LoadStore::open_read_only(&paths.load_history())?
                .between(since, until)?
                .is_empty()
        {
            continue;
        }
        let context = Timestamp::from_millis(
            day.saturating_sub(14 * DAY)
                .max(Timestamp::now().millis().saturating_sub(30 * DAY)),
        );
        let trace = crate::trace_cli::collect(paths, context, until).await?;
        let simulation = tokio::task::spawn_blocking(move || {
            crate::simulation::simulate(
                trace,
                crate::simulation::Options {
                    fidelity: true,
                    day: Some(since),
                    ..Default::default()
                },
            )
        })
        .await
        .map_err(|_| crate::rehearsal::invalid("Drift fidelity task failed."))?;
        match simulation {
            Ok(simulation) => {
                crate::tuning_trust::record_day(paths, since, &simulation["fidelity"])?
            }
            Err(error) => crate::tuning_trust::record_day(
                paths,
                since,
                &serde_json::json!({"status":"blocked_by_missing_evidence","error":error.code,"blockers":[error.message]}),
            )?,
        }
        // One day per admitted pass, newest first. Permanently incomplete
        // history must not monopolize housekeeping or delay an idle rehearsal.
        break;
    }
    Ok(())
}
pub fn start(
    paths: Paths,
    scheduler: Arc<Scheduler>,
    interval: Duration,
) -> tokio::task::AbortHandle {
    // Cleanup may be configured less often than daily. Drift still needs
    // enough passes to check every newly completed day and drain its backlog.
    let interval = interval.min(Duration::from_secs(3600));
    tokio::spawn(async move {
        tokio::time::sleep(interval.min(Duration::from_secs(60))).await;
        loop {
            let request = AdmissionRequest {
                workload: None,
                kind: Kind::Run,
                holder_pid: None,
                size: Some(Cores::ONE),
                label: Some("elsecall fidelity drift".into()),
                placement: Placement::Pinned,
                urgency: Urgency::Deferrable {
                    deadline: Timestamp::now().after(interval),
                    at_deadline: AtDeadline::Skip,
                },
                workspace: None,
                settings: None,
            };
            if let Ok(grant) = scheduler.request(std::process::id(), request).await {
                let result = monitor(&paths, &scheduler).await;
                let _ = scheduler.run_ended(
                    std::process::id(),
                    &grant.run_id,
                    RunEnded::Exited {
                        plan_observation: None,
                        exit_code: i32::from(result.is_err()),
                        cpu_ms: None,
                        run_phases: None,
                    },
                );
                if let Err(error) = result {
                    crate::service_log!(
                        "{}",
                        serde_json::json!({"event":"fidelity_monitor_failed","code":error.code})
                    );
                }
            }
            tokio::time::sleep(interval).await;
        }
    })
    .abort_handle()
}
