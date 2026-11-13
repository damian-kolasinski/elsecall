//! Automatic service configuration application at an atomic idle boundary.
use crate::{
    config_sync::service::ConfigSync,
    failure::{Failure, Result},
    scheduling::service::Scheduler,
};
use std::{os::unix::process::CommandExt, sync::Arc, time::Duration};

pub async fn when_idle(sync: Option<Arc<ConfigSync>>, scheduler: Arc<Scheduler>) {
    let Some(sync) = sync else {
        return std::future::pending().await;
    };
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        if sync.service_pending() && sync.close_for_relaunch() {
            if scheduler.stop_if_idle() {
                return;
            }
            sync.resume_sync();
        }
    }
}

/// exec preserves the environment and foreground/launchd supervision, and closes
/// CLOEXEC listeners. No work remains at this point.
pub fn execute(service_lock: &std::fs::File) -> Result<()> {
    let executable =
        std::env::current_exe().map_err(|e| Failure::io("Locate service executable", e))?;
    let mut command = std::process::Command::new(executable);
    command.args(std::env::args_os().skip(1));
    // A concurrently forked child can briefly retain a CLOEXEC descriptor and
    // its shared flock after our exec. Explicit unlock releases that shared
    // lock too, so the replacement can always acquire it. Admission is closed
    // and all work/exchanges have drained before this handoff.
    service_lock
        .unlock()
        .map_err(|e| Failure::io("Release service lock for relaunch", e))?;
    let error = command.exec();
    Err(Failure::io("Relaunch service configuration", error))
}
