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
    let started: Value = match tokio::time::timeout(
        FLEET_TIMEOUT,
        client::post_large(&paths.socket(), apply_api::ROUTE, &ApplyBody { member }),
    )
    .await
    {
        Ok(result) => result?,
        Err(_) => return Err(timed_out(FLEET_TIMEOUT)),
    };
    if !wait {
        return Ok(started);
    }
    let names: Vec<String> = started["members"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|entry| entry["state"] == "started")
        .filter_map(|entry| entry["member"].as_str().map(str::to_owned))
        .collect();
    let mut finished = Vec::new();
    for name in names {
        let read = applied(paths, Some(&name), true).await?;
        finished.extend(read["members"].as_array().cloned().unwrap_or_default());
    }
    Ok(serde_json::json!({"master": started["master"], "members": finished}))
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
