//! A point-in-time copy of one open regular file: an APFS clone, which shares the
//! file's blocks until either side changes, or a plain copy where the volume
//! cannot clone.
use crate::workspace::confined::Confined;
use std::{
    ffi::CStr,
    fs::File,
    io,
    os::fd::{AsRawFd, OwnedFd},
};

/// From `<sys/clonefile.h>`: the clone belongs to the caller, not the source's owner.
const CLONE_NOOWNERCOPY: u32 = 0x0002;

/// Puts a copy of `source` at the new name `name` in `directory`, with the
/// permission bits `mode`.
pub fn clone_file(source: &File, directory: &OwnedFd, name: &CStr, mode: u32) -> io::Result<()> {
    // SAFETY: valid descriptors and C string.
    let cloned = unsafe {
        libc::fclonefileat(
            source.as_raw_fd(),
            directory.as_raw_fd(),
            name.as_ptr(),
            CLONE_NOOWNERCOPY,
        )
    };
    if cloned == 0 {
        return clear_flags(source, directory, name);
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::EEXIST) {
        return Err(error);
    }
    // ENOTSUP on volumes without clones, and anything else a copy might still get
    // past; a copy that fails too reports its own error.
    copy_file(source, directory, name, mode)
}

/// A clone keeps the source's file flags; one such as `uchg` would keep the clone
/// from ever being removed, so they are cleared.
fn clear_flags(source: &File, directory: &OwnedFd, name: &CStr) -> io::Result<()> {
    // SAFETY: an all-zero stat is a valid output buffer.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: valid descriptor and output buffer.
    if unsafe { libc::fstat(source.as_raw_fd(), &mut stat) } < 0 || stat.st_flags == 0 {
        return Ok(());
    }
    let clone = Confined::open_child_file(directory, name)?;
    // SAFETY: valid descriptor.
    if unsafe { libc::fchflags(clone.as_raw_fd(), 0) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// The fallback where cloning is not possible.
pub fn copy_file(source: &File, directory: &OwnedFd, name: &CStr, mode: u32) -> io::Result<()> {
    use std::{io::Write, os::unix::fs::FileExt};
    let mut target = Confined::create_at(directory, name)?;
    let copied = (|| {
        // From the start, whatever the descriptor's offset.
        let mut buffer = vec![0u8; 256 * 1024];
        let mut offset = 0u64;
        loop {
            let n = source.read_at(&mut buffer, offset)?;
            if n == 0 {
                break;
            }
            target.write_all(&buffer[..n])?;
            offset += n as u64;
        }
        // SAFETY: valid descriptor.
        if unsafe { libc::fchmod(target.as_raw_fd(), (mode & 0o777) as libc::mode_t) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    })();
    if copied.is_err() {
        // SAFETY: valid descriptor and C string; a partial copy is not kept.
        unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), 0) };
    }
    copied
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        ffi::CString,
        io::{Seek, Write},
        os::unix::fs::PermissionsExt,
    };

    fn directory(path: &std::path::Path) -> OwnedFd {
        File::open(path).unwrap().into()
    }

    #[test]
    fn a_clone_or_copy_keeps_the_content_and_mode_at_that_moment() {
        let temp = tempfile::tempdir().unwrap();
        let source_path = temp.path().join("source");
        std::fs::write(&source_path, b"at exit").unwrap();
        std::fs::set_permissions(&source_path, std::fs::Permissions::from_mode(0o751)).unwrap();
        let staging = temp.path().join("staging");
        std::fs::create_dir(&staging).unwrap();
        let target = directory(&staging);
        let mut source = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&source_path)
            .unwrap();

        clone_file(&source, &target, &CString::new("cloned").unwrap(), 0o751).unwrap();
        copy_file(&source, &target, &CString::new("copied").unwrap(), 0o751).unwrap();
        // Later changes to the source do not reach either.
        source.rewind().unwrap();
        source.write_all(b"changed").unwrap();
        std::fs::remove_file(&source_path).unwrap();
        for name in ["cloned", "copied"] {
            let path = staging.join(name);
            assert_eq!(std::fs::read(&path).unwrap(), b"at exit", "{name}");
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o751, "{name}");
        }
        let taken = clone_file(&source, &target, &CString::new("cloned").unwrap(), 0o644);
        assert_eq!(taken.unwrap_err().raw_os_error(), Some(libc::EEXIST));
    }

    #[test]
    fn a_clone_of_an_immutable_file_can_be_removed() {
        let temp = tempfile::tempdir().unwrap();
        let source_path = temp.path().join("locked");
        std::fs::write(&source_path, b"x").unwrap();
        let flag = |path: &std::path::Path, flags: u32| {
            let path = CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
            // SAFETY: valid C string.
            assert_eq!(unsafe { libc::chflags(path.as_ptr(), flags) }, 0);
        };
        flag(&source_path, libc::UF_IMMUTABLE);
        let staging = temp.path().join("staging");
        std::fs::create_dir(&staging).unwrap();
        let source = File::open(&source_path).unwrap();
        clone_file(
            &source,
            &directory(&staging),
            &CString::new("clone").unwrap(),
            0o644,
        )
        .unwrap();
        std::fs::remove_file(staging.join("clone")).unwrap();
        flag(&source_path, 0);
    }
}
