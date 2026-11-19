//! Workspace copies on members, and the housekeeping that keeps their storage in
//! check.
pub mod carried_refs;
pub mod confined;
pub mod copies;
pub mod copies_api;
pub mod copies_cli;
mod copy;
mod copy_record;
pub mod digest;
mod file_clone;
pub mod fleet_callers;
pub mod fleet_copies;
pub mod framed_link;
pub mod git;
mod head;
pub mod housekeeping;
mod index;
pub mod location;
mod manifest;
pub mod not_returned;
pub mod object_sources;
pub mod output_return;
pub mod output_snapshot;
mod output_staging;
pub mod output_survey;
pub mod outputs;
pub mod path;
pub mod push_cache;
pub mod snapshot;
mod staging;
mod stat_cache;
pub mod sync;
pub mod workspace_check_api;

use elsecall_contract::failure::Failure;

/// The workspace could not be copied. The message never contains file content.
pub fn sync_failed(message: impl Into<String>) -> Failure {
    Failure::new(
        "workspace_sync_failed",
        message,
        "Check the working tree and the member's disk space, then retry. Elsecall changed nothing in the caller's working tree.",
    )
}
