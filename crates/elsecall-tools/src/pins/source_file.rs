//! Reading pin source files without trusting the workspace that holds them.
//!
//! A repository decides what its files are: a link out of the workspace, a FIFO
//! that never ends, a file of gigabytes, thousands of names. A workspace path is
//! resolved one component at a time, reading each link's text, so nothing
//! outside the workspace is touched, not even to refuse it: a link that leaves is
//! refused by its text. The resolved file is opened without following links and
//! without blocking, and only a regular UTF-8 file of at most 1 MiB is read.
//! Contents never appear in an error.
use crate::pins::{invalid, unreadable};
use elsecall_contract::failure::Result;
use std::{
    collections::VecDeque,
    ffi::OsString,
    fs,
    io::{self, Read},
    os::unix::fs::OpenOptionsExt,
    path::{Component, Path, PathBuf},
};

const LIMIT: u64 = 1024 * 1024;
/// As many links as one path may pass through, as the system's own bound.
const MAX_LINKS: usize = 32;

/// The text of `relative` inside the canonical `workspace`, or `None` when it
/// does not exist. `relative` names the file in errors.
pub(super) fn read_workspace_file(workspace: &Path, relative: &str) -> Result<Option<String>> {
    match resolve(workspace, relative)? {
        Some(path) => read_resolved(&path, relative).map(Some),
        None => Ok(None),
    }
}

/// The text of the operator's own file at `path`, which may link anywhere, or
/// `None` when it does not exist. `display` names it in errors.
pub(super) fn read_home_file(path: &Path, display: &str) -> Result<Option<String>> {
    match fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(error) if absent(&error) => return Ok(None),
        Err(error) => return Err(unreadable(display, &io_reason(&error))),
    }
    let resolved = fs::canonicalize(path)
        .map_err(|_| unreadable(display, "it is a link to nothing that can be read"))?;
    read_resolved(&resolved, display).map(Some)
}

/// The names in the workspace directory `relative` that `keep` accepts, sorted,
/// or none when it does not exist. More than `limit` of them is an error.
pub(super) fn list_workspace_directory(
    workspace: &Path,
    relative: &str,
    keep: impl Fn(&str) -> bool,
    limit: usize,
) -> Result<Vec<String>> {
    let display = if relative.is_empty() { "." } else { relative };
    let Some(directory) = resolve(workspace, relative)? else {
        return Ok(Vec::new());
    };
    let entries = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if absent(&error) => return Ok(Vec::new()),
        Err(error) => return Err(unreadable(display, &io_reason(&error))),
    };
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| unreadable(display, &io_reason(&error)))?;
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if name.chars().any(char::is_control) || !keep(&name) {
            continue;
        }
        if names.len() == limit {
            return Err(invalid(
                display,
                "",
                &format!("at most {limit} mise config files"),
            ));
        }
        names.push(name);
    }
    names.sort();
    Ok(names)
}

/// `relative` under `workspace` with every link replaced by its target, or
/// `None` when a component does not exist. Only paths inside the workspace are
/// looked at.
fn resolve(workspace: &Path, relative: &str) -> Result<Option<PathBuf>> {
    let outside = || unreadable(relative, "it resolves outside the workspace");
    let mut pending: VecDeque<OsString> = components(Path::new(relative)).ok_or_else(outside)?;
    let mut current = workspace.to_path_buf();
    let mut links = 0;
    while let Some(part) = pending.pop_front() {
        if part == ".." {
            if current == workspace {
                return Err(outside());
            }
            current.pop();
            continue;
        }
        let next = current.join(&part);
        let metadata = match fs::symlink_metadata(&next) {
            Ok(metadata) => metadata,
            Err(error) if absent(&error) => return Ok(None),
            Err(error) => return Err(unreadable(relative, &io_reason(&error))),
        };
        if !metadata.file_type().is_symlink() {
            current = next;
            continue;
        }
        links += 1;
        if links > MAX_LINKS {
            return Err(unreadable(relative, "it passes through too many links"));
        }
        let target =
            fs::read_link(&next).map_err(|error| unreadable(relative, &io_reason(&error)))?;
        let rest = if target.is_absolute() {
            current = workspace.to_path_buf();
            target.strip_prefix(workspace).map_err(|_| outside())?
        } else {
            &target
        };
        let mut parts = components(rest).ok_or_else(outside)?;
        parts.extend(pending);
        pending = parts;
    }
    Ok(Some(current))
}

/// The components of a relative path, `..` kept for the caller to apply.
fn components(path: &Path) -> Option<VecDeque<OsString>> {
    let mut parts = VecDeque::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => parts.push_back(part.to_owned()),
            Component::ParentDir => parts.push_back(OsString::from("..")),
            Component::CurDir => {}
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(parts)
}

/// Reads a path that has no links left in it.
fn read_resolved(path: &Path, display: &str) -> Result<String> {
    // Non-blocking, so a FIFO cannot hold the read open; it is then refused as
    // not a regular file. No-follow, so a link swapped in after resolving fails.
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
        .open(path)
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
    String::from_utf8(bytes).map_err(|_| unreadable(display, "it is not UTF-8 text"))
}

fn absent(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
    )
}

fn io_reason(error: &io::Error) -> String {
    if error.raw_os_error() == Some(libc::ELOOP) {
        return "it changed into a link while being read".to_owned();
    }
    match error.kind() {
        io::ErrorKind::PermissionDenied => "permission was denied".to_owned(),
        io::ErrorKind::IsADirectory => "it is a directory".to_owned(),
        kind => format!("it could not be read ({kind})"),
    }
}

#[cfg(test)]
mod tests {
    use crate::pins::source_file::*;
    use std::os::unix::fs::symlink;

    fn root() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        (dir, root)
    }

    #[test]
    fn a_missing_file_is_absent_and_a_present_one_is_read() {
        let (_dir, root) = root();
        assert_eq!(read_workspace_file(&root, "mise.toml").unwrap(), None);
        fs::write(root.join("mise.toml"), "[tools]\n").unwrap();
        assert_eq!(
            read_workspace_file(&root, "mise.toml").unwrap().as_deref(),
            Some("[tools]\n")
        );
        assert_eq!(
            read_workspace_file(&root, "missing/config.toml").unwrap(),
            None
        );
        fs::write(root.join("mise"), "a file where a directory would be").unwrap();
        assert_eq!(
            read_workspace_file(&root, "mise/config.toml").unwrap(),
            None
        );
    }

    #[test]
    fn links_inside_the_workspace_are_followed() {
        let (_dir, root) = root();
        fs::create_dir_all(root.join("shared/mise")).unwrap();
        fs::write(root.join("shared/mise/config.toml"), "[tools]\n").unwrap();
        symlink("shared", root.join(".config")).unwrap();
        symlink("../real.toml", root.join("shared/link.toml")).unwrap();
        fs::write(root.join("real.toml"), "linked").unwrap();
        symlink(root.join("real.toml"), root.join(".mise.toml")).unwrap();
        assert!(
            read_workspace_file(&root, ".config/mise/config.toml")
                .unwrap()
                .is_some()
        );
        assert_eq!(
            read_workspace_file(&root, ".config/link.toml")
                .unwrap()
                .as_deref(),
            Some("linked")
        );
        assert_eq!(
            read_workspace_file(&root, ".mise.toml").unwrap().as_deref(),
            Some("linked")
        );
    }

    #[test]
    fn links_out_of_the_workspace_are_refused_by_their_text() {
        let (_outside_dir, outside) = root();
        fs::create_dir_all(outside.join("mise/conf.d")).unwrap();
        fs::write(outside.join("id_ed25519"), "PRIVATE KEY MATERIAL").unwrap();
        fs::write(outside.join("mise/conf.d/private-name.toml"), "").unwrap();
        let (_dir, root) = root();
        symlink(outside.join("id_ed25519"), root.join("mise.toml")).unwrap();
        symlink(&outside, root.join(".config")).unwrap();
        symlink("../../escape", root.join(".mise.toml")).unwrap();
        for file in ["mise.toml", ".config/mise/config.toml", ".mise.toml"] {
            let error = read_workspace_file(&root, file).unwrap_err();
            assert_eq!(error.code, "pin_source_unreadable");
            assert_eq!(
                error.message,
                format!("{file}: it resolves outside the workspace.")
            );
        }
        let error =
            list_workspace_directory(&root, ".config/mise/conf.d", |_| true, 64).unwrap_err();
        assert!(!error.message.contains("private-name"), "{}", error.message);
        assert!(read_home_file(&root.join("mise.toml"), "~/x").is_ok());
    }

    #[test]
    fn link_loops_end() {
        let (_dir, root) = root();
        symlink("b.toml", root.join("a.toml")).unwrap();
        symlink("a.toml", root.join("b.toml")).unwrap();
        let error = read_workspace_file(&root, "a.toml").unwrap_err();
        assert!(
            error.message.contains("too many links"),
            "{}",
            error.message
        );
    }

    #[test]
    fn fifos_directories_large_and_binary_files_are_unreadable() {
        let (_dir, root) = root();
        let fifo = root.join("mise.toml");
        let name = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let error = read_workspace_file(&root, "mise.toml").unwrap_err();
        assert!(
            error.message.contains("not a regular file"),
            "{}",
            error.message
        );

        fs::create_dir(root.join(".tool-versions")).unwrap();
        assert!(read_workspace_file(&root, ".tool-versions").is_err());

        fs::write(root.join("large.toml"), vec![b'#'; (LIMIT + 1) as usize]).unwrap();
        let error = read_workspace_file(&root, "large.toml").unwrap_err();
        assert!(error.message.contains("1 MiB"));

        fs::write(root.join("binary.toml"), [0xff, 0xfe]).unwrap();
        let error = read_workspace_file(&root, "binary.toml").unwrap_err();
        assert!(error.message.contains("UTF-8"));

        symlink("nothing", root.join("dangling.toml")).unwrap();
        assert_eq!(read_workspace_file(&root, "dangling.toml").unwrap(), None);
    }

    #[test]
    fn names_are_filtered_sorted_and_bounded() {
        let (_dir, root) = root();
        assert!(
            list_workspace_directory(&root, "none", |_| true, 2)
                .unwrap()
                .is_empty()
        );
        for name in ["b.toml", "a.toml", "notes.txt"] {
            fs::write(root.join(name), "").unwrap();
        }
        let toml = |name: &str| name.ends_with(".toml");
        assert_eq!(
            list_workspace_directory(&root, "", toml, 2).unwrap(),
            ["a.toml", "b.toml"]
        );
        let error = list_workspace_directory(&root, "", toml, 1).unwrap_err();
        assert_eq!(error.message, ".: at most 1 mise config files.");
    }
}
