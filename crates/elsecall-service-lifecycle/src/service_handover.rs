//! Requests to the local service that a handover to another build interrupted.
//!
//! An install, release update or fleet upgrade stops the service and starts the new
//! build in its place. Work it admitted keeps running and the new service adopts it,
//! but a request made meanwhile used to fail: refused with `update_in_progress`
//! while the update closed admission, or `service_unavailable` while the service was
//! stopped or starting. Such a request now waits for the handover and asks the new
//! service again, so scripts and agents never see the handover. Anything else, such
//! as a service that is simply not running, fails at once as before.
use elsecall_contract::failure::{Failure, Result};
use elsecall_contract::guidance;
use elsecall_installation::installation;
use std::{future::Future, path::Path, time::Duration};
use tokio::time::Instant;

/// The longest a request waits through one handover. A release update keeps
/// admission closed until it has confirmed the new service, selected the new CLI,
/// reopened the window and, on failure, restored and confirmed the previous build;
/// each of those steps has its own timeout, and together they stay well inside this.
const BOUND: Duration = Duration::from_secs(300);
/// The first pause before the service is asked again; it doubles up to
/// [`AGAIN_AT_MOST`], since each request for placeable work asks every member.
const AGAIN_AFTER: Duration = Duration::from_millis(250);
const AGAIN_AT_MOST: Duration = Duration::from_secs(2);
/// An attempt that lasted this long was taken by a running service, such as a
/// request that queued there, so a later interruption is another handover.
const SETTLED: Duration = Duration::from_secs(5);

/// Whether `failure` came from a handover in progress, given the service's state
/// directory `data`.
pub fn interrupted(failure: &Failure, data: &Path) -> bool {
    match failure.code.as_str() {
        // Only an update that closed admission for its install answers this.
        "update_in_progress" => true,
        "service_unavailable" => installation::operation_running(data),
        _ => false,
    }
}

/// Makes `attempt` until it is not interrupted by a handover, or until one
/// handover outlasts [`BOUND`]; then its failure stands. Says on stderr when it
/// starts waiting for a handover.
pub async fn through<T, F, A>(data: &Path, mut attempt: A) -> Result<T>
where
    A: FnMut() -> F,
    F: Future<Output = Result<T>>,
{
    let mut waiting_since: Option<Instant> = None;
    let mut pause = AGAIN_AFTER;
    loop {
        let started = Instant::now();
        match attempt().await {
            Err(failure) if interrupted(&failure, data) => {
                if started.elapsed() >= SETTLED {
                    waiting_since = None;
                    pause = AGAIN_AFTER;
                }
                let since = *waiting_since.get_or_insert_with(|| {
                    guidance::write(
                        "elsecall: the local service is switching to another build; the request is sent again once it is back.",
                    );
                    Instant::now()
                });
                if since.elapsed() >= BOUND {
                    return Err(failure);
                }
                tokio::time::sleep(pause).await;
                pause = (pause * 2).min(AGAIN_AT_MOST);
            }
            other => return other,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::service_handover::*;

    fn data() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn only_a_closed_update_or_an_unreachable_service_during_an_operation_is_a_handover() {
        let dir = data();
        let unavailable = Failure::unavailable();
        let updating = Failure::new("update_in_progress", "m", "h");
        let busy = Failure::new("fleet_busy", "m", "h");
        assert!(interrupted(&updating, dir.path()));
        assert!(!interrupted(&busy, dir.path()));
        assert!(
            !interrupted(&unavailable, dir.path()),
            "no lifecycle operation ever ran"
        );

        let mut child = std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let identity = elsecall_os_processes::processes::describe(child.id())
            .unwrap()
            .identity;
        std::fs::write(
            dir.path().join("installation.lock"),
            format!("{} {}", identity.pid, identity.started_us),
        )
        .unwrap();
        assert!(interrupted(&unavailable, dir.path()));
        assert!(!interrupted(&busy, dir.path()));
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(
            !interrupted(&unavailable, dir.path()),
            "the operation ended"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_interrupted_request_is_sent_again_until_it_is_answered_or_the_bound_passes() {
        let dir = data();
        let mut attempts = 0;
        let answered: Result<u32> = through(dir.path(), || {
            attempts += 1;
            let attempt = attempts;
            async move {
                if attempt < 3 {
                    Err(Failure::new("update_in_progress", "m", "h"))
                } else {
                    Ok(attempt)
                }
            }
        })
        .await;
        assert_eq!(answered.unwrap(), 3);

        let started = tokio::time::Instant::now();
        let given_up: Result<()> = through(dir.path(), || async {
            Err(Failure::new("update_in_progress", "m", "h"))
        })
        .await;
        assert_eq!(given_up.unwrap_err().code, "update_in_progress");
        assert!(started.elapsed() >= BOUND);

        let mut attempts = 0;
        let other: Result<()> = through(dir.path(), || {
            attempts += 1;
            async { Err(Failure::new("fleet_busy", "m", "h")) }
        })
        .await;
        assert_eq!(other.unwrap_err().code, "fleet_busy");
        assert_eq!(attempts, 1, "other failures are not retried");

        // A request the new service took, which queued there until a second
        // handover, gets the whole bound again for that one.
        let mut attempts = 0;
        let started = tokio::time::Instant::now();
        let twice: Result<u32> = through(dir.path(), || {
            attempts += 1;
            let attempt = attempts;
            async move {
                match attempt {
                    1 => tokio::time::sleep(BOUND - Duration::from_secs(1)).await,
                    2 => tokio::time::sleep(BOUND).await,
                    _ => return Ok(attempt),
                }
                Err(Failure::new("update_in_progress", "m", "h"))
            }
        })
        .await;
        assert_eq!(twice.unwrap(), 3);
        assert!(started.elapsed() > BOUND, "longer than one bound in all");
    }
}
