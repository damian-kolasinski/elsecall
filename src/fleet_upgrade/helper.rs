//! The helper that finishes an upgrade on a member. It runs as its own process,
//! started by the service in a new session, because installing stops the service
//! that started it. It installs the staged build through `installation::install`,
//! exactly as `elsecall install` does, and confirms the new service is ready. If it
//! is not, it installs the previous executable the same way and confirms that one,
//! so the member is not left without a working service. It records the outcome in
//! the upgrade journal, where the restarted service reports it to the master.
use crate::{
    client,
    fleet_upgrade::{
        journal::{Journal, Phase},
        protocol::UpgradeId,
    },
    inspection::Health,
    installation::{ServiceManager, Transaction, ready::confirm},
};
use elsecall_build_identity::build_identity::{Build, BuildId, Digest};
use elsecall_failure::{Failure, Result};
use elsecall_local_state::paths::Paths;
use std::path::Path;

fn log(event: &str, id: &UpgradeId, detail: serde_json::Value) {
    crate::service_log!(
        "{}",
        serde_json::json!({"event": event, "upgrade": id, "detail": detail})
    );
}

/// Installs the upgrade `id` staged in the journal and records how it ended.
pub async fn run(paths: &Paths, manager: &dyn ServiceManager, id: &UpgradeId) -> Result<Phase> {
    let journal = Journal::new(&paths.data);
    let Some(_lock) = journal.helper_lock()? else {
        return Err(Failure::new(
            "upgrade_in_progress",
            "Another upgrade helper is running.",
            "Wait for it to finish.",
        ));
    };
    let mut state = journal
        .read()?
        .filter(|state| &state.id == id && matches!(state.phase, Phase::Installing))
        .ok_or_else(|| {
            Failure::new(
                "upgrade_not_found",
                format!("No upgrade {id} is waiting to be installed."),
                "The service starts this helper itself; run elsecall fleet upgrade on the master.",
            )
        })?;
    let socket = paths.socket();
    let before = client::get::<Health>(&socket, "/v1/health").await.ok();
    // A new build must reach the fleet if the build it replaces did, or the master
    // could neither confirm it nor upgrade it again.
    let fleet_before = crate::installation::ready::fleet_ready(paths).await;
    let incoming = journal.incoming();
    let staged = Digest::of_file(&incoming).ok() == Some(state.digest);
    // Held until the outcome is confirmed, rollback included, so other lifecycle
    // operations stay out and requests meanwhile wait for the new service.
    let transaction = Transaction::begin(paths);
    let installed = match &transaction {
        Err(error) => Err(error.clone()),
        Ok(_) if !staged => Err(Failure::new(
            "staged_build_changed",
            "The staged build no longer matches the digest the master signed; nothing was installed.",
            "Run elsecall fleet upgrade again.",
        )),
        Ok(transaction) => transaction.install(&incoming, manager, None),
    };
    let attempt = match installed {
        // Refused before anything stopped, such as by a concurrent install: the
        // service that staged the build still runs, and nothing needs restoring.
        Err(error) if untouched(paths, before.as_ref(), &state.from.build).await => {
            let phase = Phase::Interrupted { error };
            journal.advance(&mut state, phase.clone())?;
            journal.discard_builds();
            log("upgrade_finished", id, serde_json::json!({"phase": phase}));
            return Ok(phase);
        }
        Err(error) => Err(error),
        Ok(_) => confirm(paths, &state.to, fleet_before).await,
    };
    let phase = match attempt {
        Ok(running) => {
            log(
                "upgrade_ready",
                id,
                serde_json::json!({"build": running.build}),
            );
            Phase::Upgraded { running }
        }
        Err(error) => {
            log(
                "upgrade_rolling_back",
                id,
                serde_json::json!({"code": error.code}),
            );
            let restoring = match transaction {
                Ok(transaction) => Ok(transaction),
                Err(_) => Transaction::begin(paths),
            };
            let restored = match &restoring {
                Ok(transaction) => {
                    install_and_confirm(
                        transaction,
                        manager,
                        &journal.previous(),
                        &state.from.build,
                    )
                    .await
                }
                Err(error) => Err(error.clone()),
            };
            match restored {
                Ok(running) => Phase::RolledBack { running, error },
                Err(rollback) => Phase::RollbackFailed {
                    error: Failure::new(
                        "rollback_failed",
                        format!(
                            "Build {} did not become ready ({}), and restoring build {} failed too ({}).",
                            state.to, error.code, state.from.build, rollback.code
                        ),
                        format!(
                            "On this member, run {} install to restore the previous build, then elsecall doctor.",
                            journal.previous().display()
                        ),
                    )
                    .with_details(serde_json::json!({"upgrade": error, "rollback": rollback})),
                },
            }
        }
    };
    journal.advance(&mut state, phase.clone())?;
    match phase {
        // Keep the previous build where the hint says, for the operator to restore.
        Phase::RollbackFailed { .. } => {
            let _ = elsecall_local_state::private_files::remove_file(&journal.incoming());
        }
        _ => journal.discard_builds(),
    }
    log("upgrade_finished", id, serde_json::json!({"phase": phase}));
    Ok(phase)
}

/// Whether a failed install changed nothing: the service that answered `before`
/// still answers as the same process, and the installed executable is still its
/// build. `install` writes the executable before it stops the service, so a failure
/// after that write (stopping the service failed) is not untouched and is rolled
/// back.
async fn untouched(paths: &Paths, before: Option<&Health>, running: &BuildId) -> bool {
    let Some(before) = before else {
        return false;
    };
    let installed = Digest::of_file(&paths.executable())
        .ok()
        .map(|d| d.build_id());
    installed.as_ref() == Some(running)
        && client::get::<Health>(&paths.socket(), "/v1/health")
            .await
            .is_ok_and(|now| now.pid == before.pid && now.build == before.build)
}

async fn install_and_confirm(
    transaction: &Transaction,
    manager: &dyn ServiceManager,
    executable: &Path,
    expected: &BuildId,
) -> Result<Build> {
    // Updates keep the installation's session, as `elsecall install` does.
    transaction.install(executable, manager, None)?;
    confirm(transaction.paths(), expected, false).await
}
