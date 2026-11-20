//! Reading one pin source file without trusting the workspace that holds it.
//!
//! A repository decides what its files are: a link out of the workspace, a FIFO
//! that never ends, a file of gigabytes. Only a regular UTF-8 file of at most
//! 1 MiB is read, and a workspace file must resolve inside the workspace.
//! Contents never appear in an error.
use crate::pins::unreadable;
use elsecall_contract::failure::Result;
use std::{
    fs,
    io::{self, Read},
    os::unix::fs::OpenOptionsExt,
    path::Path,
};

const LIMIT: u64 = 1024 * 1024;

/// Where a file may resolve to once links are followed.
pub(super) enum Confinement<'a> {
    /// Inside this canonical workspace.
    Within(&'a Path),
    /// Anywhere: the operator's own configuration.
    Anywhere,
}

/// The file's text, or `None` when nothing exists at `path`. `display` names
/// the file in errors.
pub(super) fn read_text(
    path: &Path,
    display: &str,
    confinement: Confinement<'_>,
) -> Result<Option<String>> {
    match fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(error) if absent(&error) => return Ok(None),
        Err(error) => return Err(unreadable(display, &io_reason(&error))),
    }
    let resolved = fs::canonicalize(path)
        .map_err(|_| unreadable(display, "it is a link to nothing that can be read"))?;
    if let Confinement::Within(root) = confinement
        && !resolved.starts_with(root)
    {
        return Err(unreadable(display, "it resolves outside the workspace"));
    }
    // Non-blocking, so a FIFO planted under a pin file's name cannot hold the
    // read open; it is then refused as not a regular file.
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&resolved)
        .map_err(|error| unreadable(display, &io_reason(&error)))?;
    let metadata = file
        .metadata()
        .map_err(|error| unreadable(display, &io_reason(&error)))?;
    if !metadata.is_file() {
        return Err(unreadable(display, "it is not a regular file"));
    }
    if metadata.len() > LIMIT {
        return Err(unreadable(display, "it is larger than 1 MiB"));
    }
    let mut bytes = Vec::new();
    file.take(LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| unreadable(display, &io_reason(&error)))?;
    if bytes.len() as u64 > LIMIT {
        return Err(unreadable(display, "it is larger than 1 MiB"));
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| unreadable(display, "it is not UTF-8 text"))
}

/// The names in a workspace directory, sorted, or none when it does not exist.
/// Names that are not UTF-8 cannot be pin files and are skipped.
pub(super) fn list_names(directory: &Path, display: &str) -> Result<Vec<String>> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if absent(&error) => return Ok(Vec::new()),
        Err(error) => return Err(unreadable(display, &io_reason(&error))),
    };
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| unreadable(display, &io_reason(&error)))?;
        if let Ok(name) = entry.file_name().into_string() {
            names.push(name);
        }
    }
    names.sort();
    Ok(names)
}

fn absent(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
    )
}

fn io_reason(error: &io::Error) -> String {
    match error.kind() {
        io::ErrorKind::PermissionDenied => "permission was denied".to_owned(),
        io::ErrorKind::IsADirectory => "it is a directory".to_owned(),
        kind => format!("it could not be read ({kind})"),
    }
}

#[cfg(test)]
mod tests {
    use crate::pins::source_file::*;
    use std::{os::unix::fs::symlink, path::PathBuf};

    fn root() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        (dir, root)
    }

    #[test]
    fn a_missing_file_is_absent_and_a_present_one_is_read() {
        let (_dir, root) = root();
        let path = root.join("mise.toml");
        assert_eq!(
            read_text(&path, "mise.toml", Confinement::Within(&root)).unwrap(),
            None
        );
        fs::write(&path, "[tools]\n").unwrap();
        assert_eq!(
            read_text(&path, "mise.toml", Confinement::Within(&root))
                .unwrap()
                .as_deref(),
            Some("[tools]\n")
        );
        let nested = root.join("missing/config.toml");
        assert_eq!(
            read_text(&nested, "missing/config.toml", Confinement::Within(&root)).unwrap(),
            None
        );
    }

    #[test]
    fn a_link_out_of_the_workspace_is_refused_unless_unconfined() {
        let (_outside_dir, outside) = root();
        let secret = outside.join("id_ed25519");
        fs::write(&secret, "PRIVATE KEY MATERIAL").unwrap();
        let (_dir, root) = root();
        let link = root.join("mise.toml");
        symlink(&secret, &link).unwrap();
        let error = read_text(&link, "mise.toml", Confinement::Within(&root)).unwrap_err();
        assert_eq!(error.code, "pin_source_unreadable");
        assert!(error.message.contains("outside the workspace"));
        assert!(!error.message.contains("PRIVATE"));
        assert!(read_text(&link, "mise.toml", Confinement::Anywhere).is_ok());
        let inside = root.join("real.toml");
        fs::write(&inside, "[tools]\n").unwrap();
        let alias = root.join(".mise.toml");
        symlink(&inside, &alias).unwrap();
        assert!(read_text(&alias, ".mise.toml", Confinement::Within(&root)).is_ok());
    }

    #[test]
    fn fifos_directories_large_and_binary_files_are_unreadable() {
        let (_dir, root) = root();
        let fifo = root.join("mise.toml");
        let name = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let error = read_text(&fifo, "mise.toml", Confinement::Within(&root)).unwrap_err();
        assert!(
            error.message.contains("not a regular file"),
            "{}",
            error.message
        );

        let directory = root.join(".tool-versions");
        fs::create_dir(&directory).unwrap();
        assert!(read_text(&directory, ".tool-versions", Confinement::Within(&root)).is_err());

        let large = root.join("large.toml");
        fs::write(&large, vec![b'#'; (LIMIT + 1) as usize]).unwrap();
        let error = read_text(&large, "large.toml", Confinement::Within(&root)).unwrap_err();
        assert!(error.message.contains("1 MiB"));

        let binary = root.join("binary.toml");
        fs::write(&binary, [0xff, 0xfe]).unwrap();
        let error = read_text(&binary, "binary.toml", Confinement::Within(&root)).unwrap_err();
        assert!(error.message.contains("UTF-8"));

        let dangling = root.join("dangling.toml");
        symlink(root.join("nothing"), &dangling).unwrap();
        assert!(read_text(&dangling, "dangling.toml", Confinement::Within(&root)).is_err());
    }

    #[test]
    fn names_are_listed_sorted_and_a_missing_directory_is_empty() {
        let (_dir, root) = root();
        assert!(list_names(&root.join("none"), "none").unwrap().is_empty());
        fs::write(root.join("b.toml"), "").unwrap();
        fs::write(root.join("a.toml"), "").unwrap();
        assert_eq!(list_names(&root, ".").unwrap(), ["a.toml", "b.toml"]);
    }
}
