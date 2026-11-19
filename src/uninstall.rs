//! Removes Elsecall from this user's account: the installation, then the request
//! that makes the user's tool manager resolve the CLI, which the one-command
//! installer added and which would otherwise keep `elsecall` on the PATH.
use elsecall_failure::{Failure, Result};
use elsecall_installation::installation::{
    self, InstalledParts, Kept, ServiceManager,
    cli_request::{CliRequest, Request, Selection},
};
use elsecall_local_state::paths::Paths;
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct Uninstalled {
    pub status: &'static str,
    pub mise: Selection,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kept: Option<Kept>,
    /// Why the master's SSH key that Add Mac authorized could not be removed from
    /// `~/.ssh/authorized_keys`; absent when it was removed or never there.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub master_ssh_key_error: Option<Failure>,
}

impl Uninstalled {
    /// Whether nothing Elsecall meant to remove is still there.
    pub fn complete(&self) -> bool {
        !matches!(self.mise, Selection::Failed { .. }) && self.master_ssh_key_error.is_none()
    }
}

/// Removes the installation first: a refused uninstall, such as one of an enrolled
/// member without `force`, leaves the CLI's request alone.
pub fn uninstall(
    paths: &Paths,
    manager: &dyn ServiceManager,
    cli: &dyn CliRequest,
    force: bool,
    request: Request,
) -> Result<Uninstalled> {
    let kept = installation::uninstall(paths, manager, &InstalledSlices, force)?;
    let mise = match request {
        Request::Keep => Selection::Kept,
        Request::Remove => cli.forget(),
    };
    Ok(Uninstalled {
        status: "uninstalled",
        mise,
        kept,
        master_ssh_key_error: elsecall_add_mac::authorized_keys::remove_ours(&paths.home).err(),
    })
}

/// The update, upgrade and window slices' state under the installation.
pub struct InstalledSlices;
impl InstalledParts for InstalledSlices {
    fn helper_active(&self, paths: &Paths) -> Result<bool> {
        elsecall_release_update::release_update::store::active_helper(paths)
    }
    fn remove(&self, paths: &Paths) -> Result<()> {
        crate::fleet_upgrade::journal::Journal::new(&paths.data).remove()?;
        elsecall_release_update::release_update::store::remove(paths)?;
        elsecall_window::window_app::remove_locked(paths);
        Ok(())
    }
}
