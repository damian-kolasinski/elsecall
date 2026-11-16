use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use elsecall::{
    onboarding_local_api,
    onboarding_operation::{Operations, Stage},
};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

async fn send(router: &Router, method: &str, route: &str, body: &str) -> (StatusCode, Value) {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(route)
                .body(Body::from(body.to_owned()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    (
        status,
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap(),
    )
}

#[tokio::test]
async fn clean_prepare_restart_status_and_repeated_cancel_never_offer_an_artifact() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("onboarding");
    let router = onboarding_local_api::routes(path.clone());
    let (status, prepared) = send(&router, "POST", "/v1/onboarding/prepare", "{}").await;
    assert_eq!(status, StatusCode::OK);
    let data = &prepared["data"];
    assert_eq!(data["version"], 1);
    assert_eq!(data["stage"], "blocked");
    assert_eq!(data["error"]["code"], "onboarding_installer_unavailable");
    assert!(data["artifact"].is_null());
    let id = data["id"].as_str().unwrap();
    let route = format!("/v1/onboarding/{id}");
    drop(router);
    let router = onboarding_local_api::routes(path);
    assert_eq!(send(&router, "GET", &route, "").await.1, prepared);
    let cancelled = send(&router, "POST", &format!("{route}/cancel"), "{}")
        .await
        .1;
    assert_eq!(cancelled["data"]["stage"], "cancelled");
    assert!(cancelled["data"]["error"].is_null());
    assert_eq!(
        send(&router, "POST", &format!("{route}/cancel"), "{}")
            .await
            .1,
        cancelled
    );
    let second = send(&router, "POST", "/v1/onboarding/prepare", "{}")
        .await
        .1;
    assert_ne!(second["data"]["id"], data["id"]);
}

#[tokio::test]
async fn boundary_rejects_bad_ids_and_request_fields_without_echoing_them() {
    let dir = tempfile::tempdir().unwrap();
    let router = onboarding_local_api::routes(dir.path().join("onboarding"));
    let (status, error) = send(&router, "POST", "/v1/onboarding/prepare", &"x".repeat(1025)).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(error["error"]["code"], "onboarding_request_too_large");
    for body in ["", "null", "[]", "{\"credential\":\"do-not-echo\"}"] {
        let (status, error) = send(&router, "POST", "/v1/onboarding/prepare", body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(error["error"]["code"], "invalid_request");
        assert!(!error.to_string().contains("do-not-echo"));
    }
    assert_eq!(
        send(&router, "GET", "/v1/onboarding/not-an-id", "").await.0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        send(
            &router,
            "GET",
            "/v1/onboarding/00000000000000000000000000000000",
            ""
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
}

#[test]
fn expiration_and_existing_status_are_stable_and_unsafe_files_are_refused() {
    use std::{
        fs,
        os::unix::fs::{PermissionsExt, symlink},
    };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("onboarding");
    let operations = Operations::new(path.clone());
    let op = operations.prepare(100).unwrap();
    assert_eq!(
        operations
            .status(&op.id, op.expires_at, true)
            .unwrap()
            .stage,
        Stage::Expired
    );
    assert_eq!(
        operations.status(&op.id, 100, true).unwrap().stage,
        Stage::Expired
    );
    let file = path.join(format!("{}.json", op.id.as_str()));
    fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(operations.status(&op.id, 100, false).is_err());
    fs::remove_file(&file).unwrap();
    let target = dir.path().join("target");
    fs::write(&target, b"unrelated").unwrap();
    symlink(&target, &file).unwrap();
    assert!(operations.status(&op.id, 100, true).is_err());
    assert_eq!(fs::read(target).unwrap(), b"unrelated");
}
