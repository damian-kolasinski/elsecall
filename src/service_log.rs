//! Complete lifecycle records, reopened under a process-shared lock on every
//! write. No inherited descriptor can keep writing into a rotated archive.
use crate::{
    failure::{Failure, Result},
    private_files,
};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    pub max_bytes: u64,
    /// Archives in addition to the current file.
    pub keep: usize,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            max_bytes: 10 * 1024 * 1024,
            keep: 5,
        }
    }
}

pub struct Log {
    path: PathBuf,
    settings: Settings,
}
impl Log {
    pub fn new(path: PathBuf, settings: Settings) -> Self {
        Self { path, settings }
    }
    pub fn archive(path: &Path, number: usize) -> PathBuf {
        let mut name = path.as_os_str().to_owned();
        name.push(format!(".{number}"));
        name.into()
    }
    pub fn append(&self, line: &[u8]) -> std::io::Result<()> {
        let lock_path = self.path.with_extension("log.lock");
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(lock_path)?;
        let meta = lock.metadata()?;
        if !meta.is_file() || meta.uid() != crate::local_access::user_id() || meta.nlink() != 1 {
            return Err(std::io::Error::other("unsafe service log lock"));
        }
        lock.set_permissions(fs::Permissions::from_mode(0o600))?;
        lock.lock()?;
        // Open only real, owner-owned files, including archives left by older builds.
        let mut file = self.open()?;
        let length = file.metadata()?.len();
        if length > 0 && length.saturating_add(line.len() as u64) > self.settings.max_bytes {
            // Drop archives above a lowered count, without touching unrelated names.
            for entry in fs::read_dir(self.path.parent().expect("log parent"))? {
                let entry = entry?;
                if let Some(number) = entry
                    .file_name()
                    .to_str()
                    .and_then(|name| name.strip_prefix("service.log."))
                    .and_then(|n| n.parse::<usize>().ok())
                    && number > self.settings.keep
                {
                    fs::remove_file(entry.path())?;
                }
            }
            for number in (1..=self.settings.keep).rev() {
                let target = Self::archive(&self.path, number);
                let source = if number == 1 {
                    self.path.clone()
                } else {
                    Self::archive(&self.path, number - 1)
                };
                match fs::symlink_metadata(&source) {
                    Ok(meta) => {
                        if !meta.is_file()
                            || meta.uid() != crate::local_access::user_id()
                            || meta.nlink() != 1
                        {
                            return Err(std::io::Error::other("unsafe service log"));
                        }
                        fs::set_permissions(&source, fs::Permissions::from_mode(0o600))?;
                        fs::rename(source, target)?;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                    Err(e) => return Err(e),
                }
            }
            file = self.open()?;
        }
        // One complete record, including its newline, while holding the lock.
        file.write_all(line)
    }
    fn open(&self) -> std::io::Result<std::fs::File> {
        let file = OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&self.path)?;
        let meta = file.metadata()?;
        if !meta.is_file() || meta.uid() != crate::local_access::user_id() || meta.nlink() != 1 {
            return Err(std::io::Error::other("unsafe service log"));
        }
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        Ok(file)
    }
}

static LOG: OnceLock<Mutex<Log>> = OnceLock::new();
/// Called by the service and upgrade-helper entry points only, never CLI callers.
pub fn initialize(paths: &crate::paths::Paths) -> Result<()> {
    private_files::owned_tree(&paths.home, &paths.data)?;
    private_files::directory(&paths.data)?;
    // Defaults let invalid configuration itself leave a safe diagnostic.
    let configuration = crate::service_configuration::load(paths, None);
    let settings = configuration
        .as_ref()
        .map(|c| c.logs.clone())
        .unwrap_or_default();
    let log = Log::new(paths.log(), settings);
    log.append(b"")
        .map_err(|e| Failure::io("Open service log", e))?;
    let _ = LOG.set(Mutex::new(log));
    if let Err(error) = configuration {
        write(format_args!(
            "{}",
            serde_json::json!({"event":"service_failed", "code":error.code})
        ));
        return Err(error);
    }
    Ok(())
}
/// Applies the final accepted settings after any startup journal recovery.
pub fn configure(settings: Settings) {
    if let Some(log) = LOG.get() {
        log.lock().unwrap_or_else(|e| e.into_inner()).settings = settings;
    }
}

pub fn active() -> bool {
    LOG.get().is_some()
}
pub fn write(arguments: std::fmt::Arguments<'_>) {
    let mut line = arguments.to_string();
    line.push('\n');
    if let Some(log) = LOG.get() {
        if log
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .append(line.as_bytes())
            .is_err()
        {
            // Never recurse or include the record/error contents on failure.
            eprintln!("{{\"event\":\"service_log_write_failed\"}}");
        }
    } else {
        eprint!("{line}");
    }
}
#[macro_export]
macro_rules! service_log { ($($arg:tt)*) => { $crate::service_log::write(format_args!($($arg)*)) }; }

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rotates_whole_lines_keeps_count_and_restricts_existing_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("service.log");
        fs::write(&path, "old\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let log = Log::new(
            path.clone(),
            Settings {
                max_bytes: 8,
                keep: 2,
            },
        );
        for line in ["one\n", "two\n", "three\n", "four\n"] {
            log.append(line.as_bytes()).unwrap();
        }
        assert_eq!(fs::read_to_string(&path).unwrap(), "four\n");
        assert_eq!(
            fs::read_to_string(Log::archive(&path, 1)).unwrap(),
            "three\n"
        );
        assert_eq!(fs::read_to_string(Log::archive(&path, 2)).unwrap(), "two\n");
        for path in [&path, &Log::archive(&path, 1), &Log::archive(&path, 2)] {
            assert_eq!(fs::metadata(path).unwrap().mode() & 0o777, 0o600);
        }
        // An oversized record stays complete; the following one moves it intact.
        let oversized = b"a single line longer than the configured size\n";
        log.append(oversized).unwrap();
        assert_eq!(fs::read(&path).unwrap(), oversized);
        log.append(b"next\n").unwrap();
        assert_eq!(fs::read(Log::archive(&path, 1)).unwrap(), oversized);
        let lowered = Log::new(
            path.clone(),
            Settings {
                max_bytes: 8,
                keep: 1,
            },
        );
        lowered.append(b"again\n").unwrap();
        assert!(!Log::archive(&path, 2).exists());
    }
    #[test]
    fn refuses_links_without_touching_their_targets() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        fs::write(&target, "keep").unwrap();
        let path = dir.path().join("service.log");
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert!(
            Log::new(path.clone(), Settings::default())
                .append(b"no\n")
                .is_err()
        );
        fs::remove_file(&path).unwrap();
        fs::hard_link(&target, &path).unwrap();
        assert!(Log::new(path, Settings::default()).append(b"no\n").is_err());
        assert_eq!(fs::read_to_string(&target).unwrap(), "keep");
    }
}
