//! macOS signing and LaunchServices adapter for window publication.
use crate::window_app::WindowLauncher;
use elsecall_failure::{Failure, Result};
use std::{path::Path, process::Command};

pub struct MacWindowLauncher;

impl WindowLauncher for MacWindowLauncher {
    fn sign(&self, app: &Path) -> Result<()> {
        let signed = Command::new("/usr/bin/codesign")
            .args(["--force", "--sign", "-"])
            .arg(app)
            .output()
            .map_err(|error| Failure::io("Sign the Elsecall app", error))?;
        if !signed.status.success() {
            return Err(Failure::new(
                "window_sign_failed",
                "The fleet window could not be signed.",
                "Retry the window upgrade.",
            ));
        }
        Ok(())
    }

    fn open(&self, app: &Path) -> Result<()> {
        let startup = crate::window_startup::WindowStartup::new()?;
        // Without -n, LaunchServices activates the old instance, which is about to exit.
        let opened = Command::new("/usr/bin/open")
            .arg("-n")
            .arg(app)
            .arg("--args")
            .arg(crate::window_startup::READY_ARGUMENT)
            .arg(startup.socket())
            .output()
            .map_err(|error| Failure::io("Open the Elsecall app", error))?;
        if !opened.status.success() {
            return Err(Failure::new(
                "window_open_failed",
                "The fleet window could not be opened.",
                "Retry from a macOS desktop session.",
            ));
        }
        startup.wait(std::time::Duration::from_secs(15))
    }
}
