//! Carrying out the steps the master sent, one at a time, each as admitted work.
//!
//! One apply runs at a time on a member; another request while it runs is
//! `apply_in_progress`. Each step asks this member's scheduler for a deferrable,
//! pinned run labelled `elsecall tools install`, so it starts only while the
//! member has room beyond its reserve and no interactive work waits, and shows
//! in `elsecall runs` like any job. A step whose wait outlasts its hour starts
//! anyway at its deadline, still only when it fits. The step's commands are the
//! run's holders; the run ends with the step, exit 0 when it installed and 1
//! otherwise. A step whose manager does not install yet is skipped, and one
//! failed step does not stop the next: tools do not depend on each other.
//! The record is kept after every change, and its summary is this member's
//! `provisioning` status.
use crate::install::{
    InstallAdmission, Installer, StepFailure,
    record::{self, Failure, Record, State},
    request::InstallStep,
    work::AdmittedWork,
};
use elsecall_contract::{
    failure::{Failure as Refusal, Result},
    scheduling_model::{
        capacity::Cores,
        request::{AdmissionRequest, AtDeadline, Kind, Placement, Urgency},
        timestamp::Timestamp,
    },
};
use elsecall_fleet::provisioning_model::{ProvisioningReport, ProvisioningSummary};
use elsecall_scheduling::messages::RunEnded;
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

pub(crate) const LABEL: &str = "elsecall tools install";
/// How long a step waits for room before it starts at its deadline anyway.
const WAIT: Duration = Duration::from_secs(60 * 60);
/// What a step holds: installs download and unpack, and some compile.
fn size() -> Cores {
    Cores::from_millis(2_000)
}

pub struct Applier {
    admission: Arc<dyn InstallAdmission>,
    installers: Vec<Arc<dyn Installer>>,
    data: PathBuf,
    latest: Mutex<Option<Record>>,
}

impl Applier {
    /// The applier for this member, with the record `data` keeps, copying what
    /// it lacks from the master `fleet` records.
    pub fn new(
        data: PathBuf,
        admission: Arc<dyn InstallAdmission>,
        fleet: std::result::Result<Arc<elsecall_fleet::fleet::Fleet>, Refusal>,
    ) -> Arc<Self> {
        let source = Arc::new(crate::install::source::MasterSource(fleet));
        Self::with_installers(
            data,
            admission,
            vec![Arc::new(crate::install::mise::MiseInstaller { source })],
        )
    }

    pub(crate) fn with_installers(
        data: PathBuf,
        admission: Arc<dyn InstallAdmission>,
        installers: Vec<Arc<dyn Installer>>,
    ) -> Arc<Self> {
        let latest = record::load(&data);
        let applier = Arc::new(Self {
            admission,
            installers,
            data,
            latest: Mutex::new(latest.clone()),
        });
        // An apply cut short by a restart is kept as it now reads, so its
        // failures keep the time they were found.
        if let Some(record) = &latest {
            applier.keep(record);
        }
        applier
    }

    /// The latest apply on this member, if any.
    pub(crate) fn latest(&self) -> Option<Record> {
        self.lock().clone()
    }

    /// The record, also after a panic elsewhere: every change to it is whole.
    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Record>> {
        self.latest
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Starts carrying out `steps`, unless an apply already runs here. No steps
    /// is an apply too: it finishes at once, and the failures of the one before
    /// stop standing.
    pub(crate) fn start(self: &Arc<Self>, steps: Vec<InstallStep>) -> Result<Record> {
        let record = {
            let mut latest = self.lock();
            if latest.as_ref().is_some_and(Record::running) {
                return Err(Refusal::new(
                    "apply_in_progress",
                    "This member is already installing a plan.",
                    "Wait for it with elsecall tools applied --wait, then apply again.",
                ));
            }
            let mut record = Record::new(steps);
            for entry in &mut record.entries {
                if self.installer(&entry.step).is_none() {
                    entry.state = State::Skipped {
                        reason: "not_supported",
                    };
                }
            }
            if !record
                .entries
                .iter()
                .any(|entry| entry.state == State::Queued)
            {
                record.finished_at = Some(Timestamp::now());
            }
            *latest = Some(record.clone());
            record
        };
        self.keep(&record);
        if record.running() {
            let applier = Arc::clone(self);
            tokio::spawn(async move {
                let run = tokio::spawn(Arc::clone(&applier).run());
                if run.await.is_err() {
                    applier.cut_short("apply_failed");
                }
            });
        }
        Ok(record)
    }

    fn installer(&self, step: &InstallStep) -> Option<&Arc<dyn Installer>> {
        self.installers
            .iter()
            .find(|installer| installer.manager() == step.manager)
    }

    async fn run(self: Arc<Self>) {
        let count = self.latest().map_or(0, |record| record.entries.len());
        // A manager that could not be put in place fails its later steps with
        // the same failure, instead of trying again for each.
        let mut unusable: Vec<(crate::tool_inventory::Manager, StepFailure)> = Vec::new();
        for index in 0..count {
            let Some(entry) = self.latest().map(|record| record.entries[index].clone()) else {
                return;
            };
            let (State::Queued, Some(installer)) = (&entry.state, self.installer(&entry.step))
            else {
                continue;
            };
            let earlier = unusable
                .iter()
                .find(|(manager, _)| *manager == entry.step.manager)
                .map(|(_, failure)| *failure);
            if let Some(failure) = earlier {
                self.update(|record| {
                    record.entries[index].state = State::Failed(Failure {
                        stage: failure.stage.into(),
                        code: failure.code.into(),
                        at: Timestamp::now(),
                    })
                });
                continue;
            }
            self.update(|record| {
                record.entries[index].state = State::Running {
                    since: Timestamp::now(),
                }
            });
            let outcome = self.install(installer.as_ref(), &entry.step).await;
            if let Err(failure) = outcome
                && failure.stage == "bootstrap"
            {
                unusable.push((entry.step.manager, failure));
            }
            let state = match outcome {
                Ok(()) => State::Installed {
                    at: Timestamp::now(),
                },
                Err(failure) => State::Failed(Failure {
                    stage: failure.stage.into(),
                    code: failure.code.into(),
                    at: Timestamp::now(),
                }),
            };
            self.update(|record| record.entries[index].state = state);
        }
        self.update(|record| record.finished_at = Some(Timestamp::now()));
    }

    /// Ends an apply whose task stopped before its steps did.
    fn cut_short(&self, code: &str) {
        self.update(|record| {
            let now = Timestamp::now();
            for entry in &mut record.entries {
                if matches!(entry.state, State::Queued | State::Running { .. }) {
                    entry.state = State::Failed(Failure {
                        stage: "apply".into(),
                        code: code.into(),
                        at: now,
                    });
                }
            }
            record.finished_at = Some(now);
        });
    }

    /// One step, from admission to the end of its run.
    async fn install(
        &self,
        installer: &dyn Installer,
        step: &InstallStep,
    ) -> std::result::Result<(), StepFailure> {
        let request = AdmissionRequest {
            workload: None,
            kind: Kind::Run,
            holder_pid: None,
            size: Some(size()),
            label: Some(LABEL.into()),
            placement: Placement::Pinned,
            urgency: Urgency::Deferrable {
                deadline: Timestamp::now().after(WAIT),
                at_deadline: AtDeadline::Start,
            },
            workspace: None,
            settings: None,
        };
        let run = self
            .admission
            .admit(request)
            .await
            .map_err(|refusal| StepFailure {
                stage: "admission",
                // The scheduler refuses everything while the service stops.
                code: if refusal.code == "service_unavailable" {
                    "apply_interrupted"
                } else {
                    "install_not_admitted"
                },
            })?;
        let run = AdmittedRun {
            admission: Arc::clone(&self.admission),
            run,
            failed: true,
        };
        let work = AdmittedWork {
            admission: Arc::clone(&self.admission),
            run: run.run.clone(),
        };
        let outcome = installer.install(step, &work).await;
        run.end(outcome.is_err());
        outcome
    }

    fn update(&self, change: impl FnOnce(&mut Record)) {
        let record = {
            let mut latest = self.lock();
            let Some(record) = latest.as_mut() else {
                return;
            };
            change(record);
            record.clone()
        };
        self.keep(&record);
    }

    fn keep(&self, record: &Record) {
        if let Err(error) = record::save(&self.data, record) {
            elsecall_host::service_log!(
                "{}",
                serde_json::json!({"event": "tools_apply_unsaved", "detail": error})
            );
        }
    }
}

/// A step's admitted run, ended exactly once: with the step's outcome, or as
/// failed when the step is dropped or panics first, so its room is never held
/// past the step.
struct AdmittedRun {
    admission: Arc<dyn InstallAdmission>,
    run: elsecall_contract::scheduling_model::run_id::RunId,
    failed: bool,
}

impl AdmittedRun {
    fn end(mut self, failed: bool) {
        self.failed = failed;
    }
}

impl Drop for AdmittedRun {
    fn drop(&mut self) {
        self.admission.end(
            &self.run,
            RunEnded::Exited {
                exit_code: i32::from(self.failed),
                cpu_ms: None,
                run_phases: None,
                plan_observation: None,
            },
        );
    }
}

impl ProvisioningReport for Applier {
    fn summary(&self) -> Option<ProvisioningSummary> {
        self.latest().map(|record| record.summary())
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        install::{Ran, Work, applier::*, request::ApplyRequest},
        tool_inventory::Manager,
    };
    use elsecall_contract::scheduling_model::run_id::RunId;
    use futures_util::future::BoxFuture;
    use serde_json::json;

    /// Admits everything, unless told to refuse, and remembers what it saw.
    #[derive(Default)]
    struct Admissions {
        refuse: bool,
        requests: Mutex<Vec<AdmissionRequest>>,
        started: Mutex<Vec<u32>>,
        ended: Mutex<Vec<RunEnded>>,
    }

    impl InstallAdmission for Admissions {
        fn admit(&self, request: AdmissionRequest) -> BoxFuture<'_, Result<RunId>> {
            Box::pin(async move {
                self.requests.lock().unwrap().push(request);
                if self.refuse {
                    Err(Refusal::new("stopping", "stopping", "retry"))
                } else {
                    Ok(RunId::generate())
                }
            })
        }
        fn started(&self, _id: &RunId, pid: u32) {
            self.started.lock().unwrap().push(pid);
        }
        fn end(&self, _id: &RunId, ended: RunEnded) {
            self.ended.lock().unwrap().push(ended);
        }
    }

    /// Runs `/usr/bin/true` or `/usr/bin/false` for each step: false for node.
    struct Commands;

    impl Installer for Commands {
        fn manager(&self) -> Manager {
            Manager::Mise
        }
        fn install<'a>(
            &'a self,
            step: &'a InstallStep,
            work: &'a dyn Work,
        ) -> futures_util::future::BoxFuture<'a, std::result::Result<(), StepFailure>> {
            Box::pin(async move {
                let program = if step.tool.as_str() == "node" {
                    "/usr/bin/false"
                } else {
                    "/usr/bin/true"
                };
                match work
                    .run(
                        tokio::process::Command::new(program),
                        Duration::from_secs(10),
                    )
                    .await
                {
                    Ran::Ended(status) if status.success() => Ok(()),
                    _ => Err(StepFailure {
                        stage: "install",
                        code: "tool_install_failed",
                    }),
                }
            })
        }
    }

    fn steps() -> Vec<InstallStep> {
        serde_json::from_value::<ApplyRequest>(json!({"steps": [
            {"manager": "mise", "tool": "node", "version": "22.19.0"},
            {"manager": "mise", "tool": "bun", "version": "1.3.14"},
            {"manager": "xcodes", "tool": "xcode", "version": "26.2"},
        ]}))
        .unwrap()
        .accept()
        .unwrap()
    }

    async fn finished(applier: &Applier) -> Record {
        loop {
            let latest = applier.latest().unwrap();
            if !latest.running() {
                return latest;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn steps_run_one_at_a_time_as_admitted_work() {
        let data = tempfile::tempdir().unwrap();
        let admissions = Arc::new(Admissions::default());
        let applier = Applier::with_installers(
            data.path().to_owned(),
            admissions.clone(),
            vec![Arc::new(Commands)],
        );
        applier.start(steps()).unwrap();
        assert_eq!(
            applier.start(steps()).unwrap_err().code,
            "apply_in_progress"
        );
        let record = finished(&applier).await;
        let states: Vec<_> = record.entries.iter().map(|entry| &entry.state).collect();
        assert!(
            matches!(states[0], State::Failed(f) if f.stage == "install" && f.code == "tool_install_failed")
        );
        assert!(matches!(states[1], State::Installed { .. }));
        assert!(matches!(
            states[2],
            State::Skipped {
                reason: "not_supported"
            }
        ));
        let requests = admissions.requests.lock().unwrap().clone();
        assert_eq!(requests.len(), 2, "a skipped step asks for nothing");
        assert!(
            requests
                .iter()
                .all(|request| request.label.as_deref() == Some(LABEL)
                    && request.placement == Placement::Pinned
                    && matches!(
                        request.urgency,
                        Urgency::Deferrable {
                            at_deadline: AtDeadline::Start,
                            ..
                        }
                    ))
        );
        assert_eq!(
            admissions.started.lock().unwrap().len(),
            2,
            "each command is bound to its run"
        );
        let codes: Vec<i32> = admissions
            .ended
            .lock()
            .unwrap()
            .iter()
            .map(|ended| match ended {
                RunEnded::Exited { exit_code, .. } => *exit_code,
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(codes, [1, 0]);
        assert_eq!(
            record::load(data.path()).unwrap(),
            record,
            "the record is kept"
        );
        applier.start(steps()).unwrap();
        finished(&applier).await;
    }

    #[tokio::test]
    async fn a_step_the_scheduler_refuses_fails_at_admission() {
        let data = tempfile::tempdir().unwrap();
        let admissions = Arc::new(Admissions {
            refuse: true,
            ..Admissions::default()
        });
        let applier = Applier::with_installers(
            data.path().to_owned(),
            admissions.clone(),
            vec![Arc::new(Commands)],
        );
        applier.start(steps()).unwrap();
        let record = finished(&applier).await;
        assert!(matches!(&record.entries[0].state,
            State::Failed(f) if f.stage == "admission" && f.code == "install_not_admitted"));
        assert!(
            admissions.ended.lock().unwrap().is_empty(),
            "nothing was admitted to end"
        );
        let summary = applier.summary().unwrap();
        assert_eq!(summary.failed.len(), 2);
    }
}
