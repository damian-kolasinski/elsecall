//! The git command-line tool, the only adapter for git repositories.
//!
//! On the caller, git runs with the caller's own configuration, because the
//! caller's view of its working tree is the source of truth, but without `GIT_*`
//! variables and without optional locks, so Elsecall never takes the caller's index
//! lock or follows a redirected repository. On a member, git also ignores the
//! account's global and system configuration, so copies behave the same everywhere.
use crate::{failure::Result, workspace::sync_failed};
use serde::{Deserialize, Serialize};
use std::{
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObjectFormat {
    Sha1,
    Sha256,
}
impl ObjectFormat {
    pub fn hex_length(self) -> usize {
        match self {
            ObjectFormat::Sha1 => 40,
            ObjectFormat::Sha256 => 64,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            ObjectFormat::Sha1 => "sha1",
            ObjectFormat::Sha256 => "sha256",
        }
    }
    pub fn zero(self) -> Oid {
        Oid("0".repeat(self.hex_length()))
    }
}

/// A git object name: lowercase hex of the repository's hash length.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Oid(String);
impl Oid {
    pub fn parse(text: &str) -> Option<Self> {
        (matches!(text.len(), 40 | 64)
            && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
        .then(|| Self(text.to_owned()))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
    pub fn fits(&self, format: ObjectFormat) -> bool {
        self.0.len() == format.hex_length()
    }
}
impl TryFrom<String> for Oid {
    type Error = String;
    fn try_from(text: String) -> std::result::Result<Self, String> {
        Oid::parse(&text).ok_or_else(|| "invalid object name".into())
    }
}
impl From<Oid> for String {
    fn from(oid: Oid) -> Self {
        oid.0
    }
}
impl std::fmt::Display for Oid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Side {
    Caller,
    Member,
}

/// git, run in one directory.
#[derive(Clone, Debug)]
pub struct Git {
    directory: PathBuf,
    side: Side,
}

fn program(side: Side) -> &'static str {
    // A member's service has launchd's PATH; the command-line tools' git is fixed.
    match side {
        Side::Member if Path::new("/usr/bin/git").exists() => "/usr/bin/git",
        _ => "git",
    }
}

impl Git {
    /// git as the caller sees its own working tree.
    pub fn caller(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
            side: Side::Caller,
        }
    }
    /// git for a member's copies and object stores.
    pub fn member(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
            side: Side::Member,
        }
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    fn configure(&self, command: &mut Command) {
        for (name, _) in std::env::vars_os() {
            if name.as_encoded_bytes().starts_with(b"GIT_") {
                command.env_remove(&name);
            }
        }
        command
            .current_dir(&self.directory)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("LC_ALL", "C");
        if self.side == Side::Member {
            // A run may edit the copy's configuration, and an object source's is
            // the operator's; nothing in either may make Elsecall's own git run
            // programs or reach beyond this machine.
            command
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_NO_LAZY_FETCH", "1")
                .env("GIT_CONFIG_COUNT", "6")
                .env("GIT_CONFIG_KEY_0", "core.fsmonitor")
                .env("GIT_CONFIG_VALUE_0", "false")
                .env("GIT_CONFIG_KEY_1", "core.hooksPath")
                .env("GIT_CONFIG_VALUE_1", "/dev/null")
                .env("GIT_CONFIG_KEY_2", "core.sshCommand")
                .env("GIT_CONFIG_VALUE_2", "false")
                .env("GIT_CONFIG_KEY_3", "protocol.allow")
                .env("GIT_CONFIG_VALUE_3", "never")
                .env("GIT_CONFIG_KEY_4", "protocol.file.allow")
                .env("GIT_CONFIG_VALUE_4", "always")
                .env("GIT_CONFIG_KEY_5", "credential.helper")
                .env("GIT_CONFIG_VALUE_5", "");
        }
    }

    pub fn command<I, S>(&self, args: I) -> Command
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        let mut command = Command::new(program(self.side));
        self.configure(&mut command);
        command.args(args).stdin(Stdio::null());
        command
    }

    /// The same command for asynchronous streaming, as of packs.
    pub fn streaming<I, S>(&self, args: I) -> tokio::process::Command
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        let command = self.command(args);
        let mut command = tokio::process::Command::from(command);
        command.kill_on_drop(true);
        command
    }

    /// Runs git and returns its standard output, or `None` if it failed.
    pub fn try_output<S: AsRef<std::ffi::OsStr>>(&self, args: &[S]) -> Option<Vec<u8>> {
        let output = crate::child_start::output(&mut self.command(args)).ok()?;
        output.status.success().then_some(output.stdout)
    }

    /// Runs git and returns its standard output; failure is a sync failure.
    pub fn output<S: AsRef<std::ffi::OsStr>>(&self, args: &[S]) -> Result<Vec<u8>> {
        self.output_with(args, None)
    }

    /// Runs git with `input` on its standard input.
    pub fn output_with<S: AsRef<std::ffi::OsStr>>(
        &self,
        args: &[S],
        input: Option<&[u8]>,
    ) -> Result<Vec<u8>> {
        let name = args
            .first()
            .map(|a| a.as_ref().to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut command = self.command(args);
        command
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = crate::child_start::spawn(&mut command)
            .map_err(|e| sync_failed(format!("git could not be started: {e}.")))?;
        let writer = input.map(|input| {
            let mut stdin = child.stdin.take().expect("piped");
            let input = input.to_vec();
            std::thread::spawn(move || {
                let _ = stdin.write_all(&input);
            })
        });
        let output = child
            .wait_with_output()
            .map_err(|e| sync_failed(format!("git {name} failed: {e}.")))?;
        if let Some(writer) = writer {
            let _ = writer.join();
        }
        if !output.status.success() {
            // git's message names paths, never content; keep only its first line.
            let detail = String::from_utf8_lossy(&output.stderr);
            let detail = detail.lines().next().unwrap_or("").trim();
            return Err(sync_failed(format!("git {name} failed: {detail}")));
        }
        Ok(output.stdout)
    }

    /// Runs git and returns its output as one trimmed UTF-8 line.
    pub fn line<S: AsRef<std::ffi::OsStr>>(&self, args: &[S]) -> Result<String> {
        let output = self.output(args)?;
        String::from_utf8(output)
            .map(|text| text.trim_end_matches('\n').to_owned())
            .map_err(|_| sync_failed("git printed a name that is not UTF-8."))
    }

    /// Which of `oids` exist in this repository, including through alternates.
    pub fn existing(&self, oids: &[Oid]) -> Result<Vec<bool>> {
        if oids.is_empty() {
            return Ok(Vec::new());
        }
        let mut input = String::with_capacity(oids.len() * 65);
        for oid in oids {
            input.push_str(oid.as_str());
            input.push('\n');
        }
        let output = self.output_with(
            &["cat-file", "--batch-check=%(objectname)"],
            Some(input.as_bytes()),
        )?;
        let text = String::from_utf8_lossy(&output);
        let lines: Vec<&str> = text.lines().collect();
        if lines.len() != oids.len() {
            return Err(sync_failed("git cat-file returned an unexpected answer."));
        }
        Ok(lines
            .iter()
            .map(|line| !line.ends_with(" missing"))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_object_names() {
        assert!(Oid::parse(&"a".repeat(40)).is_some());
        assert!(Oid::parse(&"0".repeat(64)).is_some());
        assert!(Oid::parse(&"A".repeat(40)).is_none());
        assert!(Oid::parse("HEAD").is_none());
        assert!(Oid::parse(&"a".repeat(41)).is_none());
        assert!(ObjectFormat::Sha1.zero().fits(ObjectFormat::Sha1));
    }
}
