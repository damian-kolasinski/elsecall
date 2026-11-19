//! Publishes a staged, signed app without modifying the running executable.
use crate::{
    failure::{Failure, Result},
    paths::Paths,
    private_files,
};
use std::{
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

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
  <key>CFBundleIconFile</key>
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
  <key>NSLocalNetworkUsageDescription</key>
  <string>Elsecall finds Macs on your network and connects to the one you add to your fleet.</string>
  <key>NSBonjourServices</key>
  <array>
    <string>_ssh._tcp</string>
    <string>_companion-link._tcp</string>
    <string>_device-info._tcp</string>
  </array>
</dict>
</plist>
"#;

/// The app's icon, `CFBundleIconFile` in `Contents/Resources`. Rendered from
/// `assets/icon/elsecall.svg` by `scripts/render-icon.sh`.
const ICON: &[u8] = include_bytes!("../assets/icon/Elsecall.icns");

/// Sign before publication; restore the previous bundle if opening fails.
pub fn publish(paths: &Paths, launcher: &dyn WindowLauncher) -> Result<()> {
    let transaction = crate::installation::Transaction::begin(paths)?;
    publish_locked(&transaction, launcher)
}

pub fn publish_locked(
    transaction: &crate::installation::Transaction,
    launcher: &dyn WindowLauncher,
) -> Result<()> {
    let placed = place_locked(transaction, launcher)?;
    if let Err(error) = launcher.open(placed.app()) {
        placed.restore()?;
        return Err(error);
    }
    Ok(())
}

/// A newly published app. The bundle it replaced stays staged until this is
/// dropped, which keeps the new app, or restored.
pub struct Placed {
    app: PathBuf,
    staging: tempfile::TempDir,
    replaced: bool,
}
impl Placed {
    pub fn app(&self) -> &Path {
        &self.app
    }

    /// Puts the replaced bundle back, or removes an app that replaced nothing.
    fn restore(self) -> Result<()> {
        let staged = self.staging.path().join("Elsecall.app");
        let restored = if self.replaced {
            exchange(&staged, &self.app)
        } else {
            fs::rename(&self.app, &staged)
                .map_err(|error| Failure::io("Restore the Elsecall app", error))
        };
        restored.map_err(|restore_error| {
            let recovery = self.staging.keep();
            Failure::new(
                "window_restore_failed",
                restore_error.message,
                format!(
                    "The previous app was preserved in {}. Restore it before retrying.",
                    recovery.display()
                ),
            )
        })
    }
}

/// Signs the stored window into a staged bundle and swaps it in for the app,
/// without opening it.
pub fn place_locked(
    transaction: &crate::installation::Transaction,
    launcher: &dyn WindowLauncher,
) -> Result<Placed> {
    let paths = transaction.paths();
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
            check_identity(&app)?;
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
    Ok(Placed {
        app,
        staging,
        replaced: exists,
    })
}

/// Refuses an app at the window's path that Elsecall did not publish.
fn check_identity(app: &Path) -> Result<()> {
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
    Ok(())
}

/// Removes the app this installation published, within the uninstall's lock.
///
/// Uninstall calls this after removing everything else, so the app never stops it:
/// anything that is not provably Elsecall's app in an owned, safe directory is
/// kept, and so is an app whose removal fails. A running window keeps its open
/// files and shows the service as unavailable.
pub fn remove_locked(paths: &Paths) {
    let app = paths.window_app();
    let parent = app.parent().expect("app has a parent");
    // The app exists, so the owned-tree check creates nothing; it refuses a
    // symlinked or shared directory anywhere below home.
    let removable = fs::symlink_metadata(&app).is_ok_and(|metadata| metadata.is_dir())
        && private_files::owned_tree(&paths.home, parent).is_ok()
        && check_identity(&app).is_ok();
    if !removable {
        return;
    }
    // Move the bundle aside first, so a failure part way leaves no half-deleted app.
    let Ok(staging) = tempfile::Builder::new()
        .prefix(".elsecall-window-")
        .tempdir_in(parent)
    else {
        return;
    };
    if fs::rename(&app, staging.path().join("Elsecall.app")).is_ok() {
        drop(staging);
    }
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
        INFO_PLIST.replace("0.1.0", crate::build_identity::VERSION),
    )
    .map_err(|error| Failure::io("Write the Elsecall app identity", error))?;
    let resources = app.join("Contents/Resources");
    fs::create_dir(&resources).map_err(|error| Failure::io("Create app resources", error))?;
    fs::write(resources.join("Elsecall.icns"), ICON)
        .map_err(|error| Failure::io("Write the Elsecall app icon", error))?;
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

#[cfg(test)]
mod tests {
    use super::ICON;

    /// The element types in an ICNS file, after its 8-byte header.
    fn icns_types(icns: &[u8]) -> Vec<String> {
        assert_eq!(&icns[..4], b"icns");
        assert_eq!(
            u32::from_be_bytes(icns[4..8].try_into().unwrap()) as usize,
            icns.len()
        );
        let mut types = Vec::new();
        let mut at = 8;
        while at < icns.len() {
            let length = u32::from_be_bytes(icns[at + 4..at + 8].try_into().unwrap()) as usize;
            assert!(length >= 8, "element at {at} has length {length}");
            types.push(String::from_utf8_lossy(&icns[at..at + 4]).into_owned());
            at += length;
        }
        assert_eq!(at, icns.len());
        types
    }

    #[test]
    fn the_icon_has_every_size_finder_and_the_dock_ask_for() {
        let mut types = icns_types(ICON);
        types.retain(|kind| kind != "info");
        types.sort();
        // 16, 32, 128, 256 and 512 points at 1x and 2x: 16 to 1024 pixels.
        assert_eq!(
            types,
            [
                "ic04", "ic05", "ic07", "ic08", "ic09", "ic10", "ic11", "ic12", "ic13", "ic14"
            ]
        );
    }
}
