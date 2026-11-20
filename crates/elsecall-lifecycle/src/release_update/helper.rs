//! Detached installer: verified stage, drain, transaction, readiness, selection and rollback.
use super::{
    JobId, Phase, ReleaseDownload, StagedRelease, invalid,
    selection::{CliSelection, Snapshot},
    store::{self, Store},
};
use crate::installation::{ServiceManager, Transaction};
use crate::window_app::{self, WindowLauncher};
use elsecall_contract::build_identity::Digest;
use elsecall_contract::failure::{Failure, Result};
use elsecall_host::client;
use elsecall_host::{paths::Paths, private_files};
use std::{
    fs,
    path::PathBuf,
    time::{Duration, Instant},
};

struct WorkerCleanup(PathBuf);
impl Drop for WorkerCleanup {
    fn drop(&mut self) {
        let _ = private_files::remove_file(&self.0);
    }
}

pub async fn run(
    paths: &Paths,
    id: &JobId,
    download: &dyn ReleaseDownload,
    selection: &dyn CliSelection,
    manager: &dyn ServiceManager,
    window: &dyn WindowLauncher,
) -> Result<()> {
    let store = Store::new(paths);
    // Status probes briefly acquire the same lock; allow them (and an exiting
    // previous helper) to finish rather than abandoning a new job on collision.
    let deadline = Instant::now() + Duration::from_secs(5);
    let _lock = loop {
        if let Some(lock) = store.helper_lock()? {
            break lock;
        }
        if Instant::now() >= deadline {
            return Err(invalid("Another release helper is running."));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let mut job = store
        .job()?
        .filter(|job| &job.id == id && matches!(job.phase, Phase::Starting))
        .ok_or_else(|| invalid("No update is waiting for this helper."))?;
    let _worker = WorkerCleanup(store.worker(id));
    let mut changed = false;
    let mut selected = false;
    let mut selection_attempted = false;
    let mut stage_directory = None;
    let mut previous_selection: Option<Snapshot> = None;
    let mut transaction: Option<Transaction> = None;
    let mut old_digest = None;
    let mut old_window = false;
    let attempt: Result<()> = async {
        store.advance(&mut job, Phase::Downloading)?;
        let stage = store.stage(paths, id)?;
        stage_directory = Some(stage.clone());
        let staged = download.stage(&job.release, &stage)?;
        check_stage(&staged)?;
        previous_selection = selection.snapshot()?;
        store::write(&store.selection(), &previous_selection)?;
        store.advance(&mut job, Phase::WaitingForWork)?;
        let _: serde_json::Value = client::post_within(
            &paths.socket(),
            "/v1/update/prepare",
            serde_json::json!({"id":id}),
            Duration::from_secs(job.wait_seconds + 10),
        )
        .await?;
        transaction = Some(Transaction::begin(paths)?);
        let original =
            fs::read(paths.executable()).map_err(|e| Failure::io("Back up installed CLI", e))?;
        let digest = Digest::of(&original);
        if digest.build_id() != job.from_build {
            return Err(invalid(
                "The installed CLI changed before this update could install.",
            ));
        }
        old_digest = Some(digest);
        private_files::directory(&store.backup())?;
        private_files::write(&store.backup().join("elsecall"), &original, 0o700)?;
        old_window = paths.window_executable().is_file();
        if old_window {
            let bytes = fs::read(paths.window_executable())
                .map_err(|e| Failure::io("Back up installed window", e))?;
            private_files::write(&store.backup().join("elsecall-ui"), &bytes, 0o700)?;
        } else {
            private_files::remove_file(&store.backup().join("elsecall-ui"))?;
        }
        check_stage(&staged)?;
        store.advance(&mut job, Phase::Installing)?;
        changed = true;
        transaction
            .as_ref()
            .expect("transaction")
            .install(&staged.executable, manager, None)?;
        crate::installation::ready::confirm(
            paths,
            &staged.digest.build_id(),
            paths.fleet_membership().exists(),
        )
        .await?;
        // Health alone is insufficient: the original window follows the job
        // through this API, which must survive the service restart too.
        let status: super::Status = client::get(&paths.socket(), "/v1/update").await?;
        if status.current_version != job.release.version
            || !status
                .job
                .is_some_and(|active| active.id == job.id && active.phase.installation_started())
        {
            return Err(invalid(
                "The updated service cannot report this update's progress.",
            ));
        }
        store.advance(&mut job, Phase::SelectingCli)?;
        if let Some(previous) = &previous_selection {
            selection_attempted = true;
            selection.activate(&job.release, &staged, previous)?;
            selected = true;
        }
        if job.reopen {
            store.advance(&mut job, Phase::Reopening)?;
            window_app::publish_locked(transaction.as_ref().expect("transaction"), window)?;
        }
        Ok(())
    }
    .await;
    let phase = match attempt {
        Ok(()) => Phase::Succeeded,
        Err(error) => {
            let mut rolled_back = false;
            let final_error = if changed {
                store.advance(&mut job, Phase::Restoring)?;
                let rollback: Result<()> = async {
                    let selection_error = if selected {
                        selection
                            .restore(
                                &job.release,
                                previous_selection.as_ref().expect("selected request"),
                            )
                            .err()
                    } else {
                        None
                    };
                    if !old_window {
                        private_files::remove_file(&paths.window_executable())?;
                    }
                    transaction
                        .as_ref()
                        .expect("changed in transaction")
                        .install(&store.backup().join("elsecall"), manager, None)?;
                    crate::installation::ready::confirm(
                        paths,
                        &old_digest.expect("backed up build").build_id(),
                        false,
                    )
                    .await?;
                    if let Some(error) = selection_error {
                        return Err(error);
                    }
                    Ok(())
                }
                .await;
                match rollback {
                    Ok(()) => {
                        rolled_back = true;
                        error
                    }
                    Err(rollback) => Failure::new(
                        "update_rollback_failed",
                        format!(
                            "The update failed ({}) and restoration failed ({}).",
                            error.code, rollback.code
                        ),
                        format!(
                            "The previous CLI and window are preserved in {}. Run the previous CLI's install command to recover.",
                            store.backup().display()
                        ),
                    ),
                }
            } else {
                error
            };
            Phase::Failed {
                error: final_error,
                rolled_back,
            }
        }
    };
    if let Some(stage) = stage_directory {
        let retained =
            selection_attempted && selection.references(&job.release, &stage).unwrap_or(true);
        if !retained {
            let _ = store.discard_stage(paths, id);
        }
    }
    store.advance(&mut job, phase)
}
fn check_stage(stage: &StagedRelease) -> Result<()> {
    if Digest::of_file(&stage.executable).ok() != Some(stage.digest)
        || Digest::of_file(&stage.window).ok() != Some(stage.window_digest)
    {
        return Err(invalid(
            "A verified release executable changed before installation.",
        ));
    }
    if stage.window != stage.executable.with_file_name("elsecall-ui") {
        return Err(invalid("The verified CLI and window must be companions."));
    }
    Ok(())
}
// Keep helper execution free of private diagnostics from external programs.
pub fn production_download(paths: &Paths) -> Result<Box<dyn ReleaseDownload>> {
    Ok(super::channel::Channel::this_build()?.download(paths))
}
pub fn production_selection(paths: &Paths) -> super::selection::MiseSelection {
    super::selection::MiseSelection {
        paths: paths.clone(),
    }
}
