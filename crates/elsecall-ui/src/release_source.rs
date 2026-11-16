//! Release offers and install progress, read separately from slower fleet snapshots.
use elsecall::release_update::Status;
use rune::Latest;
use std::{path::PathBuf, sync::Arc, time::Duration};

pub struct ReleaseSnapshot {
    pub installed_window: Option<String>,
    pub release: Option<Status>,
}
pub trait ReleaseSource: Send + Sync {
    fn snapshot(&self) -> Result<ReleaseSnapshot, String>;
}
pub fn start(feed: Arc<Latest<Result<ReleaseSnapshot, String>>>, source: Arc<dyn ReleaseSource>) {
    std::thread::Builder::new()
        .name("elsecall-ui-updates".into())
        .spawn(move || {
            loop {
                let snapshot = source.snapshot();
                feed.request(snapshot, |snapshot| snapshot);
                std::thread::sleep(Duration::from_secs(2));
            }
        })
        .expect("start update polling");
}
pub struct SocketReleases {
    socket: PathBuf,
    runtime: tokio::runtime::Runtime,
}
impl SocketReleases {
    pub fn open(socket: PathBuf) -> Self {
        Self {
            socket,
            runtime: tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("update runtime"),
        }
    }
}
#[derive(serde::Deserialize)]
struct WindowOffer {
    build: Option<elsecall::build_identity::BuildId>,
}
impl ReleaseSource for SocketReleases {
    fn snapshot(&self) -> Result<ReleaseSnapshot, String> {
        let window = self
            .runtime
            .block_on(elsecall::client::get::<WindowOffer>(&self.socket, "/v1/ui"))
            .map_err(|error| error.message)?;
        // Older services still support staged-window updates without the release routes.
        let release = match self
            .runtime
            .block_on(elsecall::client::get::<Status>(&self.socket, "/v1/update"))
        {
            Ok(status) => Some(status),
            Err(error) if error.code == "not_found" => None,
            Err(error) => return Err(error.message),
        };
        Ok(ReleaseSnapshot {
            installed_window: window.build.map(|build| build.to_string()),
            release,
        })
    }
}
pub fn production() -> Arc<dyn ReleaseSource> {
    struct Lazy {
        source: std::sync::OnceLock<Result<SocketReleases, String>>,
    }
    impl ReleaseSource for Lazy {
        fn snapshot(&self) -> Result<ReleaseSnapshot, String> {
            match self.source.get_or_init(|| {
                elsecall::paths::Paths::current()
                    .map(|paths| SocketReleases::open(paths.socket()))
                    .map_err(|error| error.message)
            }) {
                Ok(source) => source.snapshot(),
                Err(error) => Err(error.clone()),
            }
        }
    }
    Arc::new(Lazy {
        source: std::sync::OnceLock::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        os::unix::{fs::PermissionsExt, net::UnixListener},
    };
    #[test]
    fn update_poll_reads_its_own_routes_and_supports_staged_updates_on_older_services() {
        for mode in ["current", "legacy", "future"] {
            let legacy = mode == "legacy";
            let future = mode == "future";
            let root = tempfile::Builder::new()
                .prefix("ec-release-read-")
                .permissions(std::fs::Permissions::from_mode(0o700))
                .tempdir_in("/tmp")
                .unwrap();
            let socket = root.path().join("service.sock");
            let listener = UnixListener::bind(&socket).unwrap();
            let service = std::thread::spawn(move || {
                for route in ["/v1/ui", "/v1/update"] {
                    let (mut stream, _) = listener.accept().unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    let mut header = Vec::new();
                    while !header.ends_with(b"\r\n\r\n") {
                        let mut byte = [0];
                        stream.read_exact(&mut byte).unwrap();
                        header.push(byte[0]);
                    }
                    assert!(
                        String::from_utf8(header)
                            .unwrap()
                            .starts_with(&format!("GET {route} HTTP/1.1\r\n"))
                    );
                    let (status, body) = if route == "/v1/ui" {
                        (
                            "200 OK",
                            serde_json::json!({"schema_version":1,"data":{"build":"1111111111111111"}}),
                        )
                    } else if legacy {
                        (
                            "404 Not Found",
                            serde_json::json!({"schema_version":1,"error":{"code":"not_found","message":"Unknown route.","hint":"Use an installed UI build."}}),
                        )
                    } else {
                        let job = if future {
                            serde_json::json!({"id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","release":{"version":"0.1.1"},"from_build":"1111111111111111","started_at_ms":1,"reopen":true,"wait_seconds":300,"state":"validating_new_release","new_field":true,"updated_at":1700000000})
                        } else {
                            serde_json::Value::Null
                        };
                        (
                            "200 OK",
                            serde_json::json!({"schema_version":1,"data":{"current_version":"0.1.0","available":{"version":"0.1.1"},"checked_at":1700000000,"check_error":null,"job":job}}),
                        )
                    };
                    let body = body.to_string();
                    write!(stream,"HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
                }
            });
            let snapshot = SocketReleases::open(socket).snapshot().unwrap();
            assert_eq!(
                snapshot.installed_window.as_deref(),
                Some("1111111111111111")
            );
            assert_eq!(snapshot.release.is_none(), legacy);
            if let Some(release) = snapshot.release {
                assert_eq!(release.available.unwrap().version.to_string(), "0.1.1");
                if future {
                    assert!(matches!(
                        release.job.unwrap().phase,
                        elsecall::release_update::Phase::Unknown
                    ));
                }
            }
            service.join().unwrap();
        }
    }
}
