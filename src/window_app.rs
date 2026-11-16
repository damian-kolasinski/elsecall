//! Publishes a staged, signed app without modifying the running executable.
use crate::{
    failure::{Failure, Result},
    paths::Paths,
    private_files,
};
use std::{fs, io::Write, os::unix::fs::PermissionsExt, path::Path};

/// OS operations needed to prepare and launch the fleet window.
pub trait WindowLauncher: Send + Sync {
    fn sign(&self, app: &Path) -> Result<()>;
    /// Start a fresh instance even when the old window is still running, and
    /// return success only after its native window acknowledges readiness.
    fn open(&self, app: &Path) -> Result<()>;
}

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

/// Sign before publication; restore the previous bundle if opening fails.
pub fn publish(paths: &Paths, launcher: &dyn WindowLauncher) -> Result<()> {
    private_files::check_directory(&paths.data)?;
    let _lock =
        private_files::try_lock(&paths.data.join("installation.lock"))?.ok_or_else(|| {
            Failure::new(
                "operation_in_progress",
                "Another Elsecall installation or window update is running.",
                "Wait for it to finish, then retry.",
            )
        })?;
    let bytes = match fs::read(paths.window_executable()) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(Failure::new(
                "window_not_installed",
                "This Elsecall build has no fleet window to install.",
                "Run elsecall install from a release that includes elsecall-ui.",
            ));
        }
        Err(error) => return Err(Failure::io("Read the fleet window", error)),
    };
    let app = paths.window_app();
    let parent = app.parent().expect("app has a parent");
    private_files::owned_tree(&paths.home, parent)?;
    let exists = match fs::symlink_metadata(&app) {
        Ok(_) => {
            private_files::owned_tree(&paths.home, &app.join("Contents/MacOS"))?;
            let plist = fs::read(app.join("Contents/Info.plist"))
                .map_err(|error| Failure::io("Read installed app identity", error))?;
            let plist: plist::Value = plist::from_bytes(&plist).map_err(|error| {
                Failure::new(
                    "window_conflict",
                    format!("The existing app identity could not be read ({error})."),
                    "Inspect Elsecall.app before retrying.",
                )
            })?;
            if plist
                .as_dictionary()
                .and_then(|dict| dict.get("CFBundleIdentifier"))
                .and_then(plist::Value::as_string)
                != Some("com.elsecall.ui")
            {
                return Err(Failure::new(
                    "window_conflict",
                    "The existing Elsecall.app belongs to another application.",
                    "Move that application before retrying.",
                ));
            }
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(Failure::io("Inspect the Elsecall app", error)),
    };
    let staging = tempfile::Builder::new()
        .prefix(".elsecall-window-")
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir_in(parent)
        .map_err(|error| Failure::io("Stage the Elsecall app", error))?;
    let staged = staging.path().join("Elsecall.app");
    write_bundle(&staged, &bytes)?;
    launcher.sign(&staged)?;
    if exists {
        exchange(&staged, &app)?;
    } else {
        fs::rename(&staged, &app)
            .map_err(|error| Failure::io("Publish the Elsecall app", error))?;
    }
    if let Err(error) = launcher.open(&app) {
        let restored = if exists {
            exchange(&staged, &app)
        } else {
            fs::rename(&app, &staged)
                .map_err(|error| Failure::io("Restore the Elsecall app", error))
        };
        if let Err(restore_error) = restored {
            let recovery = staging.keep();
            return Err(Failure::new(
                "window_restore_failed",
                restore_error.message,
                format!(
                    "The previous app was preserved in {}. Restore it before retrying.",
                    recovery.display()
                ),
            ));
        }
        return Err(error);
    }
    Ok(())
}

fn write_bundle(app: &Path, binary: &[u8]) -> Result<()> {
    let macos = app.join("Contents/MacOS");
    fs::create_dir_all(&macos).map_err(|error| Failure::io("Create the Elsecall app", error))?;
    let executable = macos.join("Elsecall");
    fs::File::create(&executable)
        .and_then(|mut file| file.write_all(binary))
        .map_err(|error| Failure::io("Write the Elsecall app", error))?;
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o755))
        .map_err(|error| Failure::io("Set Elsecall app permissions", error))?;
    fs::write(
        app.join("Contents/Info.plist"),
        INFO_PLIST.replace("0.1.0", env!("CARGO_PKG_VERSION")),
    )
    .map_err(|error| Failure::io("Write the Elsecall app identity", error))?;
    let resources = app.join("Contents/Resources");
    fs::create_dir(&resources).map_err(|error| Failure::io("Create app resources", error))?;
    fs::write(
        resources.join("build-id"),
        crate::build_identity::Digest::of(binary)
            .build_id()
            .to_string(),
    )
    .map_err(|error| Failure::io("Write the window release identity", error))
}

#[cfg(target_os = "macos")]
fn exchange(left: &Path, right: &Path) -> Result<()> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    let left = CString::new(left.as_os_str().as_bytes()).expect("filesystem path has no NUL");
    let right = CString::new(right.as_os_str().as_bytes()).expect("filesystem path has no NUL");
    // Both bundles are on the same volume. The old mapped executable keeps its inode.
    if unsafe { libc::renamex_np(left.as_ptr(), right.as_ptr(), libc::RENAME_SWAP) } != 0 {
        return Err(Failure::io(
            "Replace the Elsecall app",
            std::io::Error::last_os_error(),
        ));
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn exchange(_: &Path, _: &Path) -> Result<()> {
    Err(Failure::new(
        "unsupported_platform",
        "The fleet window requires macOS.",
        "Run on macOS.",
    ))
}

/// App signing changes executable bytes. Read the original release identity,
/// captured at startup by the UI, from the signed bundle instead.
pub fn running_build(executable: &Path) -> Result<crate::build_identity::BuildId> {
    let parent = executable.parent().expect("executable has a parent");
    if executable
        .file_name()
        .is_some_and(|name| name == "Elsecall")
        && parent.file_name().is_some_and(|name| name == "MacOS")
    {
        let path = parent
            .parent()
            .expect("MacOS has a parent")
            .join("Resources/build-id");
        return fs::read_to_string(path)
            .map_err(|error| Failure::io("Read window release identity", error))?
            .parse();
    }
    crate::build_identity::Digest::of_file(executable)
        .map(|digest| digest.build_id())
        .map_err(|error| Failure::io("Read the running window", error))
}
