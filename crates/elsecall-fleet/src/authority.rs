//! Who may make a member request that only the recorded master, or the member
//! it upgrades or syncs, may make. Handlers take these as extractors; the member
//! API has authenticated the request before any of them runs.
use crate::fleet::{
    AuthenticatedPeer, Fleet, PlacementAuthority, ProvisioningAuthority, SyncAuthority,
    UpgradeAuthority,
};
use axum::{
    extract::FromRequestParts,
    http::{StatusCode, request::Parts},
    response::Response,
};
use elsecall_api_response::failure;
use std::sync::Arc;

impl FromRequestParts<Arc<Fleet>> for SyncAuthority {
    type Rejection = Response;
    async fn from_request_parts(parts: &mut Parts, fleet: &Arc<Fleet>) -> Result<Self, Response> {
        // The member API authenticates every request before any handler runs.
        let peer = parts
            .extensions
            .get::<AuthenticatedPeer>()
            .expect("member requests are authenticated before extraction");
        fleet
            .authorize_sync(&peer.key)
            .map_err(|error| failure(StatusCode::FORBIDDEN, error))
    }
}

impl FromRequestParts<Arc<Fleet>> for UpgradeAuthority {
    type Rejection = Response;
    async fn from_request_parts(parts: &mut Parts, fleet: &Arc<Fleet>) -> Result<Self, Response> {
        // The member API authenticates every request before any handler runs.
        let peer = parts
            .extensions
            .get::<AuthenticatedPeer>()
            .expect("member requests are authenticated before extraction");
        fleet
            .authorize_upgrade(&peer.key)
            .map_err(|error| failure(StatusCode::FORBIDDEN, error))
    }
}

impl FromRequestParts<Arc<Fleet>> for PlacementAuthority {
    type Rejection = Response;
    async fn from_request_parts(parts: &mut Parts, fleet: &Arc<Fleet>) -> Result<Self, Response> {
        // The member API authenticates every request before any handler runs.
        let peer = parts
            .extensions
            .get::<AuthenticatedPeer>()
            .expect("member requests are authenticated before extraction");
        fleet
            .authorize_placement(&peer.key)
            .map_err(|error| failure(StatusCode::FORBIDDEN, error))
    }
}

impl FromRequestParts<Arc<Fleet>> for ProvisioningAuthority {
    type Rejection = Response;
    async fn from_request_parts(parts: &mut Parts, fleet: &Arc<Fleet>) -> Result<Self, Response> {
        // The member API authenticates every request before any handler runs.
        let peer = parts
            .extensions
            .get::<AuthenticatedPeer>()
            .expect("member requests are authenticated before extraction");
        fleet
            .authorize_provisioning(&peer.key)
            .map_err(|error| failure(StatusCode::FORBIDDEN, error))
    }
}
