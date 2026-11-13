//! Normal run-client execution, private process groups and result collection.
use super::{
    invalid,
    protocol::{Job, Launch},
};
use crate::{
    failure::{Failure, Result},
    paths::Paths,
    scheduling::{
        processes::ProcessIdentity,
        request::{AtDeadline, Placement, Urgency},
        timestamp::Timestamp,
    },
};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::process::{Child, Command};
pub struct Caller {
    child: Child,
    original_identity: Option<ProcessIdentity>,
    status: PathBuf,
    index: usize,
    exit_code: Option<i32>,
    run_id: Option<String>,
}
impl Caller {
    pub fn start(
        paths: &Paths,
        root: &Path,
        seed: u64,
        until: Timestamp,
        request: Launch,
    ) -> Result<Self> {
        let directory = root.join(&request.workspace);
        if !directory.is_dir() {
            return Err(invalid("The synthetic workspace was not prepared."));
        }
        let status = root.join(format!("{}.run-id", request.index));
        let mut command = Command::new(
            std::env::current_exe().map_err(|e| Failure::io("Find rehearsal executable", e))?,
        );
        command
            .current_dir(&directory)
            .env("HOME", &paths.home)
            .env("ELSECALL", "on")
            .env("ELSECALL_WHEN_UNAVAILABLE", "fail");
        // Do not inherit a consumer's outer grant, output files or flags.
        for (name, _) in std::env::vars_os() {
            if name.as_encoded_bytes().starts_with(b"ELSECALL_")
                && !matches!(
                    name.to_str(),
                    Some(
                        "ELSECALL_TAILNET_NAME"
                            | "ELSECALL_TAILNET_ADDRESSES"
                            | "ELSECALL_FLEET_PORT"
                    )
                )
            {
                command.env_remove(name);
            }
        }
        command.env("ELSECALL_WHEN_UNAVAILABLE", "fail");
        command
            .arg("run")
            .arg("--label")
            .arg(&request.label)
            .arg("--size")
            .arg(format!("{:.3}", f64::from(request.size.millis()) / 1000.0))
            .arg("--run-id-file")
            .arg(&status);
        match request.placement {
            Placement::Pinned => {
                command.arg("--pinned");
            }
            Placement::Placeable => {
                command.arg("--placeable");
            }
            Placement::Member(member) => {
                command.arg("--member").arg(member);
            }
        }
        match request.urgency {
            Urgency::Interactive { .. } => {
                command
                    .arg("--interactive")
                    .arg("--hold-limit")
                    .arg(format!("{}ms", request.hold_ms.unwrap()));
            }
            Urgency::Deferrable {
                deadline,
                at_deadline,
            } => {
                command
                    .arg("--deferrable")
                    .arg("--deadline")
                    .arg(deadline.to_string())
                    .arg("--at-deadline")
                    .arg(match at_deadline {
                        AtDeadline::Start => "start",
                        AtDeadline::Skip => "skip",
                    });
            }
        }
        command
            .args(["--", "/usr/bin/perl", "worker.pl"])
            .arg(format!("{:.3}", request.duration_ms as f64 / 1000.0))
            .arg(seed.to_string())
            .arg(request.exit_code.to_string())
            .arg((until.millis() as f64 / 1000.0).to_string());
        let work = request.work.as_ref().expect("validated fixed work");
        command.arg(work.iterations.to_string());
        for (member, cores) in request.demand {
            command
                .arg(&member)
                .arg(cores.to_string())
                .arg(work.rates[&member].to_string());
        }
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .kill_on_drop(true);
        let child = command
            .spawn()
            .map_err(|e| Failure::io("Start synthetic request", e))?;
        let original_identity = child
            .id()
            .and_then(crate::macos::processes::describe)
            .map(|p| p.identity);
        Ok(Self {
            child,
            original_identity,
            status,
            index: request.index,
            exit_code: None,
            run_id: None,
        })
    }
    pub fn index(&self) -> usize {
        self.index
    }
    pub fn identity(&self) -> Option<ProcessIdentity> {
        self.original_identity
    }
    pub fn job(&mut self) -> Result<Job> {
        if self.exit_code.is_none() {
            self.exit_code = self
                .child
                .try_wait()
                .map_err(|e| Failure::io("Wait for synthetic caller", e))?
                .map(|s| s.code().unwrap_or(128));
        }
        if self.run_id.is_none() {
            self.run_id = fs::read_to_string(&self.status).ok().and_then(|s| {
                crate::scheduling::run_id::RunId::parse(s.trim()).map(|id| id.to_string())
            });
        }
        Ok(Job {
            index: self.index,
            run_id: self.run_id.clone(),
            exit_code: self.exit_code,
        })
    }
    pub fn terminate(&mut self) -> Result<()> {
        if self.exit_code.is_none()
            && self
                .child
                .try_wait()
                .map_err(|e| Failure::io("Inspect synthetic caller", e))?
                .is_none()
            && let Some(pid) = self.child.id()
        {
            // Local workers inherit this private group. Remote callers forward
            // TERM and close the normal authenticated execution connection.
            unsafe {
                libc::kill(-(pid as i32), libc::SIGTERM);
            }
        }
        Ok(())
    }
    pub async fn settle(&mut self) -> Result<()> {
        if self.exit_code.is_none() {
            let status =
                match tokio::time::timeout(Duration::from_secs(5), self.child.wait()).await {
                    Ok(status) => status,
                    Err(_) => {
                        if let Some(pid) = self.child.id() {
                            unsafe {
                                libc::kill(-(pid as i32), libc::SIGKILL);
                            }
                        }
                        self.child.wait().await
                    }
                }
                .map_err(|e| Failure::io("Stop synthetic caller", e))?;
            self.exit_code = Some(status.code().unwrap_or(128));
        }
        Ok(())
    }
}
