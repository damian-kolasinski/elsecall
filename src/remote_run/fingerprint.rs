//! Taking consistency fingerprints: running a toolchain's identifying command, and
//! hashing a configuration file or directory, under a salt. Each is bounded in
//! time and size, so a declaration cannot hang the side that takes them.
use crate::{
    consumer_configuration::ConfigurationLocation,
    remote_run::consistency::{Declared, Fingerprints},
    workspace::digest::{Digest, Hasher},
};
use std::{
    ffi::OsString,
    io::Read,
    os::unix::{fs::OpenOptionsExt, process::CommandExt},
    path::Path,
    process::{Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
const MAXIMUM_PROBE_OUTPUT: u64 = 1 << 20;
const MAXIMUM_LOCATION_ENTRIES: usize = 10_000;
const MAXIMUM_LOCATION_BYTES: u64 = 64 << 20;

fn keyed(salt: &[u8], kind: &str) -> Hasher {
    let mut hasher = Hasher::default();
    hasher.update(salt);
    hasher.update(kind.as_bytes());
    hasher.update(&[0]);
    hasher
}

/// Runs a toolchain's identifying command in its own process group and digests
/// what it printed; the group is killed if it outlives the timeout.
fn probe(
    salt: &[u8],
    command: &[String],
    environment: Option<&[(OsString, OsString)]>,
    directory: &Path,
) -> Digest {
    let mut hasher = keyed(salt, "toolchain");
    let mut child = Command::new(&command[0]);
    child
        .args(&command[1..])
        .current_dir(directory)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    if let Some(environment) = environment {
        child
            .env_clear()
            .envs(environment.iter().map(|(k, v)| (k, v)));
    }
    let Ok(mut child) = crate::child_start::spawn(&mut child) else {
        hasher.update(b"missing");
        return hasher.finish();
    };
    let group = child.id() as i32;
    let (sender, outputs) = mpsc::channel();
    for (index, stream) in [
        child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>),
        child
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>),
    ]
    .into_iter()
    .enumerate()
    {
        let sender = sender.clone();
        let Some(mut stream) = stream else { continue };
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = (&mut stream)
                .take(MAXIMUM_PROBE_OUTPUT)
                .read_to_end(&mut bytes);
            let _ = sender.send((index, bytes));
        });
    }
    drop(sender);
    let deadline = Instant::now() + PROBE_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => break None,
        }
    };
    let mut printed = [Vec::new(), Vec::new()];
    if status.is_some() {
        // Output that a background process keeps open is read until the deadline.
        for _ in 0..2 {
            let left = deadline.saturating_duration_since(Instant::now());
            match outputs.recv_timeout(left) {
                Ok((index, bytes)) => printed[index] = bytes,
                Err(_) => break,
            }
        }
    }
    // SAFETY: kill has no memory-safety preconditions; the probe leads its group.
    unsafe { libc::kill(-group, libc::SIGKILL) };
    let _ = child.wait();
    match status {
        None => hasher.update(b"timed out"),
        Some(status) => {
            hasher.update(format!("{:?}", status.code()).as_bytes());
            hasher.update(&printed[0]);
            hasher.update(&[0]);
            hasher.update(&printed[1]);
        }
    }
    hasher.finish()
}

/// Digests a configuration file or directory: names, kinds and contents. Links
/// are followed, as configuration often lives in a linked dotfiles directory, but
/// only regular files are read, without blocking.
fn location(salt: &[u8], path: &Path) -> Digest {
    struct Walk {
        hasher: Hasher,
        entries: usize,
        bytes: u64,
    }
    fn visit(path: &Path, relative: &str, walk: &mut Walk) {
        walk.entries += 1;
        if walk.entries > MAXIMUM_LOCATION_ENTRIES || walk.bytes > MAXIMUM_LOCATION_BYTES {
            walk.hasher.update(b"too large");
            return;
        }
        walk.hasher.update(relative.as_bytes());
        walk.hasher.update(&[0]);
        match std::fs::metadata(path) {
            Err(_) => walk.hasher.update(b"absent"),
            Ok(meta) if meta.is_dir() => {
                walk.hasher.update(b"directory");
                let mut names: Vec<_> = std::fs::read_dir(path)
                    .map(|dir| dir.filter_map(|e| e.ok()).map(|e| e.file_name()).collect())
                    .unwrap_or_default();
                names.sort();
                for name in names {
                    let child = format!("{relative}/{}", name.to_string_lossy());
                    visit(&path.join(&name), &child, walk);
                }
            }
            Ok(meta) if meta.is_file() => {
                walk.hasher.update(b"file");
                let opened = std::fs::OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NONBLOCK)
                    .open(path)
                    .ok()
                    .filter(|file| file.metadata().is_ok_and(|m| m.is_file()));
                match opened {
                    Some(file) => {
                        let allowed = MAXIMUM_LOCATION_BYTES.saturating_sub(walk.bytes);
                        match Digest::of_reader(file.take(allowed)) {
                            Ok((digest, read)) => {
                                walk.bytes += read;
                                walk.hasher.update(&digest.0);
                            }
                            Err(_) => walk.hasher.update(b"unreadable"),
                        }
                    }
                    None => walk.hasher.update(b"unreadable"),
                }
            }
            Ok(_) => walk.hasher.update(b"special"),
        }
        walk.hasher.update(&[0]);
    }
    let mut walk = Walk {
        hasher: keyed(salt, "location"),
        entries: 0,
        bytes: 0,
    };
    visit(path, "", &mut walk);
    walk.hasher.finish()
}

/// Fingerprints what `declared` names, as this account sees it: commands run in
/// `directory` with `environment` (the process's own when `None`), and `~/`
/// resolved against `home`.
pub fn take(
    declared: &Declared,
    locations: &[ConfigurationLocation],
    salt: &[u8],
    home: &Path,
    environment: Option<&[(OsString, OsString)]>,
    directory: &Path,
) -> Fingerprints {
    Fingerprints {
        toolchains: declared
            .toolchains
            .iter()
            .map(|(name, command)| (name.clone(), probe(salt, command, environment, directory)))
            .collect(),
        locations: locations
            .iter()
            .map(|l| (l.display(), location(salt, &l.resolve(home))))
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote_run::consistency::{compare, salt};
    use std::collections::BTreeMap;

    #[test]
    fn salted_fingerprints_match_only_equal_things_and_never_hang() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join("config"), "same").unwrap();
        let declared = Declared {
            toolchains: BTreeMap::from([
                (
                    "echo".to_owned(),
                    vec!["/bin/echo".to_owned(), "v1".to_owned()],
                ),
                // A background process holding the output open must not hang it.
                (
                    "lingering".to_owned(),
                    vec![
                        "/bin/sh".to_owned(),
                        "-c".to_owned(),
                        "echo $$ > \"$1/pid\"; (while [ -d \"$1\" ] && [ ! -f \"$1/release\" ]; do sleep 0.02; done) & echo v".to_owned(),
                        "sh".to_owned(),
                        home.path().to_string_lossy().into_owned(),
                    ],
                ),
            ]),
            locations: vec!["~/config".into(), "~/missing".into(), "/dev/zero".into()],
        };
        let locations = declared.validate().unwrap();
        let salt = salt().unwrap();
        let (sent, received) = std::sync::mpsc::channel();
        let (worker_declared, worker_locations, worker_salt, root) = (
            declared.clone(),
            locations.clone(),
            salt.clone(),
            home.path().to_owned(),
        );
        std::thread::spawn(move || {
            let _ = sent.send(take(
                &worker_declared,
                &worker_locations,
                &worker_salt,
                &root,
                None,
                &root,
            ));
        });
        let a = received
            .recv_timeout(Duration::from_secs(120))
            .expect("a blocked output pipe cannot hang the probe");
        let group: u32 = std::fs::read_to_string(home.path().join("pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(120);
        while !crate::macos::process_group::members(group).is_empty() {
            assert!(
                Instant::now() < deadline,
                "the timed-out probe's descendants were not stopped"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        let b = take(&declared, &locations, &salt, home.path(), None, home.path());
        assert!(compare(&declared, &a, &b).is_empty());
        std::fs::write(home.path().join("config"), "different").unwrap();
        let c = take(&declared, &locations, &salt, home.path(), None, home.path());
        assert_eq!(compare(&declared, &a, &c).locations, ["~/config"]);
        let d = take(
            &declared,
            &locations,
            &super::super::consistency::salt().unwrap(),
            home.path(),
            None,
            home.path(),
        );
        assert_ne!(
            c.locations["~/missing"], d.locations["~/missing"],
            "another salt, other fingerprints"
        );
    }
}
