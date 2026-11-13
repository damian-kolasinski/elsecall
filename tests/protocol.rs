#![cfg(target_os = "macos")]
use axum::{Json, Router, routing::get};
use elsecall::{
    client,
    failure::{Failure, Result},
    inspection::{Inspection, LoadSample, Machine, MachineInspector},
    macos::processes::MacProcesses,
    paths::Paths,
    peer_listener, private_files,
    scheduling::service::Adapters,
    server,
    tailnet::{Presence, Tailnet},
};
use std::{future::IntoFuture, sync::Arc};

struct UnavailableMachine;
impl MachineInspector for UnavailableMachine {
    fn inspect(&self) -> Result<Machine> {
        Err(Failure::new(
            "observation_unavailable",
            "The operating system could not be read.",
            "Retry later.",
        ))
    }
    fn sample_load(&self) -> Result<LoadSample> {
        Err(Failure::new(
            "observation_unavailable",
            "The operating system could not be read.",
            "Retry later.",
        ))
    }
}
struct NoTailnet;
impl Tailnet for NoTailnet {
    fn discover(&self) -> Result<Presence> {
        Err(elsecall::tailnet::unavailable("not connected"))
    }
}
fn fixture() -> (tempfile::TempDir, Paths) {
    let home = tempfile::Builder::new()
        .prefix("ec-")
        .tempdir_in("/tmp")
        .unwrap();
    let paths = Paths::for_home(home.path()).unwrap();
    private_files::directory(&paths.data).unwrap();
    (home, paths)
}

#[tokio::test]
async fn inspection_failure_crosses_real_api_without_becoming_false_health_failure() {
    let (_home, paths) = fixture();
    let machine = Arc::new(UnavailableMachine);
    let (processes, exits) = MacProcesses::start().unwrap();
    let task = tokio::spawn(server::serve(
        paths.clone(),
        Arc::new(Inspection::new(machine.clone())),
        Adapters {
            machine,
            speed: Arc::new(elsecall::machine_script::FixedSpeed(1.0)),
            processes,
            exits,
        },
        Arc::new(elsecall::macos::process_sampler::MacProcessSampler),
        peer_listener::Settings {
            tailnet: Arc::new(NoTailnet),
            port: 1,
        },
    ));
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(120);
    loop {
        if client::get::<elsecall::inspection::Health>(&paths.socket(), "/v1/health")
            .await
            .is_ok()
        {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let error = client::get::<Machine>(&paths.socket(), "/v1/machine")
        .await
        .unwrap_err();
    assert_eq!(error.code, "observation_unavailable");
    assert_eq!(error.hint, "Retry later.");
    let unknown = client::get::<serde_json::Value>(&paths.socket(), "/unknown")
        .await
        .unwrap_err();
    assert_eq!(unknown.code, "not_found");
    task.abort();
    let _ = task.await;
}

#[tokio::test]
async fn client_rejects_incompatible_malformed_oversized_and_stalled_responses() {
    let (_home, paths) = fixture();
    let listener = tokio::net::UnixListener::bind(paths.socket()).unwrap();
    let app = Router::new()
        .route(
            "/version",
            get(|| async { Json(serde_json::json!({"schema_version": 9000, "data": {}})) }),
        )
        .route("/malformed", get(|| async { "not json" }))
        .route("/oversized", get(|| async { "x".repeat(65537) }))
        .route(
            "/stalled",
            get(|| async { std::future::pending::<String>().await }),
        );
    let task = tokio::spawn(axum::serve(listener, app).into_future());
    for route in ["/version", "/malformed", "/oversized"] {
        assert_eq!(
            client::get::<serde_json::Value>(&paths.socket(), route)
                .await
                .unwrap_err()
                .code,
            "protocol_error"
        );
    }
    assert_eq!(
        client::get::<serde_json::Value>(&paths.socket(), "/stalled")
            .await
            .unwrap_err()
            .code,
        "service_timeout"
    );
    task.abort();
    let _ = task.await;
}
