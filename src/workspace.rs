//! Workspace copies: the git working tree a placed run needs, reproduced on the member
//! that runs it.
//!
//! The caller takes a [`snapshot`] of its working tree: HEAD, the refs a copy
//! carries besides it ([`carried_refs`]), the index, and the
//! content of every tracked or untracked file git does not ignore. The member keeps a
//! persistent [`copy`] that [`sync`] brings to that snapshot, moving only what
//! changed. History moves as git packs, file content by digest. Every write on either
//! side goes through a [`confined`] directory, so no path escapes its workspace.
pub mod carried_refs;
pub mod confined;
pub mod copies;
pub mod copy;
pub mod copy_record;
pub mod digest;
pub mod file_clone;
pub mod framed_link;
pub mod git;
pub mod head;
pub mod index;
pub mod location;
pub mod manifest;
pub mod not_returned;
pub mod object_sources;
pub mod output_return;
pub mod output_snapshot;
pub mod output_staging;
pub mod output_survey;
pub mod outputs;
pub mod path;
pub mod push_cache;
pub mod snapshot;
pub mod staging;
pub mod stat_cache;
pub mod sync;

use crate::failure::Failure;

/// The workspace could not be copied. The message never contains file content.
pub fn sync_failed(message: impl Into<String>) -> Failure {
    Failure::new(
        "workspace_sync_failed",
        message,
        "Check the working tree and the member's disk space, then retry. Elsecall changed nothing in the caller's working tree.",
    )
}
