//! How much space the volume holding copies has: a port, since tests cannot fill a
//! real disk to its threshold.
use std::{
    ffi::CString,
    io,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Space {
    pub total_bytes: u64,
    /// What an unprivileged process can still write.
    pub available_bytes: u64,
}

pub trait Volumes: Send + Sync {
    fn space(&self, path: &Path) -> io::Result<Space>;
}

/// The file system's own figures.
pub struct Statfs;
impl Volumes for Statfs {
    fn space(&self, path: &Path) -> io::Result<Space> {
        let path = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        // SAFETY: statfs is plain data; zero is a valid initial value.
        let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
        // SAFETY: valid C string and a writable statfs buffer.
        if unsafe { libc::statfs(path.as_ptr(), &mut stat) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let block = stat.f_bsize as u64;
        Ok(Space {
            total_bytes: stat.f_blocks.saturating_mul(block),
            available_bytes: stat.f_bavail.saturating_mul(block),
        })
    }
}

/// A volume of a fixed size whose only changing content is the workspace root, for
/// services that tests start. The file holds
/// `{"total_bytes": N, "used_elsewhere_bytes": M}`; what is available is the total
/// minus that and minus what the workspace root holds now, so removing copies
/// frees space as on a real disk.
pub struct SimulatedVolume {
    pub file: PathBuf,
    pub root: PathBuf,
}
impl Volumes for SimulatedVolume {
    fn space(&self, _: &Path) -> io::Result<Space> {
        #[derive(serde::Deserialize)]
        struct Simulated {
            total_bytes: u64,
            used_elsewhere_bytes: u64,
        }
        let simulated: Simulated = serde_json::from_slice(&std::fs::read(&self.file)?)
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?;
        let held = super::measure::allocated(&self.root)?;
        Ok(Space {
            total_bytes: simulated.total_bytes,
            available_bytes: simulated
                .total_bytes
                .saturating_sub(simulated.used_elsewhere_bytes)
                .saturating_sub(held),
        })
    }
}

/// The real volumes, unless `ELSECALL_SIMULATED_VOLUME` names a simulated one.
pub fn from_environment(root: &Path) -> Arc<dyn Volumes> {
    match std::env::var_os("ELSECALL_SIMULATED_VOLUME") {
        Some(file) if !file.is_empty() => {
            crate::service_log!(
                "{}",
                serde_json::json!({"event": "test_seam_in_use", "detail": "ELSECALL_SIMULATED_VOLUME replaces the free space housekeeping sees"})
            );
            Arc::new(SimulatedVolume {
                file: file.into(),
                root: root.to_owned(),
            })
        }
        _ => Arc::new(Statfs),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statfs_reports_a_plausible_volume() {
        let space = Statfs.space(Path::new("/tmp")).unwrap();
        assert!(space.total_bytes > 0);
        assert!(space.available_bytes <= space.total_bytes);
        assert!(Statfs.space(Path::new("/no/such/place")).is_err());
    }

    #[test]
    fn a_simulated_volume_frees_what_the_root_releases() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        std::fs::create_dir(&root).unwrap();
        let file = dir.path().join("volume.json");
        std::fs::write(
            &file,
            r#"{"total_bytes": 10000000, "used_elsewhere_bytes": 1000000}"#,
        )
        .unwrap();
        let volume = SimulatedVolume {
            file,
            root: root.clone(),
        };
        let empty = volume.space(&root).unwrap();
        std::fs::write(root.join("blob"), vec![1u8; 100_000]).unwrap();
        let full = volume.space(&root).unwrap();
        assert!(empty.available_bytes - full.available_bytes >= 100_000);
        std::fs::remove_file(root.join("blob")).unwrap();
        assert_eq!(volume.space(&root).unwrap(), empty);
    }
}
