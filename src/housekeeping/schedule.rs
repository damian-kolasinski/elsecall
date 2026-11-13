//! Housekeeping as this member's own work: every interval the service asks its own
//! admission for a deferrable, pinned run, so a pass starts only while the member
//! has room beyond its reserve and no interactive work waits. A pass that cannot
//! start within its interval does what `at_deadline` says, and the next interval
//! tries again. Each attempt leaves a record labelled `elsecall housekeeping`.
use crate::{
    failure::Result,
    housekeeping::pass::Housekeeper,
    scheduling::{
        capacity::Cores,
        messages::RunEnded,
        request::{AdmissionRequest, Kind, Placement, Urgency},
        run_id::RunId,
        service::Scheduler,
        timestamp::Timestamp,
    },
};
use futures_util::future::BoxFuture;
use std::{sync::Arc, time::Duration};

pub const LABEL: &str = "elsecall housekeeping";
/// The first pass after the service starts waits at most this long.
const FIRST_PASS: Duration = Duration::from_secs(5 * 60);

/// How housekeeping asks for room on this member.
pub trait Admission: Send + Sync {
    /// Waits until the request starts; fails with `deadline_skipped` and the like.
    fn admit(&self, request: AdmissionRequest) -> BoxFuture<'_, Result<RunId>>;
    fn end(&self, id: &RunId, ended: RunEnded);
}

/// The member's own scheduler, asked as the service's own process.
impl Admission for Arc<Scheduler> {
    fn admit(&self, request: AdmissionRequest) -> BoxFuture<'_, Result<RunId>> {
        Box::pin(async move {
            self.request(std::process::id(), request)
                .await
                .map(|grant| grant.run_id)
        })
    }
    fn end(&self, id: &RunId, ended: RunEnded) {
        if let Err(error) = self.run_ended(std::process::id(), id, ended) {
            crate::service_log!(
                "{}",
                serde_json::json!({"event": "housekeeping_unrecorded", "detail": error})
            );
        }
    }
}

/// Runs passes every interval until the service stops.
pub fn start(
    housekeeper: Arc<Housekeeper>,
    admission: Arc<dyn Admission>,
) -> tokio::task::AbortHandle {
    tokio::spawn(async move {
        let interval = housekeeper.settings().interval;
        tokio::time::sleep(interval.min(FIRST_PASS)).await;
        loop {
            let attempt = tokio::time::Instant::now();
            let request = AdmissionRequest {
                workload: None,
                kind: Kind::Run,
                holder_pid: None,
                size: Some(Cores::ONE),
                label: Some(LABEL.into()),
                placement: Placement::Pinned,
                urgency: Urgency::Deferrable {
                    deadline: Timestamp::now().after(interval),
                    at_deadline: housekeeper.settings().at_deadline,
                },
                workspace: None,
                settings: None,
            };
            match admission.admit(request).await {
                Ok(id) => {
                    let report = housekeeper.pass(false).await;
                    admission.end(
                        &id,
                        if report.problems.is_empty() {
                            RunEnded::Exited {
                                plan_observation: None,
                                exit_code: 0,
                                cpu_ms: None,
                                run_phases: None,
                            }
                        } else {
                            RunEnded::Exited {
                                plan_observation: None,
                                exit_code: 1,
                                cpu_ms: None,
                                run_phases: None,
                            }
                        },
                    );
                }
                Err(error) => crate::service_log!(
                    "{}",
                    serde_json::json!({"event": "housekeeping_not_admitted", "detail": error})
                ),
            }
            tokio::time::sleep_until(attempt + interval).await;
        }
    })
    .abort_handle()
}
