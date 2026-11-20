//! Delivering fleet configuration to the newly enrolled Mac and waiting for
//! the member's normal idle restart to apply managed service settings.

use elsecall_contract::failure::{Failure, Result};
use elsecall_fleet::add_mac::Outcome;
use elsecall_host::client;
use elsecall_sync::config_sync::service::PushRound;
use std::{path::Path, time::Duration};

const SYNC_WITHIN: Duration = Duration::from_secs(120);
const RETRY_AFTER: Duration = Duration::from_secs(1);

pub(super) async fn synchronize(socket: &Path, member: &str) -> Result<Outcome> {
    let body = serde_json::json!({"member": member});
    let push = || async {
        let round: PushRound = client::post_large(socket, "/v1/sync/push", &body).await?;
        // A push naming one member returns an error for a failed delivery.
        Ok(!round.deliveries.is_empty())
    };
    match tokio::time::timeout(SYNC_WITHIN, wait_for_configuration(push)).await {
        Ok(Ok(outcome)) => Ok(outcome),
        Ok(Err(failure)) => Err(Failure::new(
            "member_configuration_sync_failed",
            format!("{member} joined the fleet. {}", failure.message),
            failure.hint,
        )),
        Err(_) => Err(Failure::new(
            "member_configuration_sync_failed",
            format!("{member} joined the fleet, but configuration did not finish syncing in time."),
            "Keep the Mac awake and connected, then try again.",
        )),
    }
}

async fn wait_for_configuration(mut push: impl AsyncFnMut() -> Result<bool>) -> Result<Outcome> {
    let mut restarting = false;
    loop {
        match push().await {
            Ok(delivered) => {
                return Ok(if delivered || restarting {
                    Outcome::Done
                } else {
                    Outcome::Already
                });
            }
            Err(failure) if failure.code == "service_configuration_pending" => {
                restarting = true;
            }
            Err(failure) if restarting && failure.code == "member_unavailable" => {}
            Err(failure) => return Err(failure),
        }
        tokio::time::sleep(RETRY_AFTER).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn waits_for_the_member_to_restart_and_accept_its_configuration() {
        let unavailable = || Failure::new("member_unavailable", "unavailable", "check the Mac");
        let mut replies = [
            Err(Failure::new(
                "service_configuration_pending",
                "pending",
                "retry",
            )),
            Err(unavailable()),
            Ok(false),
        ]
        .into_iter();
        let sent = wait_for_configuration(async || replies.next().unwrap())
            .await
            .unwrap();
        assert_eq!(sent, Outcome::Done);
        assert_eq!(
            wait_for_configuration(async || Err(unavailable()))
                .await
                .unwrap_err()
                .code,
            "member_unavailable",
        );
    }
}
