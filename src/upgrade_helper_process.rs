//! The production `Installer` for upgrades: this executable's hidden
//! `upgrade-helper` command, started in its own session (`setsid`) so it outlives
//! the service it stops, launchd's `bootout` included, with its output in the
//! service log.
use crate::{
    failure::{Failure, Result},
    fleet_upgrade::{member::Installer, protocol::UpgradeId},
};
use std::os::unix::process::CommandExt;

pub struct HelperProcess;

impl Installer for HelperProcess {
    fn start(&self, id: &UpgradeId) -> Result<tokio::sync::oneshot::Receiver<()>> {
        let executable =
            std::env::current_exe().map_err(|e| Failure::io("Find this executable", e))?;
        let mut command = std::process::Command::new(executable);
        command
            .args(["upgrade-helper", "--id", id.as_str()])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        // SAFETY: setsid is async-signal-safe and touches no memory of this process.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = crate::child_start::spawn(&mut command)
            .map_err(|e| Failure::io("Start the upgrade helper", e))?;
        let (ended, receiver) = tokio::sync::oneshot::channel();
        // Reap it, and say so, if it ends while this service still runs.
        std::thread::spawn(move || {
            let _ = child.wait();
            let _ = ended.send(());
        });
        Ok(receiver)
    }
}
