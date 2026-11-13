//! Synthetic source repositories. No source trace content is executable.
use super::{invalid, worker};
use crate::{
    failure::{Failure, Result},
    workspace::git::Git,
};
use std::{fs, path::Path, process::Stdio};
use tokio::time::Instant;
pub fn create(session: &str, member: &str) -> Result<tempfile::TempDir> {
    tempfile::Builder::new()
        .prefix(&format!("elsecall-rehearsal-{session}-{member}-"))
        .tempdir_in("/private/tmp")
        .map_err(|e| Failure::io("Create rehearsal scratch", e))
}
pub async fn populate(root: &Path, workspaces: Vec<String>, expires: Instant) -> Result<()> {
    for workspace in workspaces {
        let directory = root.join(workspace);
        fs::create_dir(&directory).map_err(|e| Failure::io("Create synthetic workspace", e))?;
        git(&directory, &["init", "-q"], expires).await?;
        fs::write(directory.join("worker.pl"), worker::SCRIPT)
            .map_err(|e| Failure::io("Write synthetic workload", e))?;
        // Stop ancestor consumer rules and environment forwarding at this root.
        fs::write(directory.join(".elsecall.toml"), "")
            .map_err(|e| Failure::io("Write rehearsal configuration", e))?;
        git(
            &directory,
            &["add", "--", "worker.pl", ".elsecall.toml"],
            expires,
        )
        .await?;
        git(
            &directory,
            &[
                "-c",
                "user.name=Elsecall rehearsal",
                "-c",
                "user.email=rehearsal@localhost",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-qm",
                "synthetic workload",
            ],
            expires,
        )
        .await?;
    }
    Ok(())
}
async fn git(directory: &Path, args: &[&str], expires: Instant) -> Result<()> {
    let mut command = Git::member(directory).streaming(args);
    command.stdout(Stdio::null()).stderr(Stdio::null());
    let status = tokio::time::timeout_at(expires, command.status())
        .await
        .map_err(|_| invalid("Rehearsal preparation reached its duration limit."))?
        .map_err(|e| Failure::io("Prepare synthetic repository", e))?;
    if !status.success() {
        return Err(invalid("Could not prepare a synthetic git repository."));
    }
    Ok(())
}
