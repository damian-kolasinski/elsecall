//! Version 1 same-user service preparation boundary. Fail closed until signed
//! installer acquisition and a receiver exist. Never display raw service data.
use crate::onboarding::{OnboardingPreparation, Prepared, Problem};
use serde::Deserialize;
use std::path::Path;

pub struct SocketPreparation;
impl OnboardingPreparation for SocketPreparation {
    fn prepare(&self) -> Result<Prepared, Problem> {
        let paths = elsecall::paths::Paths::current().map_err(|_| Problem::ServiceUnavailable)?;
        prepare_at(&paths.socket())
    }
}

fn prepare_at(socket: &Path) -> Result<Prepared, Problem> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| Problem::ServiceUnavailable)?;
    let operation = runtime
        .block_on(elsecall::client::post_within::<Operation>(
            socket,
            "/v1/onboarding/prepare",
            serde_json::json!({}),
            std::time::Duration::from_secs(3),
        ))
        .map_err(|failure| classify(&failure.code))?;
    validate(operation)
}

fn classify(code: &str) -> Problem {
    match code {
        "service_unavailable" | "service_timeout" => Problem::ServiceUnavailable,
        _ => Problem::UnsupportedResponse,
    }
}

#[derive(Deserialize)]
struct Operation {
    version: u32,
    id: String,
    expires_at: u64,
    stage: String,
    artifact: Option<serde_json::Value>,
    error: Option<OperationError>,
}
#[derive(Deserialize)]
struct OperationError {
    code: String,
}

fn validate(operation: Operation) -> Result<Prepared, Problem> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| Problem::UnsupportedResponse)?
        .as_secs();
    if operation.version != 1
        || operation.id.len() != 32
        || !operation
            .id
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        || operation.expires_at <= now
        || operation.artifact.is_some()
    {
        return Err(Problem::UnsupportedResponse);
    }
    match (
        operation.stage.as_str(),
        operation.error.map(|error| error.code),
    ) {
        ("blocked", Some(code)) if code == "onboarding_installer_unavailable" => {
            Err(Problem::InstallerUnavailable)
        }
        _ => Err(Problem::UnsupportedResponse),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        os::unix::{fs::PermissionsExt, net::UnixListener},
    };

    #[test]
    fn normal_io_failures_offer_service_recovery_instead_of_version_repair() {
        for code in ["service_unavailable", "service_timeout"] {
            assert_eq!(classify(code), Problem::ServiceUnavailable);
            assert!(classify(code).message().contains("elsecall doctor"));
        }
        assert_eq!(classify("protocol_error"), Problem::UnsupportedResponse);
    }

    #[test]
    fn real_socket_preparation_handles_blocker_and_rejects_artifacts_and_bad_contracts() {
        let valid = serde_json::json!({"version":1,"id":"0123456789abcdef0123456789abcdef","expires_at":u64::MAX,"stage":"blocked","artifact":null,"error":{"code":"onboarding_installer_unavailable","message":"unused","hint":"unused"}});
        let cases = [
            (valid.clone(), Problem::InstallerUnavailable),
            (
                {
                    let mut v = valid.clone();
                    v["artifact"] = serde_json::json!({"path":"/tmp/unverified.zip","format":"elsecall-onboarding-v1"});
                    v
                },
                Problem::UnsupportedResponse,
            ),
            (
                {
                    let mut v = valid.clone();
                    v["version"] = 2.into();
                    v
                },
                Problem::UnsupportedResponse,
            ),
            (
                {
                    let mut v = valid.clone();
                    v["expires_at"] = 0.into();
                    v
                },
                Problem::UnsupportedResponse,
            ),
            (
                {
                    let mut v = valid.clone();
                    v["id"] = "invalid".into();
                    v
                },
                Problem::UnsupportedResponse,
            ),
            (
                {
                    let mut v = valid.clone();
                    v["stage"] = "joined".into();
                    v
                },
                Problem::UnsupportedResponse,
            ),
            (
                {
                    let mut v = valid.clone();
                    v["error"] = serde_json::Value::Null;
                    v
                },
                Problem::UnsupportedResponse,
            ),
        ];
        for (operation, expected) in cases {
            let root = tempfile::Builder::new()
                .prefix("ec-add-mac-")
                .permissions(std::fs::Permissions::from_mode(0o700))
                .tempdir_in("/tmp")
                .unwrap();
            let socket = root.path().join("service.sock");
            let listener = UnixListener::bind(&socket).unwrap();
            let service = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                    .unwrap();
                let mut bytes = [0; 4096];
                let n = stream.read(&mut bytes).unwrap();
                assert!(
                    std::str::from_utf8(&bytes[..n])
                        .unwrap()
                        .starts_with("POST /v1/onboarding/prepare HTTP/1.1\r\n")
                );
                let body = serde_json::json!({"schema_version":1,"data":operation}).to_string();
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            });
            assert_eq!(prepare_at(&socket).err(), Some(expected));
            service.join().unwrap();
        }
    }
}
