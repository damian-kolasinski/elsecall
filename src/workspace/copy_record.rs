//! What Elsecall last wrote into a copy, kept in the member's private state
//! directory: the caller's manifest, each file's status as written, the index, the
//! index file's status, HEAD and the carried refs. Any status that differs later
//! means a run changed the file.
use crate::workspace::{
    carried_refs::CarriedRefs, confined::Status, git::ObjectFormat, head::Head,
    index::IndexListing, manifest::Manifest, path::WorkspacePath,
};
use std::{collections::BTreeMap, collections::HashMap, io, path::Path};

const VERSION: u8 = 3;

/// A file's status as Elsecall wrote it. The change time is included because no
/// program can set it back, so an edit that restores the size and modification
/// time is still noticed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Written {
    inode: u64,
    size: u64,
    modified_ns: i64,
    changed_ns: i64,
    mode: u32,
}
impl From<&Status> for Written {
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
impl Written {
    fn encode(&self, out: &mut Vec<u8>) {
        for value in [
            self.inode,
            self.size,
            self.modified_ns as u64,
            self.changed_ns as u64,
            self.mode as u64,
        ] {
            out.extend_from_slice(&value.to_be_bytes());
        }
    }
}

#[derive(Default)]
pub struct CopyRecord {
    /// The copy directory's device and inode, so a replaced directory starts afresh.
    pub identity: (u64, u64),
    pub manifest: Manifest,
    pub written: HashMap<WorkspacePath, Written>,
    pub index: IndexListing,
    pub head: Option<Head>,
    /// git's index file as Elsecall last left it, so an index no run touched is not
    /// listed again.
    pub index_file: Option<Written>,
    /// The refs last carried, whose objects the store keeps references for. Records
    /// written before refs were carried end without them.
    pub refs: Option<CarriedRefs>,
}

struct Reader<'a>(&'a [u8]);
impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let (head, tail) = self.0.split_at_checked(n)?;
        self.0 = tail;
        Some(head)
    }
    fn number(&mut self) -> Option<u64> {
        Some(u64::from_be_bytes(self.take(8)?.try_into().ok()?))
    }
    fn written(&mut self) -> Option<Written> {
        Some(Written {
            inode: self.number()?,
            size: self.number()?,
            modified_ns: self.number()? as i64,
            changed_ns: self.number()? as i64,
            mode: self.number()? as u32,
        })
    }
    fn block(&mut self) -> Option<&'a [u8]> {
        let length = self.number()? as usize;
        self.take(length)
    }
}

impl CopyRecord {
    pub fn load(path: &Path, format: ObjectFormat) -> Option<Self> {
        let bytes = std::fs::read(path).ok()?;
        let mut reader = Reader(&bytes);
        if reader.take(1)?[0] != VERSION {
            return None;
        }
        let identity = (reader.number()?, reader.number()?);
        let manifest = Manifest::decode(reader.block()?)?;
        let count = reader.number()? as usize;
        let mut written = HashMap::with_capacity(count.min(1 << 20));
        for _ in 0..count {
            let length = u16::from_be_bytes(reader.take(2)?.try_into().ok()?) as usize;
            let path = WorkspacePath::parse(std::str::from_utf8(reader.take(length)?).ok()?)?;
            written.insert(path, reader.written()?);
        }
        let index = IndexListing::decode(reader.block()?, format)?;
        let head = serde_json::from_slice(reader.block()?).ok()?;
        let index_file = match reader.take(1)?[0] {
            0 => None,
            _ => Some(reader.written()?),
        };
        let refs = match reader.0.is_empty() {
            true => None,
            false => serde_json::from_slice(reader.block()?).ok()?,
        };
        reader.0.is_empty().then_some(Self {
            identity,
            manifest,
            written,
            index,
            head,
            index_file,
            refs,
        })
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        let mut out = vec![VERSION];
        out.extend_from_slice(&self.identity.0.to_be_bytes());
        out.extend_from_slice(&self.identity.1.to_be_bytes());
        let block = |out: &mut Vec<u8>, bytes: &[u8]| {
            out.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
            out.extend_from_slice(bytes);
        };
        block(&mut out, &self.manifest.encode());
        out.extend_from_slice(&(self.written.len() as u64).to_be_bytes());
        let written: BTreeMap<_, _> = self.written.iter().collect();
        for (path, status) in written {
            out.extend_from_slice(&(path.as_str().len() as u16).to_be_bytes());
            out.extend_from_slice(path.as_str().as_bytes());
            status.encode(&mut out);
        }
        block(&mut out, &self.index.encode());
        block(
            &mut out,
            &serde_json::to_vec(&self.head).expect("heads serialize"),
        );
        match &self.index_file {
            None => out.push(0),
            Some(status) => {
                out.push(1);
                status.encode(&mut out);
            }
        }
        if let Some(refs) = &self.refs {
            block(&mut out, &serde_json::to_vec(refs).expect("refs serialize"));
        }
        crate::private_files::replace(path, &out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_keep_carried_refs_and_read_older_ones_without() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("record");
        let mut record = CopyRecord {
            identity: (1, 2),
            ..Default::default()
        };
        record.save(&path).unwrap();
        let older = CopyRecord::load(&path, ObjectFormat::Sha1).unwrap();
        assert_eq!(older.refs, None, "a record without refs reads as before");

        let refs: CarriedRefs = serde_json::from_str(&format!(
            r#"{{"refs":[{{"name":"refs/remotes/origin/main","object":"{}"}}],"defaults":[{{"remote":"origin","branch":"refs/remotes/origin/main"}}]}}"#,
            "a".repeat(40)
        ))
        .unwrap();
        record.refs = Some(refs.clone());
        record.save(&path).unwrap();
        let loaded = CopyRecord::load(&path, ObjectFormat::Sha1).unwrap();
        assert_eq!(loaded.refs, Some(refs));
        assert_eq!(loaded.identity, (1, 2));
    }
}
