//! What a working tree contains, as far as a copy must match it: every tracked or
//! untracked non-ignored file and symlink, with its permissions and content digest.
//! Ignored files are never listed, so a copy's ignored build state is never touched.
use crate::workspace::{digest::Digest, path::WorkspacePath};
use std::collections::BTreeMap;

/// Permission bits a copy reproduces; set-id and sticky bits are never copied.
pub const PERMISSIONS: u32 = 0o777;
const MAXIMUM_ENTRIES: u32 = 4_000_000;
const MAXIMUM_TARGET: usize = 4096;
const VERSION: u8 = 1;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Entry {
    File {
        /// Permission bits, within [`PERMISSIONS`].
        mode: u32,
        size: u64,
        digest: Digest,
    },
    Symlink {
        /// The link's target exactly as stored; it is never followed.
        target: String,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Manifest(BTreeMap<WorkspacePath, Entry>);

/// The changes that turn one manifest into another.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Delta {
    pub set: Vec<(WorkspacePath, Entry)>,
    pub remove: Vec<WorkspacePath>,
}
impl Delta {
    pub fn is_empty(&self) -> bool {
        self.set.is_empty() && self.remove.is_empty()
    }
}

impl Manifest {
    pub fn new(entries: BTreeMap<WorkspacePath, Entry>) -> Self {
        Self(entries)
    }
    pub fn entries(&self) -> &BTreeMap<WorkspacePath, Entry> {
        &self.0
    }
    pub fn get(&self, path: &WorkspacePath) -> Option<&Entry> {
        self.0.get(path)
    }
    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    pub fn remove(&mut self, path: &WorkspacePath) -> Option<Entry> {
        self.0.remove(path)
    }
    pub fn insert(&mut self, path: WorkspacePath, entry: Entry) {
        self.0.insert(path, entry);
    }

    /// Total bytes of file content listed.
    pub fn content_bytes(&self) -> u64 {
        self.0
            .values()
            .map(|entry| match entry {
                Entry::File { size, .. } => *size,
                Entry::Symlink { target } => target.len() as u64,
            })
            .sum()
    }

    /// Identifies the whole manifest, so two sides can tell they agree without
    /// exchanging it.
    pub fn digest(&self) -> Digest {
        Digest::of(&self.encode())
    }

    pub fn delta_to(&self, target: &Manifest) -> Delta {
        let mut delta = Delta::default();
        for (path, entry) in &target.0 {
            if self.0.get(path) != Some(entry) {
                delta.set.push((path.clone(), entry.clone()));
            }
        }
        for path in self.0.keys() {
            if !target.0.contains_key(path) {
                delta.remove.push(path.clone());
            }
        }
        delta
    }

    pub fn apply(&mut self, delta: &Delta) {
        for path in &delta.remove {
            self.0.remove(path);
        }
        for (path, entry) in &delta.set {
            self.0.insert(path.clone(), entry.clone());
        }
    }

    /// Whether a real file system could hold this manifest: no listed path lies
    /// inside another listed path, since that one is a file or symlink.
    pub fn is_tree(&self) -> bool {
        self.0.keys().all(|path| {
            let mut parent = path.parent();
            while let Some(directory) = parent {
                if self.0.contains_key(&directory) {
                    return false;
                }
                parent = directory.parent();
            }
            true
        })
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(16 + self.0.len() * 96);
        out.push(VERSION);
        out.extend_from_slice(&(self.0.len() as u32).to_be_bytes());
        for (path, entry) in &self.0 {
            put_text(&mut out, path.as_str());
            put_entry(&mut out, entry);
        }
        out
    }

    /// Decodes and validates an untrusted manifest.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let mut reader = Reader(bytes);
        if reader.byte()? != VERSION {
            return None;
        }
        let count = reader.count()?;
        let mut entries = BTreeMap::new();
        let mut previous: Option<WorkspacePath> = None;
        for _ in 0..count {
            let path = WorkspacePath::parse(reader.text()?)?;
            // Sorted and unique, as encoding produces.
            if previous.as_ref().is_some_and(|p| p >= &path) {
                return None;
            }
            let entry = reader.entry()?;
            previous = Some(path.clone());
            entries.insert(path, entry);
        }
        reader.0.is_empty().then_some(Self(entries))
    }
}

impl Delta {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = vec![VERSION];
        out.extend_from_slice(&(self.set.len() as u32).to_be_bytes());
        for (path, entry) in &self.set {
            put_text(&mut out, path.as_str());
            put_entry(&mut out, entry);
        }
        out.extend_from_slice(&(self.remove.len() as u32).to_be_bytes());
        for path in &self.remove {
            put_text(&mut out, path.as_str());
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let mut reader = Reader(bytes);
        if reader.byte()? != VERSION {
            return None;
        }
        let mut delta = Delta::default();
        for _ in 0..reader.count()? {
            let path = WorkspacePath::parse(reader.text()?)?;
            delta.set.push((path, reader.entry()?));
        }
        for _ in 0..reader.count()? {
            delta.remove.push(WorkspacePath::parse(reader.text()?)?);
        }
        reader.0.is_empty().then_some(delta)
    }
}

fn put_text(out: &mut Vec<u8>, text: &str) {
    out.extend_from_slice(&(text.len() as u16).to_be_bytes());
    out.extend_from_slice(text.as_bytes());
}
fn put_entry(out: &mut Vec<u8>, entry: &Entry) {
    match entry {
        Entry::File { mode, size, digest } => {
            out.push(0);
            out.extend_from_slice(&(*mode as u16).to_be_bytes());
            out.extend_from_slice(&size.to_be_bytes());
            out.extend_from_slice(&digest.0);
        }
        Entry::Symlink { target } => {
            out.push(1);
            put_text(out, target);
        }
    }
}

struct Reader<'a>(&'a [u8]);
impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.0.len() < n {
            return None;
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Some(head)
    }
    fn byte(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }
    fn count(&mut self) -> Option<u32> {
        let count = u32::from_be_bytes(self.take(4)?.try_into().ok()?);
        (count <= MAXIMUM_ENTRIES).then_some(count)
    }
    fn text(&mut self) -> Option<&'a str> {
        let length = u16::from_be_bytes(self.take(2)?.try_into().ok()?) as usize;
        std::str::from_utf8(self.take(length)?).ok()
    }
    fn entry(&mut self) -> Option<Entry> {
        match self.byte()? {
            0 => {
                let mode = u16::from_be_bytes(self.take(2)?.try_into().ok()?) as u32;
                let size = u64::from_be_bytes(self.take(8)?.try_into().ok()?);
                let digest = Digest(self.take(32)?.try_into().ok()?);
                (mode & !PERMISSIONS == 0).then_some(Entry::File { mode, size, digest })
            }
            1 => {
                let target = self.text()?;
                (!target.is_empty() && target.len() <= MAXIMUM_TARGET && !target.contains('\0'))
                    .then(|| Entry::Symlink {
                        target: target.to_owned(),
                    })
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(text: &str) -> WorkspacePath {
        WorkspacePath::parse(text).unwrap()
    }
    fn file(content: &[u8], mode: u32) -> Entry {
        Entry::File {
            mode,
            size: content.len() as u64,
            digest: Digest::of(content),
        }
    }

    #[test]
    fn encodes_diffs_and_applies() {
        let mut from = Manifest::default();
        from.insert(path("a.txt"), file(b"a", 0o644));
        from.insert(path("dir/run.sh"), file(b"#!", 0o755));
        from.insert(path("gone"), file(b"x", 0o644));
        let mut to = from.clone();
        to.remove(&path("gone"));
        to.insert(path("a.txt"), file(b"b", 0o644));
        to.insert(
            path("link"),
            Entry::Symlink {
                target: "../outside".into(),
            },
        );
        assert_eq!(Manifest::decode(&to.encode()), Some(to.clone()));
        let delta = from.delta_to(&to);
        assert_eq!(delta.remove, vec![path("gone")]);
        assert_eq!(delta.set.len(), 2);
        assert_eq!(Delta::decode(&delta.encode()), Some(delta.clone()));
        let mut applied = from.clone();
        applied.apply(&delta);
        assert_eq!(applied, to);
        assert_eq!(applied.digest(), to.digest());
        assert_ne!(from.digest(), to.digest());
        assert!(from.delta_to(&from).is_empty());
    }

    #[test]
    fn rejects_untrusted_encodings() {
        let mut manifest = Manifest::default();
        manifest.insert(path("a"), file(b"a", 0o644));
        let mut bytes = manifest.encode();
        bytes.push(0);
        assert!(Manifest::decode(&bytes).is_none(), "trailing bytes");
        let mut escaping = Manifest::default().encode();
        escaping[1..5].copy_from_slice(&1u32.to_be_bytes());
        put_text(&mut escaping, "../etc/passwd");
        put_entry(&mut escaping, &file(b"x", 0o644));
        assert!(Manifest::decode(&escaping).is_none(), "path escapes");
        let mut setuid = Manifest::default().encode();
        setuid[1..5].copy_from_slice(&1u32.to_be_bytes());
        put_text(&mut setuid, "x");
        put_entry(&mut setuid, &file(b"x", 0o4755));
        assert!(Manifest::decode(&setuid).is_none(), "set-id bits");
        assert!(Manifest::decode(&[9]).is_none());
        let mut nested = Manifest::default();
        nested.insert(path("a"), file(b"a", 0o644));
        nested.insert(path("a/b"), file(b"b", 0o644));
        assert!(!nested.is_tree());
        assert!(manifest.is_tree());
    }
}
