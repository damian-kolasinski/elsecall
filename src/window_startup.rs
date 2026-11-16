//! Private launch acknowledgment, sent only after the native window is ready.
use crate::failure::{Failure, Result};
use std::{
    io::{Read, Write},
    os::unix::{
        fs::PermissionsExt,
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

pub const READY_ARGUMENT: &str = "--elsecall-ready";
const ACK: &[u8; 5] = b"ready";

pub struct WindowStartup {
    _directory: tempfile::TempDir,
    listener: UnixListener,
    socket: PathBuf,
}
impl WindowStartup {
    pub fn new() -> Result<Self> {
        // Short path for macOS's socket limit; tempfile creates the directory as 0700.
        let directory = tempfile::Builder::new()
            .prefix("ec-ready-")
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir_in("/tmp")
            .map_err(|error| Failure::io("Prepare window startup", error))?;
        let socket = directory.path().join("ready.sock");
        let listener = UnixListener::bind(&socket)
            .map_err(|error| Failure::io("Listen for window startup", error))?;
        listener
            .set_nonblocking(true)
            .map_err(|error| Failure::io("Configure window startup", error))?;
        Ok(Self {
            _directory: directory,
            listener,
            socket,
        })
    }
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    pub fn wait(&self, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(not_ready());
            }
            match self.listener.accept() {
                Ok((mut connection, _)) => {
                    connection
                        .set_nonblocking(true)
                        .map_err(|error| Failure::io("Wait for window startup", error))?;
                    let mut ack = [0; 5];
                    let mut received = 0;
                    while received < ack.len() {
                        match connection.read(&mut ack[received..]) {
                            Ok(0) => return Err(not_ready()),
                            Ok(count) => received += count,
                            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                                let remaining = deadline.saturating_duration_since(Instant::now());
                                if remaining.is_zero() {
                                    return Err(not_ready());
                                }
                                std::thread::sleep(remaining.min(Duration::from_millis(20)));
                            }
                            Err(_) => return Err(not_ready()),
                        }
                    }
                    if &ack != ACK {
                        return Err(not_ready());
                    }
                    // A late UI also needs acceptance, otherwise it must quit after rollback.
                    return connection.write_all(b"go").map_err(|_| not_ready());
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(remaining.min(Duration::from_millis(20)));
                }
                Err(error) => return Err(Failure::io("Wait for window startup", error)),
            }
        }
    }
}

pub fn acknowledge(socket: &Path) -> std::io::Result<()> {
    let mut connection = UnixStream::connect(socket)?;
    connection.write_all(ACK)?;
    let mut accepted = [0; 2];
    connection.read_exact(&mut accepted)?;
    if &accepted == b"go" {
        Ok(())
    } else {
        Err(std::io::Error::other("window launch was not accepted"))
    }
}
fn not_ready() -> Failure {
    Failure::new(
        "window_open_failed",
        "The updated window did not become ready.",
        "The previous app was preserved. Retry the upgrade from a macOS desktop session.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn startup_requires_an_acknowledgment_and_cleans_up_after_a_failure() {
        let startup = WindowStartup::new().unwrap();
        assert_eq!(
            std::fs::metadata(startup.socket().parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        let socket = startup.socket().to_owned();
        let window = std::thread::spawn(move || acknowledge(&socket));
        startup.wait(Duration::from_secs(1)).unwrap();
        window.join().unwrap().unwrap();
        let startup = WindowStartup::new().unwrap();
        assert_eq!(
            startup.wait(Duration::from_millis(5)).unwrap_err().code,
            "window_open_failed"
        );
        let socket = startup.socket().to_owned();
        drop(startup);
        assert!(acknowledge(&socket).is_err()); // A late window cannot remain open after rollback.
    }
    #[test]
    fn an_incomplete_startup_message_is_not_ready() {
        let startup = WindowStartup::new().unwrap();
        UnixStream::connect(startup.socket())
            .unwrap()
            .write_all(b"bad")
            .unwrap();
        assert!(startup.wait(Duration::from_secs(1)).is_err());
    }
}
