//! The master's `fleet-tools.toml`, beside `fleet-config.toml`.
//!
//! A missing file is an empty desired state. Writes replace the file atomically
//! with an owner-only temporary, the same way other application files are replaced.
use crate::{
    failure::{Failure, Result},
    private_files,
    toolset::document::LIMIT,
};
use std::{
    fs::File,
    io::Read,
    path::{Path, PathBuf},
};

pub const SOURCE: &str = "fleet-tools.toml";

pub fn path(data: &Path) -> PathBuf {
    data.join(SOURCE)
}

/// Reads at most one byte past the document limit. `NotFound` is an empty document.
pub fn read(path: &Path) -> Result<Vec<u8>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(Failure::io("Read fleet tools", error)),
    };
    let mut bytes = Vec::new();
    file.take((LIMIT + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| Failure::io("Read fleet tools", error))?;
    Ok(bytes)
}

pub fn write(path: &Path, bytes: &[u8]) -> Result<()> {
    private_files::replace(path, bytes).map_err(|error| Failure::io("Write fleet tools", error))
}
