//! The user's tool manager, as far as its request for the Elsecall CLI goes:
//! uninstall asks it to forget the request, and a release update reads it.
use crate::failure::Failure;
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
