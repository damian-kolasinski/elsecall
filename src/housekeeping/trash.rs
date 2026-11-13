//! Deleting a copy or object store safely.
//!
//! A directory is checked to be what housekeeping means to remove, never a link
//! or a stranger's directory, before anything happens to it. It is then moved
//! aside in one atomic rename, so a run that comes next finds nothing at the old
//! place and starts afresh, and checked again where it landed: it must be the same
//! directory, by device and inode. Deletion walks from open directory handles and
//! never follows links. A pass interrupted after the move finishes the deletion on
//! its next run, and deletes only a directory that is still the one it moved.
use crate::{
    failure::{Failure, Result},
    housekeeping::catalog::Catalog,
    workspace::confined::{self, Confined},
};
use std::{
    ffi::CString,
    fs, io,
    os::unix::{
        ffi::OsStrExt,
        fs::{DirBuilderExt, MetadataExt},
    },
    path::{Path, PathBuf},
    sync::Arc,
};

pub struct Trash {
    catalog: Arc<Catalog>,
    /// Inside the workspace root, so mapped copies and stores move within one volume.
    directory: PathBuf,
}

/// What moving a directory aside found.
pub enum Moved {
    /// It is now at this place, waiting to be deleted.
    Aside(PathBuf),
    /// Nothing was there.
    Missing,
}

fn failed(action: &str, path: &Path, error: io::Error) -> Failure {
    Failure::new(
        "housekeeping_failed",
        format!("{action} {} failed: {error}.", path.display()),
        "Check the ownership and permissions of the workspace root; the next housekeeping pass retries.",
    )
}

/// A directory's device and inode.
pub type Identity = (u64, u64);

fn identity(path: &Path) -> io::Result<Option<Identity>> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => Ok(Some((meta.dev(), meta.ino()))),
        Ok(_) => Ok(None),
        Err(e) => Err(e),
    }
}

fn not_ours(path: &Path) -> Failure {
    Failure::new(
        "housekeeping_refused",
        format!(
            "{} is not what Elsecall recorded there, so it was left untouched.",
            path.display()
        ),
        "Inspect the directory yourself; Elsecall stopped tracking it.",
    )
}

impl Trash {
    pub fn new(catalog: Arc<Catalog>, workspace_root: &Path) -> Self {
        Self {
            catalog,
            directory: workspace_root.join(".trash"),
        }
    }

    fn fresh_name(&self, beside: &Path) -> Result<PathBuf> {
        let name =
            confined::temporary_name().map_err(|e| failed("Naming a removal for", beside, e))?;
        Ok(PathBuf::from(std::ffi::OsStr::from_bytes(name.as_bytes())))
    }

    /// Moves the directory at `path` aside, provided `is_expected` holds for it
    /// before and after the move and it is the same directory throughout. If it
    /// changed during the move, it is moved back, or left where it is and never
    /// deleted.
    pub fn move_aside(&self, path: &Path, is_expected: impl Fn(&Path) -> bool) -> Result<Moved> {
        let before = match identity(path) {
            Ok(Some(before)) if is_expected(path) => before,
            Ok(_) => return Err(not_ours(path)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Moved::Missing),
            Err(e) => return Err(failed("Inspecting", path, e)),
        };
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.directory)
            .map_err(|e| failed("Creating", &self.directory, e))?;
        let name = self.fresh_name(path)?;
        let mut aside = self.directory.join(&name);
        self.catalog.add_trash(&aside, before)?;
        let mut moved = fs::rename(path, &aside);
        if moved
            .as_ref()
            .is_err_and(|e| e.raw_os_error() == Some(libc::EXDEV))
        {
            // A same-path copy on another volume is moved aside next to itself.
            self.catalog.remove_trash(&aside)?;
            aside = path.parent().ok_or_else(|| not_ours(path))?.join(&name);
            self.catalog.add_trash(&aside, before)?;
            moved = fs::rename(path, &aside);
        }
        match moved {
            Ok(()) => (),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                self.catalog.remove_trash(&aside)?;
                return Ok(Moved::Missing);
            }
            Err(e) => {
                self.catalog.remove_trash(&aside)?;
                return Err(failed("Moving aside", path, e));
            }
        }
        if identity(&aside).ok().flatten() == Some(before) && is_expected(&aside) {
            return Ok(Moved::Aside(aside));
        }
        // Whatever was moved is not what was checked: it is never deleted.
        self.catalog.remove_trash(&aside)?;
        match fs::rename(&aside, path) {
            Ok(()) => Err(not_ours(path)),
            Err(_) => Err(Failure::new(
                "housekeeping_refused",
                format!(
                    "{} changed while Elsecall moved it aside to remove it; it is now at {} and was left untouched.",
                    path.display(),
                    aside.display()
                ),
                "Inspect it and move it back yourself.",
            )),
        }
    }

    /// Deletes a directory moved aside.
    pub fn empty(&self, aside: &Path) -> Result<()> {
        let (Some(parent), Some(name)) = (aside.parent(), aside.file_name()) else {
            return Err(not_ours(aside));
        };
        let name = CString::new(name.as_bytes()).map_err(|_| not_ours(aside))?;
        let parent = Confined::open(parent).map_err(|e| failed("Opening", parent, e))?;
        match confined::remove_at(parent.root(), &name) {
            Ok(()) => self.catalog.remove_trash(aside),
            Err(e) => Err(failed("Deleting", aside, e)),
        }
    }

    /// Deletes what an interrupted pass moved aside: only directories recorded
    /// before their move, and only while they are still the directory that was
    /// checked.
    pub fn recover(&self) -> Vec<Failure> {
        let pending = match self.catalog.trash() {
            Ok(pending) => pending,
            Err(error) => return vec![error],
        };
        let mut problems = Vec::new();
        for (aside, recorded) in pending {
            let result = match identity(&aside) {
                Ok(Some(found)) if found == recorded => self.empty(&aside),
                Err(e) if e.kind() == io::ErrorKind::NotFound => self.catalog.remove_trash(&aside),
                // Never moved there, or something else is there now.
                _ => self.catalog.remove_trash(&aside),
            };
            if let Err(error) = result {
                problems.push(error);
            }
        }
        problems
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::housekeeping::clock::SystemClock;

    fn trash(root: &Path) -> Trash {
        let catalog =
            Arc::new(Catalog::open(&root.join("copies.sqlite"), Arc::new(SystemClock)).unwrap());
        Trash::new(catalog, root)
    }

    #[test]
    fn moves_aside_then_deletes_even_read_only_trees() {
        let dir = tempfile::tempdir().unwrap();
        let trash = trash(dir.path());
        let copy = dir.path().join("copy");
        std::fs::create_dir_all(copy.join("locked/inner")).unwrap();
        std::fs::write(copy.join("locked/inner/file"), "x").unwrap();
        let outside = dir.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("keep"), "keep").unwrap();
        std::os::unix::fs::symlink(&outside, copy.join("link")).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(copy.join("locked"), std::fs::Permissions::from_mode(0o500))
            .unwrap();
        let Moved::Aside(aside) = trash.move_aside(&copy, |p| p.is_dir()).unwrap() else {
            panic!("the copy was there");
        };
        assert!(!copy.exists());
        assert_eq!(trash.catalog.trash().unwrap()[0].0, aside);
        trash.empty(&aside).unwrap();
        assert!(!aside.exists());
        assert!(trash.catalog.trash().unwrap().is_empty());
        assert_eq!(
            std::fs::read_to_string(outside.join("keep")).unwrap(),
            "keep"
        );
        assert!(matches!(
            trash.move_aside(&copy, |_| true).unwrap(),
            Moved::Missing
        ));
    }

    #[test]
    fn something_unexpected_is_moved_back_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let trash = trash(dir.path());
        let target = dir.path().join("real");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("file"), "mine").unwrap();
        let link = dir.path().join("copy");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let refused = trash.move_aside(&link, |_| true);
        assert_eq!(refused.err().unwrap().code, "housekeeping_refused");
        assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(
            std::fs::read_to_string(target.join("file")).unwrap(),
            "mine"
        );
        let stranger = trash.move_aside(&target, |_| false);
        assert_eq!(stranger.err().unwrap().code, "housekeeping_refused");
        assert!(target.join("file").exists(), "checked before it is moved");
        assert!(trash.catalog.trash().unwrap().is_empty());
    }

    #[test]
    fn recovery_deletes_only_the_directory_it_moved() {
        let dir = tempfile::tempdir().unwrap();
        let trash = trash(dir.path());
        let copy = dir.path().join("copy");
        std::fs::create_dir(&copy).unwrap();
        let Moved::Aside(aside) = trash.move_aside(&copy, |_| true).unwrap() else {
            panic!();
        };
        // Something else now stands where the moved directory was.
        std::fs::remove_dir(&aside).unwrap();
        std::fs::create_dir(&aside).unwrap();
        std::fs::write(aside.join("precious"), "keep").unwrap();
        assert!(trash.recover().is_empty());
        assert!(aside.join("precious").exists());
        assert!(trash.catalog.trash().unwrap().is_empty());
    }

    #[test]
    fn an_interrupted_deletion_is_finished_later() {
        let dir = tempfile::tempdir().unwrap();
        let trash = trash(dir.path());
        let copy = dir.path().join("copy");
        std::fs::create_dir_all(copy.join("a")).unwrap();
        let Moved::Aside(aside) = trash.move_aside(&copy, |_| true).unwrap() else {
            panic!();
        };
        assert!(aside.exists());
        assert!(trash.recover().is_empty());
        assert!(!aside.exists());
        assert!(trash.catalog.trash().unwrap().is_empty());
    }
}
