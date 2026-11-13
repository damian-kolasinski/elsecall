//! A process gate whose release remains observable even after fixture teardown.
use std::path::{Path, PathBuf};

pub struct FileGate(PathBuf);
impl FileGate {
    /// The child waits while this marker exists. Absence is a permanent release,
    /// so deleting a temporary home cannot strand a process during unwinding.
    pub fn new(home: &Path, name: &str) -> Self {
        let path = home.join(name);
        std::fs::File::create_new(&path).unwrap();
        Self(path)
    }
    pub fn open(&self) {
        match std::fs::remove_file(&self.0) {
            Ok(()) => (),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => panic!("release process gate: {e}"),
        }
    }
}
impl Drop for FileGate {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        process::Stdio,
        time::{Duration, Instant},
    };

    #[test]
    fn explicit_release_unwind_and_home_removal_all_release_a_real_process() {
        for mode in 0..3 {
            let mut home = Some(tempfile::tempdir().unwrap());
            let path = home.as_ref().unwrap().path().to_owned();
            let mut gate = Some(FileGate::new(&path, "wait"));
            let mut child = crate::processes::command("/bin/sh")
                .args([
                    "-c",
                    "touch \"$2\"; while [ -f \"$1\" ]; do sleep 0.02; done",
                    "sh",
                ])
                .arg(path.join("wait"))
                .arg(path.join("ready"))
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            let deadline = Instant::now() + crate::processes::WAIT;
            while !path.join("ready").exists() {
                assert!(Instant::now() < deadline, "gate process never became ready");
                std::thread::sleep(Duration::from_millis(20));
            }
            assert!(
                child.try_wait().unwrap().is_none(),
                "closed gate holds the process"
            );
            match mode {
                0 => gate.as_ref().unwrap().open(),
                1 => drop(gate.take()),
                _ => drop(home.take()),
            }
            loop {
                if let Some(status) = child.try_wait().unwrap() {
                    assert!(status.success(), "{status}");
                    break;
                }
                assert!(Instant::now() < deadline, "released process did not exit");
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}
