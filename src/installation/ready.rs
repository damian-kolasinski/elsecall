//! Whether a freshly installed service came up: it answers its local API with
//! the build that was installed, and keeps running as the same process.
use crate::{
    build_identity::{Build, BuildId},
    client,
    failure::{Failure, Result},
    inspection::Health,
    paths::Paths,
};
use std::time::{Duration, Instant};

/// How long a freshly installed service has to answer with its build.
const READY_WITHIN: Duration = Duration::from_secs(20);
/// How long a ready service must keep running, as the same process, to count.
const STABLE_FOR: Duration = Duration::from_secs(2);

/// Waits until the service answers with `expected`, and its fleet identity if
/// `with_fleet`, and keeps running for a moment.
pub async fn confirm(paths: &Paths, expected: &BuildId, with_fleet: bool) -> Result<Build> {
    let deadline = Instant::now() + READY_WITHIN;
    let socket = paths.socket();
    let health = || client::get::<Health>(&socket, "/v1/health");
    while Instant::now() < deadline {
        if let Ok(first) = health().await
            && first.build.as_ref() == Some(expected)
            && (!with_fleet || fleet_ready(paths).await)
        {
            tokio::time::sleep(STABLE_FOR).await;
            if let Ok(again) = health().await
                && again.pid == first.pid
                && again.build == first.build
            {
                return Ok(Build {
                    version: again.version,
                    build: expected.clone(),
                });
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    Err(Failure::new(
        "startup_failed",
        format!(
            "Build {expected} did not become ready (answering its local API and reaching its fleet identity) within {} seconds.",
            READY_WITHIN.as_secs()
        ),
        "Inspect the service log on this member for the build's startup error.",
    ))
}

/// Whether the service has its fleet identity and member listener.
pub async fn fleet_ready(paths: &Paths) -> bool {
    client::get::<serde_json::Value>(&paths.socket(), "/v1/fleet/identity")
        .await
        .is_ok()
}
