//! Failing on purpose, to rehearse an update's rollback against real builds.
//!
//! A build made with the `local-release-channel` feature and
//! `ELSECALL_LOCAL_RELEASE_FAULT=serve` refuses to start its service, so updating
//! to it makes the updater restore the previous release. Builds without the
//! feature always start.
use crate::failure::Result;

#[cfg(feature = "local-release-channel")]
const FAULT: Option<&str> = option_env!("ELSECALL_LOCAL_RELEASE_FAULT");
#[cfg(feature = "local-release-channel")]
const _: () = assert!(
    match FAULT {
        None => true,
        Some(fault) => matches!(fault.as_bytes(), b"serve"),
    },
    "ELSECALL_LOCAL_RELEASE_FAULT is either unset or `serve`."
);

/// Refuses to start a service built to fail at startup.
pub fn startup() -> Result<()> {
    #[cfg(feature = "local-release-channel")]
    if FAULT.is_some() {
        return Err(crate::failure::Failure::new(
            "local_release_rehearsed_failure",
            "This local release was built to fail at startup, to rehearse an update's rollback.",
            "Update to another local release; the updater restores the previous one on its own.",
        ));
    }
    Ok(())
}
