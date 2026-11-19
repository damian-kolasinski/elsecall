//! Exercise real bundle publication with only macOS signing/launching replaced.
#![cfg(target_os = "macos")]
use elsecall_contract::build_identity::Digest;
use elsecall_contract::failure::{Failure, Result};
use elsecall_host::{paths::Paths, private_files};
use elsecall_installation::window_release;
use elsecall_window::window_app::{self, WindowLauncher};
use std::{fs, os::unix::fs::symlink, path::Path, sync::Mutex};

#[derive(Default)]
struct Desktop {
    fail_sign: bool,
    fail_open: bool,
    opened: Mutex<Vec<Vec<u8>>>,
}
impl WindowLauncher for Desktop {
    fn sign(&self, app: &Path) -> Result<()> {
        if self.fail_sign {
            return Err(failed("window_sign_failed"));
        }
        // Signing changes the Mach-O bytes in production too.
        let executable = app.join("Contents/MacOS/Elsecall");
        let mut bytes = fs::read(&executable).unwrap();
        bytes.extend_from_slice(b"-signed");
        fs::write(executable, bytes).unwrap();
        Ok(())
    }
    fn open(&self, app: &Path) -> Result<()> {
        self.opened
            .lock()
            .unwrap()
            .push(fs::read(app.join("Contents/MacOS/Elsecall")).unwrap());
        if self.fail_open {
            Err(failed("window_open_failed"))
        } else {
            Ok(())
        }
    }
}
fn failed(code: &str) -> Failure {
    Failure::new(code, "Window operation failed.", "Retry the upgrade.")
}
fn setup() -> (tempfile::TempDir, Paths) {
    let root = tempfile::Builder::new()
        .prefix("ec-window-")
        .tempdir_in("/tmp")
        .unwrap();
    let paths = Paths::for_home(root.path()).unwrap();
    private_files::owned_tree(&paths.home, &paths.data).unwrap();
    private_files::directory(&paths.data).unwrap();
    fs::write(paths.window_executable(), b"old").unwrap();
    (root, paths)
}
fn executable(paths: &Paths) -> std::path::PathBuf {
    paths.window_app().join("Contents/MacOS/Elsecall")
}

#[test]
fn installation_offers_the_companion_then_replaces_the_app_without_touching_its_old_inode() {
    let (root, paths) = setup();
    window_app::publish(&paths, &Desktop::default()).unwrap();
    let mut running = fs::File::open(executable(&paths)).unwrap();
    let old_build = window_app::running_build(&executable(&paths)).unwrap();
    assert_eq!(old_build, Digest::of(b"old").build_id());
    let release = root.path().join("release");
    fs::create_dir(&release).unwrap();
    fs::write(release.join("elsecall-ui"), b"new").unwrap();
    window_release::store_companion(&paths, &release.join("elsecall")).unwrap();
    let offer = window_release::installed_build(&paths).unwrap().unwrap();
    assert_ne!(old_build, offer);
    let desktop = Desktop::default();
    window_app::publish(&paths, &desktop).unwrap();
    assert_eq!(
        *desktop.opened.lock().unwrap(),
        vec![b"new-signed".to_vec()]
    );
    assert_eq!(
        window_app::running_build(&executable(&paths)).unwrap(),
        offer
    );
    use std::io::Read;
    let mut mapped = Vec::new();
    running.read_to_end(&mut mapped).unwrap();
    assert_eq!(mapped, b"old-signed");
    assert_eq!(
        fs::read_dir(paths.window_app().parent().unwrap())
            .unwrap()
            .count(),
        1
    );
}

#[test]
fn signing_or_launch_failure_preserves_the_old_app_and_the_offer() {
    for fail_sign in [true, false] {
        let (_root, paths) = setup();
        window_app::publish(&paths, &Desktop::default()).unwrap();
        fs::write(paths.window_executable(), b"new").unwrap();
        let desktop = Desktop {
            fail_sign,
            fail_open: !fail_sign,
            ..Desktop::default()
        };
        let error = window_app::publish(&paths, &desktop).unwrap_err();
        assert_eq!(
            error.code,
            if fail_sign {
                "window_sign_failed"
            } else {
                "window_open_failed"
            }
        );
        assert_eq!(fs::read(executable(&paths)).unwrap(), b"old-signed");
        assert_eq!(
            window_release::installed_build(&paths).unwrap(),
            Some(Digest::of(b"new").build_id())
        );
        window_app::publish(&paths, &Desktop::default()).unwrap();
        assert_eq!(fs::read(executable(&paths)).unwrap(), b"new-signed");
    }
}

#[test]
fn an_unavailable_desktop_on_first_open_leaves_no_broken_app() {
    let (_root, paths) = setup();
    let desktop = Desktop {
        fail_open: true,
        ..Desktop::default()
    };
    assert_eq!(
        window_app::publish(&paths, &desktop).unwrap_err().code,
        "window_open_failed"
    );
    assert!(!paths.window_app().exists());
}

#[test]
fn concurrent_lifecycle_operations_and_missing_companions_are_refused() {
    let (_root, paths) = setup();
    let lock = private_files::try_lock(&paths.data.join("installation.lock"))
        .unwrap()
        .unwrap();
    assert_eq!(
        window_app::publish(&paths, &Desktop::default())
            .unwrap_err()
            .code,
        "operation_in_progress"
    );
    drop(lock);
    fs::remove_file(paths.window_executable()).unwrap();
    assert_eq!(
        window_app::publish(&paths, &Desktop::default())
            .unwrap_err()
            .code,
        "window_not_installed"
    );
    assert!(!paths.window_app().exists());
}

#[test]
fn app_paths_cannot_follow_symlinks_or_replace_another_app() {
    let (_root, paths) = setup();
    let other = paths.home.join("other");
    fs::create_dir(&other).unwrap();
    fs::create_dir(paths.home.join("Applications")).unwrap();
    symlink(&other, paths.window_app()).unwrap();
    assert_eq!(
        window_app::publish(&paths, &Desktop::default())
            .unwrap_err()
            .code,
        "unsafe_directory"
    );
    assert_eq!(fs::read_dir(&other).unwrap().count(), 0);
    fs::remove_file(paths.window_app()).unwrap();
    window_app::publish(&paths, &Desktop::default()).unwrap();
    fs::write(paths.window_app().join("Contents/Info.plist"), b"<?xml version=\"1.0\"?><plist version=\"1.0\"><dict><key>CFBundleIdentifier</key><string>com.example.other</string></dict></plist>").unwrap();
    assert_eq!(
        window_app::publish(&paths, &Desktop::default())
            .unwrap_err()
            .code,
        "window_conflict"
    );
    assert_eq!(fs::read(executable(&paths)).unwrap(), b"old-signed");
}

#[test]
fn the_actual_macos_signer_keeps_the_release_identity_readable() {
    let (_root, paths) = setup();
    let binary = std::env::current_exe().unwrap();
    let original = fs::read(binary).unwrap();
    fs::write(paths.window_executable(), &original).unwrap();
    struct SignOnly;
    impl WindowLauncher for SignOnly {
        fn sign(&self, app: &Path) -> Result<()> {
            elsecall_platform_macos::macos::window_launcher::MacWindowLauncher.sign(app)
        }
        fn open(&self, _: &Path) -> Result<()> {
            Ok(())
        }
    }
    window_app::publish(&paths, &SignOnly).unwrap();
    assert_eq!(
        window_app::running_build(&executable(&paths)).unwrap(),
        Digest::of(&original).build_id()
    );
    assert!(
        std::process::Command::new("/usr/bin/codesign")
            .args(["--verify", "--deep", "--strict"])
            .arg(paths.window_app())
            .status()
            .unwrap()
            .success()
    );
}

#[tokio::test]
async fn the_local_routes_offer_the_staged_build_and_report_a_missing_window() {
    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let (_root, paths) = setup();
    let app = elsecall::ui_app::routes(paths.clone());
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/ui")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let json: serde_json::Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(
        json["data"]["build"],
        Digest::of(b"old").build_id().to_string()
    );
    fs::remove_file(paths.window_executable()).unwrap();
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/ui")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let json: serde_json::Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert!(json["data"]["build"].is_null());
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/ui/update")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 500);
    let json: serde_json::Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(json["error"]["code"], "window_not_installed");
    assert!(!paths.window_app().exists());
}
