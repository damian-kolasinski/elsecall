//! Removes Elsecall from this user's account: the installation, then the request
//! that makes the user's tool manager resolve the CLI, which the one-command
//! installer added and which would otherwise keep `elsecall` on the PATH.
use crate::{
    failure::{Failure, Result},
    installation::{self, Kept, ServiceManager},
    paths::Paths,
};
use serde::Serialize;

/// The user's tool manager, as far as its request for the Elsecall CLI goes.
pub trait CliRequest {
    /// Removes Elsecall's global request and the releases nothing else requests.
    /// Never [`Selection::Kept`], which only the operator asks for.
    fn forget(&self) -> Selection;
}

/// Whether to leave Elsecall's request in the tool manager's global configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request {
    Remove,
    Keep,
}

/// What happened to the tool manager's request for Elsecall.
#[derive(Debug, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Selection {
    /// No mise was found, so nothing could have requested the CLI through it.
    NoMise,
    /// mise holds no global request for Elsecall, or cannot resolve one, having
    /// no packslip backend.
    Absent,
    /// Removed from `config`, which requested `version`. `pruned` says whether the
    /// releases no other configuration requests were deleted too.
    Removed {
        config: std::path::PathBuf,
        version: String,
        pruned: bool,
    },
    /// Left as asked.
    Kept,
    /// Possibly still there: reading or removing it failed after the installation
    /// was gone.
    Failed { error: Failure },
}

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
    let kept = installation::uninstall(paths, manager, force)?;
    let mise = match request {
        Request::Keep => Selection::Kept,
        Request::Remove => cli.forget(),
    };
    Ok(Uninstalled {
        status: "uninstalled",
        mise,
        kept,
        master_ssh_key_error: crate::authorized_keys::remove_ours(&paths.home).err(),
    })
}
