//! The staged state: git's index as `git ls-files -s` lists it, without stat data.
//! Copies reproduce it entry by entry, so unchanged entries keep their stat data
//! on the member and git does not re-read unchanged files.
use crate::workspace::{
    digest::Digest,
    git::{ObjectFormat, Oid},
    path::WorkspacePath,
};
use std::collections::BTreeMap;

pub const GITLINK: u32 = 0o160000;
const MODES: [u32; 4] = [0o100644, 0o100755, 0o120000, GITLINK];
const VERSION: u8 = 1;

/// One staged entry of a path: its stage (0, or 1 to 3 while conflicted), mode and
/// object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Staged {
    pub stage: u8,
    pub mode: u32,
    pub oid: Oid,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IndexListing(BTreeMap<WorkspacePath, Vec<Staged>>);

/// Paths whose staged entries change; an empty list removes the path.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IndexDelta(pub Vec<(WorkspacePath, Vec<Staged>)>);

impl IndexListing {
    /// Parses `git ls-files -s -z` output. `None` if git printed something this
    /// version cannot reproduce, such as a path that is not UTF-8.
    pub fn parse_git(output: &[u8], format: ObjectFormat) -> Option<Self> {
        let mut listing: BTreeMap<WorkspacePath, Vec<Staged>> = BTreeMap::new();
        for record in output.split(|b| *b == 0).filter(|r| !r.is_empty()) {
            let text = std::str::from_utf8(record).ok()?;
            let (meta, path) = text.split_once('\t')?;
            let mut fields = meta.split(' ');
            let mode = u32::from_str_radix(fields.next()?, 8).ok()?;
            let oid = Oid::parse(fields.next()?)?;
            let stage: u8 = fields.next()?.parse().ok()?;
            let staged = valid(Staged { stage, mode, oid }, format)?;
            listing
                .entry(WorkspacePath::parse(path)?)
                .or_default()
                .push(staged);
        }
        Some(Self(listing))
    }

    pub fn paths(&self) -> &BTreeMap<WorkspacePath, Vec<Staged>> {
        &self.0
    }

    pub fn len(&self) -> usize {
        self.0.values().map(Vec::len).sum()
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The stage-0 entry of `path`, if it is staged without conflict.
    pub fn resolved(&self, path: &WorkspacePath) -> Option<&Staged> {
        self.0
            .get(path)
            .and_then(|entries| entries.iter().find(|e| e.stage == 0))
    }

    pub fn digest(&self) -> Digest {
        Digest::of(&self.encode())
    }

    pub fn delta_to(&self, target: &IndexListing) -> IndexDelta {
        let mut changes = Vec::new();
        for (path, entries) in &target.0 {
            if self.0.get(path) != Some(entries) {
                changes.push((path.clone(), entries.clone()));
            }
        }
        for path in self.0.keys() {
            if !target.0.contains_key(path) {
                changes.push((path.clone(), Vec::new()));
            }
        }
        IndexDelta(changes)
    }

    pub fn apply(&mut self, delta: &IndexDelta) {
        for (path, entries) in &delta.0 {
            if entries.is_empty() {
                self.0.remove(path);
            } else {
                self.0.insert(path.clone(), entries.clone());
            }
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        encode_changes(self.0.iter())
    }

    pub fn decode(bytes: &[u8], format: ObjectFormat) -> Option<Self> {
        let changes = decode_changes(bytes, format)?;
        let mut listing = BTreeMap::new();
        for (path, entries) in changes {
            if entries.is_empty() || listing.insert(path, entries).is_some() {
                return None;
            }
        }
        Some(Self(listing))
    }
}

impl IndexDelta {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn encode(&self) -> Vec<u8> {
        encode_changes(self.0.iter().map(|(p, e)| (p, e)))
    }

    pub fn decode(bytes: &[u8], format: ObjectFormat) -> Option<Self> {
        decode_changes(bytes, format).map(Self)
    }

    /// Input for `git update-index -z --index-info`: each changed path is removed,
    /// then its target entries are added.
    pub fn index_info(&self, format: ObjectFormat) -> Vec<u8> {
        let zero = format.zero();
        let mut out = Vec::new();
        for (path, entries) in &self.0 {
            out.extend_from_slice(format!("0 {zero}\t{path}\0").as_bytes());
            for entry in entries {
                out.extend_from_slice(
                    format!("{:06o} {} {}\t{path}\0", entry.mode, entry.oid, entry.stage)
                        .as_bytes(),
                );
            }
        }
        out
    }

    /// Objects the target entries name, which must exist for the index to be usable.
    pub fn objects(&self) -> impl Iterator<Item = &Oid> {
        self.0
            .iter()
            .flat_map(|(_, entries)| entries)
            .filter(|e| e.mode != GITLINK)
            .map(|e| &e.oid)
    }
}

fn valid(staged: Staged, format: ObjectFormat) -> Option<Staged> {
    (MODES.contains(&staged.mode) && staged.stage <= 3 && staged.oid.fits(format)).then_some(staged)
}

fn encode_changes<'a>(
    changes: impl Iterator<Item = (&'a WorkspacePath, &'a Vec<Staged>)>,
) -> Vec<u8> {
    let mut out = vec![VERSION];
    let changes: Vec<_> = changes.collect();
    out.extend_from_slice(&(changes.len() as u32).to_be_bytes());
    for (path, entries) in changes {
        out.extend_from_slice(&(path.as_str().len() as u16).to_be_bytes());
        out.extend_from_slice(path.as_str().as_bytes());
        out.push(entries.len() as u8);
        for entry in entries {
            out.push(entry.stage);
            out.extend_from_slice(&entry.mode.to_be_bytes());
            out.push(entry.oid.as_str().len() as u8);
            out.extend_from_slice(entry.oid.as_str().as_bytes());
        }
    }
    out
}

fn decode_changes(bytes: &[u8], format: ObjectFormat) -> Option<Vec<(WorkspacePath, Vec<Staged>)>> {
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
    if count > 4_000_000 {
        return None;
    }
    let mut changes = Vec::new();
    for _ in 0..count {
        let length = u16::from_be_bytes(take(2)?.try_into().ok()?) as usize;
        let path = WorkspacePath::parse(std::str::from_utf8(take(length)?).ok()?)?;
        let entries = take(1)?[0];
        if entries > 4 {
            return None;
        }
        let mut staged = Vec::new();
        for _ in 0..entries {
            let stage = take(1)?[0];
            let mode = u32::from_be_bytes(take(4)?.try_into().ok()?);
            let length = take(1)?[0] as usize;
            let oid = Oid::parse(std::str::from_utf8(take(length)?).ok()?)?;
            staged.push(valid(Staged { stage, mode, oid }, format)?);
        }
        changes.push((path, staged));
    }
    rest.is_empty().then_some(changes)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FORMAT: ObjectFormat = ObjectFormat::Sha1;

    #[test]
    fn parses_git_and_diffs() {
        let a = "a".repeat(40);
        let b = "b".repeat(40);
        let output = format!(
            "100644 {a} 0\tREADME\0100755 {b} 0\tbin/run\0100644 {a} 1\tc\0100644 {b} 2\tc\0"
        );
        let listing = IndexListing::parse_git(output.as_bytes(), FORMAT).unwrap();
        assert_eq!(listing.len(), 4);
        assert_eq!(
            IndexListing::decode(&listing.encode(), FORMAT),
            Some(listing.clone())
        );
        let path = |p: &str| WorkspacePath::parse(p).unwrap();
        assert!(listing.resolved(&path("c")).is_none(), "conflicted");
        let mut target = listing.clone();
        target.apply(&IndexDelta(vec![
            (path("README"), Vec::new()),
            (
                path("c"),
                vec![Staged {
                    stage: 0,
                    mode: 0o100644,
                    oid: Oid::parse(&b).unwrap(),
                }],
            ),
        ]));
        let delta = listing.delta_to(&target);
        assert_eq!(delta.0.len(), 2);
        assert_eq!(
            IndexDelta::decode(&delta.encode(), FORMAT),
            Some(delta.clone())
        );
        let info = String::from_utf8(delta.index_info(FORMAT)).unwrap();
        assert!(info.contains(&format!("0 {}\tc\0100644 {b} 0\tc\0", "0".repeat(40))));
        let mut applied = listing.clone();
        applied.apply(&delta);
        assert_eq!(applied.digest(), target.digest());
    }

    #[test]
    fn refuses_entries_it_cannot_reproduce() {
        let a = "a".repeat(40);
        for bad in [
            format!("100600 {a} 0\tx\0"),
            format!("100644 {a} 4\tx\0"),
            format!("100644 {a} 0\t.git/config\0"),
            format!("100644 {} 0\tx\0", "a".repeat(64)),
        ] {
            assert!(
                IndexListing::parse_git(bad.as_bytes(), FORMAT).is_none(),
                "{bad}"
            );
        }
    }
}
