//! The helper that finishes an upgrade on a member. It runs as its own process,
//! started by the service in a new session, because installing stops the service
//! that started it. It installs the staged build through `installation::install`,
//! exactly as `elsecall install` does, and confirms the new service is ready. If it
//! is not, it installs the previous executable the same way and confirms that one,
//! so the member is not left without a working service. It records the outcome in
//! the upgrade journal, where the restarted service reports it to the master.
use crate::{
    build_identity::{Build, BuildId, Digest},
    client,
    failure::{Failure, Result},
    fleet_upgrade::journal::{Journal, Phase},
    fleet_upgrade::protocol::UpgradeId,
    inspection::Health,
    installation::{self, ServiceManager},
    paths::Paths,
};
use std::{
    path::Path,
    time::{Duration, Instant},
};

/// How long a freshly installed service has to answer with its build.
const READY_WITHIN: Duration = Duration::from_secs(20);
/// How long a ready service must keep running, as the same process, to count.
const STABLE_FOR: Duration = Duration::from_secs(2);

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
    let fleet_before = fleet_ready(paths).await;
    let incoming = journal.incoming();
    let staged = Digest::of_file(&incoming).ok() == Some(state.digest);
    let installed = if staged {
        installation::install(paths, &incoming, manager, None)
    } else {
        Err(Failure::new(
            "staged_build_changed",
            "The staged build no longer matches the digest the master signed; nothing was installed.",
            "Run elsecall fleet upgrade again.",
        ))
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
            match install_and_confirm(paths, manager, &journal.previous(), &state.from.build).await
            {
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
            let _ = crate::private_files::remove_file(&journal.incoming());
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

/// Whether the service has its fleet identity and member listener.
async fn fleet_ready(paths: &Paths) -> bool {
    client::get::<serde_json::Value>(&paths.socket(), "/v1/fleet/identity")
        .await
        .is_ok()
}

async fn install_and_confirm(
    paths: &Paths,
    manager: &dyn ServiceManager,
    executable: &Path,
    expected: &BuildId,
) -> Result<Build> {
    // Updates keep the installation's session, as `elsecall install` does.
    installation::install(paths, executable, manager, None)?;
    confirm(paths, expected, false).await
}

/// Waits until the service answers with `expected`, and its fleet identity if
/// `with_fleet`, and keeps running for a moment.
pub async fn confirm(paths: &Paths, expected: &BuildId, with_fleet: bool) -> Result<Build> {
    let deadline = Instant::now() + READY_WITHIN;
    let socket = paths.socket();
    let health = || client::get::<Health>(&socket, "/v1/health");
    while Instant::now() < deadline {
        if let Ok(first) = health().await
            && first.build.as_ref() == Some(expected)
            && (!with_fleet || fleet_ready(paths).await)
        {
            tokio::time::sleep(STABLE_FOR).await;
            if let Ok(again) = health().await
                && again.pid == first.pid
                && again.build == first.build
            {
                return Ok(Build {
                    version: again.version,
                    build: expected.clone(),
                });
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    Err(Failure::new(
        "startup_failed",
        format!(
            "Build {expected} did not become ready (answering its local API and reaching its fleet identity) within {} seconds.",
            READY_WITHIN.as_secs()
        ),
        "Inspect the service log on this member for the build's startup error.",
    ))
}
