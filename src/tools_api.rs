//! Tool inventories over HTTP.
//!
//! `GET /v1/tools/inventory` is this machine. `GET /v1/tools/fleet-inventory`
//! is every member. The fleet document is its own path, as `/v1/fleet` and
//! `/v1/fleet/builds` are, because it is a list of members rather than this
//! machine's managers. `?member=NAME` limits that list to one member.
//!
//! `GET /peer/v1/tools/inventory` returns this member's inventory to any
//! enrolled member. The listener's authentication is the same gate the report
//! routes use, including the refusal of a different build. Reading the managers
//! blocks, so it runs off the async worker, and only one such read is in flight.
use crate::{
    api,
    failure::Failure,
    fleet::Fleet,
    fleet_tool_inventory::{self, MemberInventories, OwnInventories, PeerInventories, Source},
    membership_store::Membership,
    peer_api,
    roster::{MemberName, Roster},
    tool_inventory::Inventory,
};
use axum::{
    Json, Router,
    extract::{Query, State},
    http::{HeaderValue, StatusCode, header},
    response::IntoResponse,
    routing::get,
};
use serde::Deserialize;
use std::sync::Arc;

pub(crate) const LOCAL_ROUTE: &str = "/v1/tools/inventory";
pub(crate) const FLEET_ROUTE: &str = "/v1/tools/fleet-inventory";

pub fn routes(fleet: Result<Arc<Fleet>, Failure>) -> Router {
    Router::new()
        .route(LOCAL_ROUTE, get(inventory))
        .route(FLEET_ROUTE, get(fleet_inventory))
        .with_state(fleet)
}

/// The member route, behind the member API's authentication.
pub fn peer_routes() -> Router<Arc<Fleet>> {
    Router::new().route(fleet_tool_inventory::PEER_ROUTE, get(peer_inventory))
}

async fn inventory() -> axum::response::Response {
    match installed().await {
        Ok(inventory) => Json(api::Response::new(inventory)).into_response(),
        Err(error) => error_response(StatusCode::INTERNAL_SERVER_ERROR, error),
    }
}

async fn peer_inventory() -> axum::response::Response {
    match installed().await {
        Ok(inventory) => match peer_document(&inventory) {
            Ok(bytes) => json_bytes(bytes),
            Err(error) => peer_api::failure(StatusCode::PAYLOAD_TOO_LARGE, error),
        },
        Err(error) => peer_api::failure(StatusCode::INTERNAL_SERVER_ERROR, error),
    }
}

fn peer_document(inventory: &Inventory) -> Result<Vec<u8>, Failure> {
    let bytes = serde_json::to_vec(&api::Response::new(inventory)).expect("inventory serializes");
    if bytes.len() > fleet_tool_inventory::PER_MEMBER_LIMIT {
        Err(Failure::new(
            "inventory_too_large",
            format!(
                "This member's tool inventory is larger than {} KiB.",
                fleet_tool_inventory::PER_MEMBER_LIMIT / 1024
            ),
            "Prune unused versions with mise, then retry.",
        ))
    } else {
        Ok(bytes)
    }
}

async fn installed() -> Result<Inventory, Failure> {
    crate::inventory_reader::installed().await.map_err(|_| {
        Failure::new(
            "tool_inventory_failed",
            "Reading this machine's tools failed.",
            "Run elsecall service restart, then retry elsecall tools inventory.",
        )
    })
}

#[derive(Deserialize)]
struct FleetQuery {
    #[serde(default)]
    member: Option<String>,
}

async fn fleet_inventory(
    State(fleet): State<Result<Arc<Fleet>, Failure>>,
    Query(query): Query<FleetQuery>,
) -> axum::response::Response {
    let fleet = match fleet {
        Ok(fleet) => fleet,
        Err(error) => return error_response(StatusCode::SERVICE_UNAVAILABLE, error),
    };
    let (names, sources) = match fleet.roster() {
        Ok(roster) => enrolled(&fleet, &roster),
        Err(_) if matches!(fleet.membership(), Membership::Solo) => solo(&fleet),
        Err(error) => return error_response(StatusCode::SERVICE_UNAVAILABLE, error),
    };
    if let Some(member) = query.member.as_deref()
        && !names.iter().any(|name| name.as_str() == member)
    {
        return error_response(StatusCode::BAD_REQUEST, unknown_member(member, &names));
    }
    let sources = sources
        .into_iter()
        .filter(|source| {
            query
                .member
                .as_deref()
                .is_none_or(|wanted| wanted == source.member.as_str())
        })
        .collect::<Vec<_>>();
    respond(fleet_tool_inventory::gather(&sources).await)
}

fn enrolled(fleet: &Arc<Fleet>, roster: &Roster) -> (Vec<MemberName>, Vec<Source>) {
    let this_member = roster
        .member_by_key(&fleet.public_key())
        .map(|member| member.name.clone());
    let names = fleet_tool_inventory::this_member_first(
        &roster
            .members()
            .iter()
            .map(|member| member.name.clone())
            .collect::<Vec<_>>(),
        this_member.as_ref(),
    );
    let sources = names
        .iter()
        .map(|name| {
            let member = roster
                .member(name)
                .expect("the names are this roster's members")
                .clone();
            let inventories: Arc<dyn MemberInventories> = if member.key == fleet.public_key() {
                Arc::new(OwnInventories)
            } else {
                Arc::new(PeerInventories {
                    fleet: Arc::clone(fleet),
                    member: member.clone(),
                })
            };
            Source {
                member: member.name,
                inventories,
            }
        })
        .collect();
    (names, sources)
}

/// A solo machine is a fleet of one even when `roster` cannot name it. The
/// name is the listener's presence when it has one, as `elsecall fleet` uses
/// that presence, and otherwise the host name a report uses for this machine.
fn solo(fleet: &Fleet) -> (Vec<MemberName>, Vec<Source>) {
    let name = fleet
        .presence()
        .map(|(name, _)| name)
        .unwrap_or_else(|| name_from_host(sysinfo::System::host_name().as_deref()));
    let sources = vec![Source {
        member: name.clone(),
        inventories: Arc::new(OwnInventories),
    }];
    (vec![name], sources)
}

pub(crate) fn name_from_host(host: Option<&str>) -> MemberName {
    let host = crate::scheduling::service::member_name(host);
    if let Ok(name) = host.parse() {
        return name;
    }
    let lowered = host.to_ascii_lowercase();
    lowered
        .parse()
        .unwrap_or_else(|_| "local".parse().expect("local is a member name"))
}

fn respond(inventory: fleet_tool_inventory::FleetInventory) -> axum::response::Response {
    let bytes =
        serde_json::to_vec(&api::Response::new(&inventory)).expect("fleet inventory serializes");
    if bytes.len() > fleet_tool_inventory::FLEET_RESPONSE_LIMIT {
        return error_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            Failure::new(
                "inventory_too_large",
                format!(
                    "The fleet's tool inventory is larger than {} MiB.",
                    fleet_tool_inventory::FLEET_RESPONSE_LIMIT / (1024 * 1024)
                ),
                "Prune unused versions with mise, then retry elsecall tools inventory --fleet.",
            ),
        );
    }
    json_bytes(bytes)
}

fn json_bytes(bytes: Vec<u8>) -> axum::response::Response {
    (
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        )],
        bytes,
    )
        .into_response()
}

fn unknown_member(member: &str, names: &[MemberName]) -> Failure {
    let listed = names
        .iter()
        .map(MemberName::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    Failure::new(
        "invalid_request",
        format!("--member {member:?} is not a member of this fleet; its members are {listed}."),
        "Run elsecall fleet to list members, then retry.",
    )
}

fn error_response(status: StatusCode, error: Failure) -> axum::response::Response {
    (status, Json(api::ErrorResponse::new(error))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_inventory::{
        Executable, InstalledTool, Manager, ManagerInventory, ManagerReport, ToolName, ToolVersion,
    };

    fn inventory_with(version: &str) -> Inventory {
        Inventory {
            managers: vec![
                ManagerReport::Read(
                    ManagerInventory::try_new(
                        Manager::Mise,
                        Executable::Missing,
                        vec![InstalledTool {
                            name: ToolName::new("node").unwrap(),
                            version: ToolVersion::new(version).unwrap(),
                            default: true,
                            detail: None,
                        }],
                    )
                    .unwrap(),
                ),
                ManagerReport::Read(
                    ManagerInventory::try_new(Manager::Xcodes, Executable::Missing, Vec::new())
                        .unwrap(),
                ),
            ],
        }
    }

    #[test]
    fn a_peer_document_over_the_member_limit_is_refused() {
        let small = peer_document(&inventory_with("22.11.0")).unwrap();
        assert!(small.len() < fleet_tool_inventory::PER_MEMBER_LIMIT);
        let huge = inventory_with(&"1".repeat(fleet_tool_inventory::PER_MEMBER_LIMIT));
        let error = peer_document(&huge).unwrap_err();
        assert_eq!(error.code, "inventory_too_large");
        assert!(error.hint.contains("mise"));
    }

    #[test]
    fn a_host_name_becomes_a_member_name() {
        assert_eq!(name_from_host(Some("macbook-pro")).as_str(), "macbook-pro");
        assert_eq!(
            name_from_host(Some("MacBook-Pro.local")).as_str(),
            "macbook-pro"
        );
        assert_eq!(name_from_host(Some("???")).as_str(), "local");
        assert_eq!(name_from_host(None).as_str(), "local");
    }
}
