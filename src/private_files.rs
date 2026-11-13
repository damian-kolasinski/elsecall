//! Filesystem ownership and exclusive locks for per-user state.
use crate::{
    failure::{Failure, Result},
    local_access::user_id,
};
use std::{
    fs::{self, File, OpenOptions},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::Path,
};

pub fn directory(path: &Path) -> Result<()> {
    if !path
        .try_exists()
        .map_err(|e| Failure::io("Inspect directory", e))?
    {
        fs::DirBuilder::new()
            .mode(0o700)
            .recursive(true)
            .create(path)
            .map_err(|e| Failure::io("Create directory", e))?;
    }
    check_directory(path)
}

pub fn check_directory(path: &Path) -> Result<()> {
    let meta =
        fs::symlink_metadata(path).map_err(|e| Failure::io("Inspect private directory", e))?;
    if !meta.is_dir() || meta.uid() != user_id() || meta.mode() & 0o077 != 0 {
        return Err(Failure::new(
            "unsafe_directory",
            "Elsecall's state directory must be a real directory owned by you with mode 0700.",
            format!(
                "Inspect {} and correct its ownership or permissions before retrying.",
                path.display()
            ),
        ));
    }
    Ok(())
}

/// Takes an exclusive lock without waiting. `None` means another process holds it.
pub fn try_lock(path: &Path) -> Result<Option<File>> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|e| Failure::io("Open lock", e))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(e)) => Err(Failure::io("Lock", e)),
    }
}

pub fn write(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    use std::io::Write;
    let parent = path.parent().expect("application file has parent");
    let temp = parent.join(format!(".write-{}", std::process::id()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&temp)
            .map_err(|e| Failure::io("Create temporary file", e))?;
        file.write_all(bytes)
            .and_then(|_| file.sync_all())
            .map_err(|e| Failure::io("Write application file", e))?;
        fs::set_permissions(&temp, fs::Permissions::from_mode(mode))
            .map_err(|e| Failure::io("Set file permissions", e))?;
        fs::rename(&temp, path).map_err(|e| Failure::io("Replace application file", e))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

/// Replaces `path` atomically with an owner-only file, under a temporary name no
/// concurrent writer shares.
pub fn replace(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut random = [0u8; 6];
    getrandom::fill(&mut random).map_err(|_| std::io::Error::other("no random numbers"))?;
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(format!(
        ".{}.tmp",
        random
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    ));
    let temporary = std::path::PathBuf::from(temporary);
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&temporary)?;
        file.write_all(bytes)?;
        fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

pub fn remove_file(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Failure::io("Remove application file", e)),
    }
}

/// Ensure all installation ancestors are owned by this user, with no symlinks or
/// write access for other users. Shared ancestors above HOME are not modified.
pub fn owned_tree(home: &Path, destination: &Path) -> Result<()> {
    let relative = destination.strip_prefix(home).map_err(|_| {
        Failure::new(
            "invalid_path",
            "Installation must stay within HOME.",
            "Use your user home directory.",
        )
    })?;
    let mut current = home.to_path_buf();
    check_owned_parent(&current)?;
    for part in relative.components() {
        if !matches!(part, std::path::Component::Normal(_)) {
            return Err(Failure::new(
                "invalid_path",
                "Installation paths must not contain parent traversal.",
                "Use a normalized HOME path.",
            ));
        }
        current.push(part);
        match fs::DirBuilder::new().mode(0o700).create(&current) {
            Ok(()) => (),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
            Err(e) => return Err(Failure::io("Create installation directory", e)),
        }
        check_owned_parent(&current)?;
    }
    Ok(())
}

fn check_owned_parent(path: &Path) -> Result<()> {
    let m =
        fs::symlink_metadata(path).map_err(|e| Failure::io("Inspect installation directory", e))?;
    if !m.is_dir() || m.uid() != user_id() || m.mode() & 0o022 != 0 {
        return Err(Failure::new(
            "unsafe_directory",
            format!(
                "{} must be a real directory owned by you and not writable by other users.",
                path.display()
            ),
            "Inspect directory ownership and permissions before retrying.",
        ));
    }
    Ok(())
}
