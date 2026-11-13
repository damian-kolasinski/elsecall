//! The optional status file: the run's outcome as JSON, so a caller can tell a
//! command that exited 125 from Elsecall's own failure.
use crate::{
    api,
    failure::{Failure, Result},
    passthrough::executor::Exit,
    run_admission::Admission,
    scheduling::run_id::RunId,
};
use serde::Serialize;
use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

/// Which run the status is about. Without admission there is no run ID or member.
#[derive(Clone, Copy, Debug)]
pub struct Subject<'a> {
    pub run_id: Option<&'a RunId>,
    pub member: Option<&'a str>,
    pub admission: Admission,
}
impl Subject<'_> {
    pub fn unadmitted(admission: Admission) -> Self {
        Self {
            run_id: None,
            member: None,
            admission,
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Status<'a> {
    /// Waiting for capacity.
    Waiting,
    Running {
        run_id: Option<&'a RunId>,
        member: Option<&'a str>,
        admission: Admission,
        /// Consistency warnings of a placed run; never part of the command's output.
        #[serde(skip_serializing_if = "<[String]>::is_empty")]
        warnings: &'a [String],
    },
    /// The command exited with `exit_code`.
    Exited {
        run_id: Option<&'a RunId>,
        member: Option<&'a str>,
        admission: Admission,
        exit_code: i32,
        #[serde(skip_serializing_if = "<[String]>::is_empty")]
        warnings: &'a [String],
    },
    /// The command was ended by `signal`.
    Signaled {
        run_id: Option<&'a RunId>,
        member: Option<&'a str>,
        admission: Admission,
        signal: i32,
        #[serde(skip_serializing_if = "<[String]>::is_empty")]
        warnings: &'a [String],
    },
}
impl<'a> Status<'a> {
    pub fn running(subject: Subject<'a>, warnings: &'a [String]) -> Self {
        Status::Running {
            run_id: subject.run_id,
            member: subject.member,
            admission: subject.admission,
            warnings,
        }
    }
    pub fn ended(subject: Subject<'a>, exit: Exit, warnings: &'a [String]) -> Self {
        let Subject {
            run_id,
            member,
            admission,
        } = subject;
        match exit {
            Exit::Code(exit_code) => Status::Exited {
                run_id,
                member,
                admission,
                exit_code,
                warnings,
            },
            Exit::Signal(signal) => Status::Signaled {
                run_id,
                member,
                admission,
                signal,
                warnings,
            },
        }
    }
}

pub struct StatusFile(PathBuf);

impl StatusFile {
    /// Writes the waiting state, so a path that cannot be written fails before the
    /// command runs.
    pub fn create(path: PathBuf) -> Result<Self> {
        let file = Self(path);
        file.write(&api::Response::new(Status::Waiting))?;
        Ok(file)
    }

    /// A status file to report a failure in, without writing the waiting state.
    pub fn at(path: PathBuf) -> Self {
        Self(path)
    }

    pub fn update(&self, status: Status) -> Result<()> {
        self.write(&api::Response::new(status))
    }

    /// Records one of Elsecall's own failures, as the last stderr line does.
    pub fn fail(&self, failure: &Failure) -> Result<()> {
        self.write(&super::envelope(failure))
    }

    /// Replaces the file atomically, so a reader never sees a partial document.
    fn write(&self, document: &impl Serialize) -> Result<()> {
        let failed = |e| Failure::io("Write the status file", e);
        let directory = self.0.parent().filter(|p| !p.as_os_str().is_empty());
        let directory = directory.unwrap_or(Path::new("."));
        let name = self.0.file_name().ok_or_else(|| {
            Failure::new(
                "invalid_request",
                "--status-file must name a file.",
                "Pass a file path such as /tmp/run-status.json.",
            )
        })?;
        let mut temporary = directory.join(".");
        temporary.as_mut_os_string().push(name);
        temporary
            .as_mut_os_string()
            .push(format!(".elsecall-{}", std::process::id()));
        let result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o666)
                .open(&temporary)?;
            serde_json::to_writer(&mut file, document)?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            fs::rename(&temporary, &self.0)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result.map_err(failed)
    }
}
