//! The fleet window that belongs to this Elsecall build.
//!
//! `elsecall install` copies a sibling `elsecall-ui` next to the service
//! executable into the support directory. The window compares its own
//! executable with that file. When they differ, it asks this route to replace
//! `~/Applications/Elsecall.app` and open the new one.
use crate::{api, build_identity::Digest, failure::Failure, paths::Paths, private_files};
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Serialize;
use std::{
    fs,
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::Path,
    process::Command,
};

const INFO_PLIST: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleExecutable</key>
  <string>Elsecall</string>
  <key>CFBundleIdentifier</key>
  <string>com.elsecall.ui</string>
  <key>CFBundleName</key>
  <string>Elsecall</string>
  <key>CFBundleDisplayName</key>
  <string>Elsecall</string>
  <key>CFBundlePackageType</key>
  <string>APPL</string>
  <key>CFBundleShortVersionString</key>
  <string>0.1.0</string>
  <key>CFBundleVersion</key>
  <string>0.1.0</string>
  <key>LSMinimumSystemVersion</key>
  <string>14.0</string>
  <key>NSHighResolutionCapable</key>
  <true/>
</dict>
</plist>
"#;

#[derive(Serialize)]
struct Offer {
    build: Option<String>,
}

#[derive(Serialize)]
struct Launched {
    launched: bool,
}

pub fn routes(paths: Paths) -> Router {
    Router::new()
        .route("/v1/ui", get(offer))
        .route("/v1/ui/update", post(update))
        .with_state(paths)
}

async fn offer(State(paths): State<Paths>) -> Response {
    let found = tokio::task::spawn_blocking(move || installed_build(&paths))
        .await
        .expect("reading the window build does not panic");
    match found {
        Ok(build) => Json(api::Response::new(Offer {
            build: build.map(|id| id.to_string()),
        }))
        .into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(api::ErrorResponse::new(error)),
        )
            .into_response(),
    }
}

async fn update(State(paths): State<Paths>) -> Response {
    let launched = tokio::task::spawn_blocking(move || publish(&paths))
        .await
        .expect("replacing the window does not panic");
    match launched {
        Ok(()) => Json(api::Response::new(Launched { launched: true })).into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(api::ErrorResponse::new(error)),
        )
            .into_response(),
    }
}

/// `elsecall-ui` sitting next to the service executable that is being installed.
pub fn companion(source: &Path) -> std::path::PathBuf {
    source.with_file_name("elsecall-ui")
}

/// Copies the sibling window into the support directory when the install has one.
/// A missing sibling leaves any previously stored window in place.
pub fn store_companion(paths: &Paths, source: &Path) -> crate::failure::Result<()> {
    let sibling = companion(source);
    if !sibling.is_file() {
        return Ok(());
    }
    let bytes = fs::read(&sibling).map_err(|error| Failure::io("Read the fleet window", error))?;
    private_files::write(&paths.window_executable(), &bytes, 0o700)
}

pub fn installed_build(
    paths: &Paths,
) -> crate::failure::Result<Option<crate::build_identity::BuildId>> {
    let path = paths.window_executable();
    if !path.is_file() {
        return Ok(None);
    }
    let digest =
        Digest::of_file(&path).map_err(|error| Failure::io("Read the fleet window", error))?;
    Ok(Some(digest.build_id()))
}

/// Replaces the Finder app with the window stored for this install and opens it.
pub fn publish(paths: &Paths) -> crate::failure::Result<()> {
    let stored = paths.window_executable();
    if !stored.is_file() {
        return Err(Failure::new(
            "window_not_installed",
            "This Elsecall build has no fleet window to install.",
            "Run elsecall install from a build that includes elsecall-ui.",
        ));
    }
    let bytes = fs::read(&stored).map_err(|error| Failure::io("Read the fleet window", error))?;
    let app = paths.window_app();
    write_bundle(&app, &bytes)?;
    sign_and_open(&app)
}

pub(crate) fn write_bundle(app: &Path, binary: &[u8]) -> crate::failure::Result<()> {
    let macos = app.join("Contents/MacOS");
    fs::create_dir_all(&macos).map_err(|error| Failure::io("Create the Elsecall app", error))?;
    place(&macos.join("Elsecall"), binary, 0o755)?;
    place(
        app.join("Contents/Info.plist").as_path(),
        INFO_PLIST.as_bytes(),
        0o644,
    )
}

fn place(path: &Path, bytes: &[u8], mode: u32) -> crate::failure::Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(path)
        .map_err(|error| Failure::io("Write the Elsecall app", error))?;
    file.write_all(bytes)
        .map_err(|error| Failure::io("Write the Elsecall app", error))?;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(|error| Failure::io("Set Elsecall app permissions", error))
}

fn sign_and_open(app: &Path) -> crate::failure::Result<()> {
    let signed = Command::new("codesign")
        .args(["--force", "--sign", "-"])
        .arg(app)
        .status()
        .map_err(|error| Failure::io("Sign the Elsecall app", error))?;
    if !signed.success() {
        return Err(Failure::new(
            "window_sign_failed",
            "The fleet window could not be signed.",
            "Run codesign --force --sign - on Elsecall.app, then open it.",
        ));
    }
    let opened = Command::new("open")
        .arg(app)
        .status()
        .map_err(|error| Failure::io("Open the Elsecall app", error))?;
    if !opened.success() {
        return Err(Failure::new(
            "window_open_failed",
            "The fleet window was replaced but did not open.",
            "Open Elsecall from Applications.",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bundle_contains_the_binary_and_names_the_app() {
        let root = std::path::PathBuf::from(format!("/tmp/ec-ui-bundle-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let app = root.join("Elsecall.app");
        write_bundle(&app, b"window-bytes").unwrap();
        assert_eq!(
            fs::read(app.join("Contents/MacOS/Elsecall")).unwrap(),
            b"window-bytes"
        );
        let plist = fs::read_to_string(app.join("Contents/Info.plist")).unwrap();
        assert!(plist.contains("<string>Elsecall</string>"));
        assert!(plist.contains("com.elsecall.ui"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_sibling_window_is_stored_and_hashed() {
        let home = std::path::PathBuf::from(format!("/tmp/ec-ui-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&home).unwrap();
        let paths = Paths::for_home(&home).unwrap();
        fs::create_dir_all(&paths.data).unwrap();
        let source_dir = home.join("build");
        fs::create_dir_all(&source_dir).unwrap();
        let source = source_dir.join("elsecall");
        fs::write(&source, b"service").unwrap();
        store_companion(&paths, &source).unwrap();
        assert!(!paths.window_executable().exists());
        fs::write(source_dir.join("elsecall-ui"), b"window-bytes").unwrap();
        store_companion(&paths, &source).unwrap();
        assert_eq!(
            fs::read(paths.window_executable()).unwrap(),
            b"window-bytes"
        );
        let build = installed_build(&paths).unwrap().unwrap();
        assert_eq!(build, Digest::of(b"window-bytes").build_id());
        let _ = fs::remove_dir_all(&home);
    }
}
