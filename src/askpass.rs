//! Giving OpenSSH a password without putting it in an argument, the environment
//! or a file.
//!
//! The command that holds the password serves it on a Unix socket in a private
//! directory it created. OpenSSH runs this executable as its `SSH_ASKPASS`
//! program, with the socket's path in `ELSECALL_ASKPASS_SOCKET`; that process
//! reads the password from the socket and prints it for OpenSSH.
use elsecall_failure::{Failure, Result};
use std::{
    io::{Read, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
};
use tokio::{io::AsyncWriteExt, net::UnixListener, task::JoinHandle};
use zeroize::Zeroizing;

/// Names the socket for the askpass process; it holds a path, never the password.
pub const SOCKET_VARIABLE: &str = "ELSECALL_ASKPASS_SOCKET";
/// OpenSSH asks once per attempt; a few attempts cover keyboard-interactive and
/// password prompts on one connection.
const MOST_ANSWERS: usize = 3;
const PASSWORD_LIMIT: u64 = 4096;

/// Serves `password` at `socket` to processes of this user, until dropped.
pub struct Server {
    socket: PathBuf,
    task: JoinHandle<()>,
}

impl Server {
    /// `socket` must be in a directory only this user can enter.
    pub fn start(socket: &Path, password: Zeroizing<String>) -> Result<Self> {
        let listener = UnixListener::bind(socket)
            .map_err(|error| Failure::io("Prepare the password for SSH", error))?;
        let task = tokio::spawn(async move {
            for _ in 0..MOST_ANSWERS {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                if !same_user(&stream) {
                    continue;
                }
                let _ = stream.write_all(password.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
        });
        Ok(Self {
            socket: socket.to_owned(),
            task,
        })
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_file(&self.socket);
    }
}

fn same_user(stream: &tokio::net::UnixStream) -> bool {
    stream
        .peer_cred()
        .is_ok_and(|peer| peer.uid() == unsafe { libc::getuid() })
}

/// Runs as OpenSSH's askpass program when `ELSECALL_ASKPASS_SOCKET` is set: prints
/// the password the socket serves. Returns the process's exit status.
pub fn answer(socket: &Path) -> u8 {
    let mut password = Zeroizing::new(Vec::new());
    let read = UnixStream::connect(socket)
        .and_then(|stream| stream.take(PASSWORD_LIMIT).read_to_end(&mut password));
    if read.is_err() || password.is_empty() {
        return 1;
    }
    let mut stdout = std::io::stdout();
    if stdout
        .write_all(&password)
        .and_then(|()| stdout.write_all(b"\n"))
        .and_then(|()| stdout.flush())
        .is_err()
    {
        return 1;
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_password_is_served_a_few_times_then_no_more() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("p");
        let server = Server::start(&socket, Zeroizing::new("s3cret pass".into())).unwrap();
        for _ in 0..MOST_ANSWERS {
            let socket = socket.clone();
            let mut read = String::new();
            tokio::task::spawn_blocking(move || {
                UnixStream::connect(socket)
                    .unwrap()
                    .read_to_string(&mut read)
                    .unwrap();
                read
            })
            .await
            .map(|read| assert_eq!(read, "s3cret pass"))
            .unwrap();
        }
        drop(server);
        assert!(!socket.exists());
    }
}
