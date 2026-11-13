//! Crash remnants of atomic writes in service-owned metadata directories. Never
//! visit stored content, caller workspaces, or backup trees.
use crate::{
    failure::{Failure, Result},
    paths::Paths,
};
use std::{
    fs,
    time::{Duration, SystemTime},
};

pub fn prune(paths: &Paths) -> Result<()> {
    for (directory, targets) in [
        (paths.data.clone(), &["fleet.json"][..]),
        (paths.data.join("upgrade"), &[][..]),
        (
            paths.data.join("config-sync"),
            &["state.json", "history.jsonl"][..],
        ),
    ] {
        match fs::symlink_metadata(&directory) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(Failure::io("Inspect temporary state directory", e)),
            Ok(_) => crate::private_files::check_directory(&directory)?,
        }
        let entries = match fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(Failure::io("Inspect temporary state", e)),
        };
        for entry in entries {
            let entry = entry.map_err(|e| Failure::io("Inspect temporary state", e))?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let ours = name
                .strip_prefix(".write-")
                .is_some_and(|pid| pid.parse::<u32>().is_ok())
                || targets.iter().any(|target| {
                    name.strip_prefix(&format!("{target}."))
                        .and_then(|tail| tail.strip_suffix(".tmp"))
                        .is_some_and(|hex| {
                            hex.len() == 12 && hex.bytes().all(|b| b.is_ascii_hexdigit())
                        })
                });
            if !ours {
                continue;
            }
            let meta = fs::symlink_metadata(entry.path())
                .map_err(|e| Failure::io("Inspect temporary state", e))?;
            if meta.is_file()
                && meta
                    .modified()
                    .ok()
                    .and_then(|at| SystemTime::now().duration_since(at).ok())
                    .is_some_and(|age| age >= Duration::from_secs(7 * 86_400))
            {
                fs::remove_file(entry.path())
                    .map_err(|e| Failure::io("Remove temporary state", e))?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn refuses_symlinked_metadata_directory_without_touching_target() {
        let home = tempfile::tempdir_in("/tmp").unwrap();
        let paths = Paths::for_home(home.path()).unwrap();
        crate::private_files::directory(&paths.data).unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join(".write-123");
        let file = fs::File::create(&target).unwrap();
        file.set_times(
            fs::FileTimes::new().set_modified(SystemTime::now() - Duration::from_secs(8 * 86_400)),
        )
        .unwrap();
        std::os::unix::fs::symlink(outside.path(), paths.data.join("upgrade")).unwrap();
        assert!(prune(&paths).is_err());
        assert!(target.exists());
    }

    #[test]
    fn expires_only_recognized_old_metadata_scratch_without_visiting_secret_storage() {
        let home = tempfile::tempdir_in("/tmp").unwrap();
        let paths = Paths::for_home(home.path()).unwrap();
        crate::private_files::directory(&paths.data.join("config-sync/backups")).unwrap();
        let old = SystemTime::now() - Duration::from_secs(8 * 86_400);
        for name in [
            ".write-123",
            "fleet.json.012345abcdef.tmp",
            "unknown.tmp",
            "config-sync/backups/.write-123",
        ] {
            let file = fs::File::create(paths.data.join(name)).unwrap();
            file.set_times(fs::FileTimes::new().set_modified(old))
                .unwrap();
        }
        fs::write(paths.data.join(".write-456"), "active").unwrap();
        prune(&paths).unwrap();
        assert!(!paths.data.join(".write-123").exists());
        assert!(!paths.data.join("fleet.json.012345abcdef.tmp").exists());
        for name in [
            "unknown.tmp",
            "config-sync/backups/.write-123",
            ".write-456",
        ] {
            assert!(paths.data.join(name).exists(), "{name}");
        }
    }
}
