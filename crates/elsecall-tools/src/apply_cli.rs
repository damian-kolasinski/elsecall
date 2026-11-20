//! `elsecall tools apply` and `elsecall tools applied`, through the local service.
//!
//! A dry run is `tools plan` itself. `--wait` reads each started member's apply
//! every two seconds until none still runs; a failed step is data, in the
//! output and in the fleet's attention, so the command still exits 0.
use crate::{
    apply_api::{self, ApplyBody},
    tools_cli::{FLEET_TIMEOUT, escaped},
};
use elsecall_contract::failure::{Failure, Result};
use elsecall_host::{client, paths::Paths};
use hyper::Method;
use serde_json::Value;
use std::time::Duration;

/// How often `--wait` reads the members' applies again.
const POLL: Duration = Duration::from_secs(2);
/// Each read asks every member once, for at most 20 seconds.
const READ_TIMEOUT: Duration = Duration::from_secs(30);
/// Planning waits up to 150 seconds for a member's inventory, then starting an
/// apply up to 20 seconds; this waits for both, with a margin.
const APPLY_TIMEOUT: Duration = Duration::from_secs(200);

pub(crate) async fn apply(
    paths: &Paths,
    member: Option<String>,
    dry_run: bool,
    wait: bool,
) -> Result<Value> {
    if dry_run {
        let route = match &member {
            Some(name) => format!("{}?member={}", crate::plan_api::ROUTE, escaped(name)),
            None => crate::plan_api::ROUTE.to_owned(),
        };
        return client::send_bounded(
            &paths.socket(),
            Method::GET,
            &route,
            FLEET_TIMEOUT,
            crate::fleet_tool_inventory::FLEET_RESPONSE_LIMIT,
        )
        .await;
    }
    let mut started: Value = match tokio::time::timeout(
        APPLY_TIMEOUT,
        client::post_large(&paths.socket(), apply_api::ROUTE, &ApplyBody { member }),
    )
    .await
    {
        Ok(result) => result?,
        Err(_) => return Err(timed_out(APPLY_TIMEOUT)),
    };
    if !wait {
        return Ok(started);
    }
    // Each started member's entry becomes its finished apply; the others stay
    // as they were, refusals included.
    if let Some(members) = started["members"].as_array_mut() {
        for entry in members.iter_mut() {
            if entry["state"] != "started" {
                continue;
            }
            let Some(name) = entry["member"].as_str().map(str::to_owned) else {
                continue;
            };
            let read = applied(paths, Some(&name), true).await?;
            let mut finished = read["members"][0].clone();
            finished["unresolved"] = entry["unresolved"].clone();
            *entry = finished;
        }
    }
    Ok(started)
}

pub(crate) async fn applied(paths: &Paths, member: Option<&str>, wait: bool) -> Result<Value> {
    let route = match member {
        Some(name) => format!("{}?member={}", apply_api::ROUTE, escaped(name)),
        None => apply_api::ROUTE.to_owned(),
    };
    loop {
        let read: Value = client::send_bounded(
            &paths.socket(),
            Method::GET,
            &route,
            READ_TIMEOUT,
            crate::fleet_tool_inventory::FLEET_RESPONSE_LIMIT,
        )
        .await?;
        let running = read["members"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|entry| entry["state"] == "running");
        if !wait || !running {
            return Ok(read);
        }
        tokio::time::sleep(POLL).await;
    }
}

fn timed_out(timeout: Duration) -> Failure {
    Failure::new(
        "service_timeout",
        format!(
            "The local service did not respond within {} seconds.",
            timeout.as_secs()
        ),
        "Run elsecall doctor, then elsecall service restart.",
    )
}
