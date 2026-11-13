//! `--run-id-file`: the request's run ID as one line of plain text, so a consumer
//! can correlate its own telemetry with `elsecall explain` without parsing JSON:
//! `read -r run_id < "$file"`. The file is emptied when the request starts, so a
//! stale ID is never mistaken for this request's, and holds the ID once admission
//! decides. It stays empty when there is no record, as for unmanaged work.
use crate::failure::{Failure, Result};
use std::path::PathBuf;

pub struct RunIdFile(PathBuf);

impl RunIdFile {
    /// Empties the file, so a path that cannot be written fails before any work.
    pub fn create(path: PathBuf) -> Result<Self> {
        let file = Self(path);
        file.write(b"")?;
        Ok(file)
    }

    /// A file to empty after a usage error, without failing on it.
    pub fn at(path: PathBuf) -> Self {
        Self(path)
    }

    /// Records the run ID: `None` leaves the file empty.
    pub fn record(&self, run_id: Option<&str>) -> Result<()> {
        match run_id {
            Some(id) => self.write(format!("{id}\n").as_bytes()),
            None => self.write(b""),
        }
    }

    fn write(&self, bytes: &[u8]) -> Result<()> {
        if self.0.file_name().is_none() {
            return Err(Failure::new(
                "invalid_request",
                "--run-id-file must name a file.",
                "Pass a file path such as \"$TMPDIR/run-id\", then rerun the same command.",
            ));
        }
        crate::private_files::replace(&self.0, bytes)
            .map_err(|e| Failure::io("Write the run ID file", e))
    }
}
