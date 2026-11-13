//! A run's own area for the snapshot of its outputs, inside a directory that holds
//! only such areas. It exists from before the command starts until the outputs
//! are returned, and is removed when dropped.
use crate::workspace::confined::{Confined, remove_at};
use std::{ffi::CString, io, os::fd::AsRawFd};

pub struct OutputStaging {
    parent: Confined,
    name: CString,
    area: Confined,
}

impl OutputStaging {
    /// A fresh area named `name` in `parent`. Areas left there by runs that ended
    /// abruptly are removed first: a copy serves one run at a time.
    pub fn create(parent: Confined, name: &str) -> io::Result<Self> {
        let name = CString::new(name).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        for left in parent.list(None)?.unwrap_or_default() {
            let _ = remove_at(parent.root(), &left);
        }
        // SAFETY: valid descriptor and C string.
        if unsafe { libc::mkdirat(parent.root().as_raw_fd(), name.as_ptr(), 0o700) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let area = Confined::open_at(parent.root(), &name)?;
        Ok(Self { parent, name, area })
    }

    /// The area, laid out like the workspace.
    pub fn area(&self) -> &Confined {
        &self.area
    }
}

impl Drop for OutputStaging {
    fn drop(&mut self) {
        let _ = remove_at(self.parent.root(), &self.name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_area_clears_ones_left_by_abrupt_ends_and_goes_when_dropped() {
        let temp = tempfile::tempdir().unwrap();
        let areas = temp.path();
        std::fs::create_dir_all(areas.join("r-0/out")).unwrap();
        std::fs::write(areas.join("r-0/out/x"), "x").unwrap();
        let staging = OutputStaging::create(Confined::open(areas).unwrap(), "r-1").unwrap();
        let names: Vec<_> = std::fs::read_dir(areas)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["r-1"]);
        drop(staging);
        assert_eq!(std::fs::read_dir(areas).unwrap().count(), 0);
    }
}
