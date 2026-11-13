//! Fleet-readable evidence; certificate writes remain on the master-authorized
//! rehearsal route. An unreachable member cannot silently certify fleet trust.
use crate::{api, failure::Result, fleet::Fleet, paths::Paths, peer_client};
use axum::{
    Extension, Json, Router,
    extract::State,
    response::{IntoResponse, Response},
    routing::post,
};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
pub async fn fleet_status(paths: &Paths, fleet: &Result<Arc<Fleet>>) -> Value {
    let mut members = Vec::new();
    let mut incomplete = false;
    match crate::tuning_trust::status(paths) {
        Ok(status)=>members.push(json!({"member":fleet.as_ref().ok().and_then(|f| f.roster().ok().map(|r|f.name_in(&r).to_string())),"evidence":status})),
        Err(error)=>{incomplete=true; members.push(json!({"error":error.code}));}
    }
    match fleet
        .as_ref()
        .map_err(Clone::clone)
        .and_then(|f| f.roster().map(|r| (f, r)))
    {
        Ok((f, roster)) => {
            let peers = roster
                .members()
                .iter()
                .filter(|m| m.key != f.public_key())
                .collect::<Vec<_>>();
            let answers = futures_util::future::join_all(peers.iter().map(|member| async move {
                let result: Result<Value> = peer_client::call(
                    f.member_key(),
                    member,
                    hyper::Method::POST,
                    "/peer/v1/tuning/trust",
                    Some(&json!({})),
                    Duration::from_secs(2),
                )
                .await;
                (member, result)
            }))
            .await;
            for (member, result) in answers {
                match result {
                    Ok(status) => members.push(json!({"member":member.name,"evidence":status})),
                    Err(error) => {
                        incomplete = true;
                        members.push(json!({"member":member.name,"error":error.code}));
                    }
                }
            }
        }
        Err(_) => incomplete = true,
    }
    let revoked = members.iter().any(|m| m["evidence"]["revoked"] == true);
    json!({"trusted_for_tuning":!incomplete && members.iter().all(|m|m["evidence"]["trusted_for_tuning"] == true),"revoked":revoked,"incomplete":incomplete,"members":members})
}
#[derive(Clone)]
struct Evidence {
    paths: Paths,
    fleet: Result<Arc<Fleet>>,
}
pub fn routes(paths: Paths, fleet: Result<Arc<Fleet>>) -> Router {
    Router::new()
        .route("/v1/tuning/trust", post(local))
        .with_state(Evidence { paths, fleet })
}
pub fn peer_routes(paths: Paths) -> Router<Arc<Fleet>> {
    Router::new()
        .route("/peer/v1/tuning/trust", post(peer))
        .layer(Extension(paths))
}
async fn local(State(e): State<Evidence>) -> Json<api::Response<Value>> {
    Json(api::Response::new(fleet_status(&e.paths, &e.fleet).await))
}
async fn peer(Extension(paths): Extension<Paths>) -> Response {
    match crate::tuning_trust::status(&paths) {
        Ok(status) => Json(api::Response::new(status)).into_response(),
        Err(error) => (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            Json(api::ErrorResponse::new(error)),
        )
            .into_response(),
    }
}
