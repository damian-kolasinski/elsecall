//! What a directory tree occupies on disk.
use std::{io, os::unix::fs::MetadataExt, path::Path};

/// Bytes allocated to everything under `root`, never following links. A missing
/// tree holds nothing. Clones and hard links are counted in full.
pub fn allocated(root: &Path) -> io::Result<u64> {
    let meta = match std::fs::symlink_metadata(root) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    let mut total = meta.blocks() * 512;
    if !meta.is_dir() {
        return Ok(total);
    }
    let mut pending = vec![root.to_owned()];
    while let Some(directory) = pending.pop() {
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            // A tree changing under the walk, or a directory a tool made
            // unreadable, is counted as far as it can be.
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
                ) =>
            {
                continue;
            }
            Err(e) => return Err(e),
        };
        for entry in entries.flatten() {
            // `DirEntry::metadata` does not follow links.
            let Ok(meta) = entry.metadata() else { continue };
            total += meta.blocks() * 512;
            if meta.is_dir() {
                pending.push(entry.path());
            }
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_files_and_directories_but_not_through_links() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("tree");
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        std::fs::write(root.join("a/b/file"), vec![7u8; 64 * 1024]).unwrap();
        let outside = dir.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("big"), vec![7u8; 1 << 20]).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        let size = allocated(&root).unwrap();
        assert!(size >= 64 * 1024, "{size}");
        assert!(size < 1 << 20, "a link is not followed: {size}");
        assert_eq!(allocated(&dir.path().join("missing")).unwrap(), 0);
    }
}
