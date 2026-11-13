//! The replica store: synced content by keyed fingerprint. It is the storage
//! explicitly meant for secret values, so a member can rebuild the master's
//! content without its own overrides, and a master move loses nothing.
//!
//! `ReplicaStore` is a port so an external secret manager can stand in later;
//! `FileStore` keeps each object as an owner-only file in Elsecall's state.
use crate::{
    config_sync::{
        fingerprint::{Fingerprint, SyncKey},
        manifest::FileNode,
    },
    failure::{Failure, Result},
    private_files,
};
use std::{
    collections::BTreeSet,
    fs,
    io::{Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};
use zeroize::Zeroizing;

pub trait ReplicaStore: Send + Sync {
    fn contains(&self, fingerprint: &Fingerprint) -> bool;
    /// Stores `content` once it matches `expected` under `key`.
    fn put(&self, key: &SyncKey, expected: &FileNode, content: &[u8]) -> Result<()>;
    /// Reads the content of `expected`, verified against it under `key`. Content
    /// that no longer matches is removed, so the master sends it again.
    fn read(&self, key: &SyncKey, expected: &FileNode) -> Result<Zeroizing<Vec<u8>>>;
    /// Removes every object not in `keep`.
    fn retain(&self, keep: &BTreeSet<Fingerprint>) -> Result<()>;
}

pub struct FileStore {
    directory: PathBuf,
}

impl FileStore {
    /// Opens (creating, owner-only) the store at `directory`.
    pub fn open(directory: &Path) -> Result<Self> {
        private_files::directory(directory)?;
        Ok(Self {
            directory: directory.to_owned(),
        })
    }

    fn object(&self, fingerprint: &Fingerprint) -> PathBuf {
        self.directory.join(fingerprint.hex())
    }
}

pub fn mismatch() -> Failure {
    Failure::new(
        "sync_content_mismatch",
        "Synced content did not match its fingerprint.",
        "Run elsecall sync push on the master; the previous synced state stays in place.",
    )
}

impl ReplicaStore for FileStore {
    fn contains(&self, fingerprint: &Fingerprint) -> bool {
        fs::symlink_metadata(self.object(fingerprint)).is_ok_and(|m| m.is_file())
    }

    fn put(&self, key: &SyncKey, expected: &FileNode, content: &[u8]) -> Result<()> {
        if content.len() as u64 != expected.size || key.fingerprint(content) != expected.fingerprint
        {
            return Err(mismatch());
        }
        // Durable before it is renamed into place, so a crash never leaves a
        // truncated object under a valid name.
        let target = self.object(&expected.fingerprint);
        let temporary = self
            .directory
            .join(format!(".{}.tmp", expected.fingerprint.hex()));
        let written = (|| {
            let _ = fs::remove_file(&temporary);
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&temporary)?;
            file.write_all(content)?;
            file.sync_all()?;
            fs::rename(&temporary, &target)
        })();
        if written.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        written.map_err(|e| Failure::io("Store synced content", e))
    }

    fn read(&self, key: &SyncKey, expected: &FileNode) -> Result<Zeroizing<Vec<u8>>> {
        let object = self.object(&expected.fingerprint);
        let mut file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&object)
            .map_err(|e| Failure::io("Read synced content", e))?;
        let mut bytes = Zeroizing::new(Vec::new());
        file.read_to_end(&mut bytes)
            .map_err(|e| Failure::io("Read synced content", e))?;
        if bytes.len() as u64 != expected.size || key.fingerprint(&bytes) != expected.fingerprint {
            let _ = fs::remove_file(&object);
            return Err(mismatch());
        }
        Ok(bytes)
    }

    fn retain(&self, keep: &BTreeSet<Fingerprint>) -> Result<()> {
        let listing =
            fs::read_dir(&self.directory).map_err(|e| Failure::io("List synced content", e))?;
        for entry in listing.flatten() {
            let name = entry.file_name();
            let kept = name
                .to_str()
                .and_then(Fingerprint::parse)
                .is_some_and(|fingerprint| keep.contains(&fingerprint));
            if !kept {
                private_files::remove_file(&entry.path())?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn stores_verified_owner_only_objects() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileStore::open(&dir.path().join("objects")).unwrap();
        let key = SyncKey::generate().unwrap();
        let node = FileNode {
            mode: 0o600,
            size: 6,
            fingerprint: key.fingerprint(b"secret"),
        };
        assert_eq!(
            store.put(&key, &node, b"secreT").unwrap_err().code,
            "sync_content_mismatch"
        );
        assert!(!store.contains(&node.fingerprint));
        store.put(&key, &node, b"secret").unwrap();
        assert!(store.contains(&node.fingerprint));
        assert_eq!(&*store.read(&key, &node).unwrap(), b"secret");
        // Corrupted content is refused and dropped, so it is sent again.
        fs::write(store.object(&node.fingerprint), b"secreX").unwrap();
        assert_eq!(
            store.read(&key, &node).unwrap_err().code,
            "sync_content_mismatch"
        );
        assert!(!store.contains(&node.fingerprint));
        store.put(&key, &node, b"secret").unwrap();
        let mode = fs::metadata(store.object(&node.fingerprint))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        store.retain(&BTreeSet::new()).unwrap();
        assert!(!store.contains(&node.fingerprint));
    }
}
