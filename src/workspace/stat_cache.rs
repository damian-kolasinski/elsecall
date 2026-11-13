//! The caller's record of file digests by file status, so an unchanged file is
//! never read again. A file whose inode, size, modification or change time or mode
//! differs is hashed afresh.
use crate::workspace::{confined::Status, digest::Digest, path::WorkspacePath};
use std::{collections::HashMap, path::Path};

const VERSION: u8 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StatKey {
    pub inode: u64,
    pub size: u64,
    pub modified_ns: i64,
    pub changed_ns: i64,
    pub mode: u32,
}
impl From<&Status> for StatKey {
    fn from(status: &Status) -> Self {
        Self {
            inode: status.inode,
            size: status.size,
            modified_ns: status.modified_ns,
            changed_ns: status.changed_ns,
            mode: status.mode,
        }
    }
}

#[derive(Default)]
pub struct StatCache(HashMap<WorkspacePath, (StatKey, Digest)>);

impl StatCache {
    /// Reads the cache; a missing or unreadable one is empty.
    pub fn load(path: &Path) -> Self {
        std::fs::read(path)
            .ok()
            .and_then(|bytes| Self::decode(&bytes))
            .unwrap_or_default()
    }

    pub fn get(&self, path: &WorkspacePath, key: &StatKey) -> Option<Digest> {
        self.0
            .get(path)
            .filter(|(cached, _)| cached == key)
            .map(|(_, digest)| *digest)
    }

    pub fn replace(&mut self, entries: HashMap<WorkspacePath, (StatKey, Digest)>) {
        self.0 = entries;
    }

    /// Writes the cache privately and atomically. A cache that cannot be written
    /// only costs hashing next time.
    pub fn save(&self, path: &Path) {
        let _ = crate::private_files::replace(path, &self.encode());
    }

    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(8 + self.0.len() * 120);
        out.push(VERSION);
        out.extend_from_slice(&(self.0.len() as u32).to_be_bytes());
        for (path, (key, digest)) in &self.0 {
            out.extend_from_slice(&(path.as_str().len() as u16).to_be_bytes());
            out.extend_from_slice(path.as_str().as_bytes());
            for value in [
                key.inode,
                key.size,
                key.modified_ns as u64,
                key.changed_ns as u64,
                key.mode as u64,
            ] {
                out.extend_from_slice(&value.to_be_bytes());
            }
            out.extend_from_slice(&digest.0);
        }
        out
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        let mut rest = bytes;
        let mut take = |n: usize| -> Option<&[u8]> {
            if rest.len() < n {
                return None;
            }
            let (head, tail) = rest.split_at(n);
            rest = tail;
            Some(head)
        };
        if take(1)?[0] != VERSION {
            return None;
        }
        let count = u32::from_be_bytes(take(4)?.try_into().ok()?);
        let mut entries = HashMap::with_capacity(count.min(1 << 20) as usize);
        for _ in 0..count {
            let length = u16::from_be_bytes(take(2)?.try_into().ok()?) as usize;
            let path = WorkspacePath::parse(std::str::from_utf8(take(length)?).ok()?)?;
            let mut values = [0u64; 5];
            for value in &mut values {
                *value = u64::from_be_bytes(take(8)?.try_into().ok()?);
            }
            let digest = Digest(take(32)?.try_into().ok()?);
            let key = StatKey {
                inode: values[0],
                size: values[1],
                modified_ns: values[2] as i64,
                changed_ns: values[3] as i64,
                mode: values[4] as u32,
            };
            entries.insert(path, (key, digest));
        }
        Some(Self(entries))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_misses_on_any_change() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("cache");
        let path = WorkspacePath::parse("a/b").unwrap();
        let key = StatKey {
            inode: 1,
            size: 2,
            modified_ns: 3,
            changed_ns: 4,
            mode: 0o644,
        };
        let mut cache = StatCache::default();
        cache.replace(HashMap::from([(path.clone(), (key, Digest::of(b"x")))]));
        cache.save(&file);
        let loaded = StatCache::load(&file);
        assert_eq!(loaded.get(&path, &key), Some(Digest::of(b"x")));
        assert_eq!(loaded.get(&path, &StatKey { size: 3, ..key }), None);
        assert_eq!(StatCache::load(&dir.path().join("none")).0.len(), 0);
    }
}
