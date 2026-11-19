//! The master's `fleet-tools.toml`, beside `fleet-config.toml`.
//!
//! A missing file is an empty desired state. Writes replace the file atomically
//! with an owner-only temporary, the same way other application files are replaced.
use crate::toolset::document::LIMIT;
use elsecall_contract::failure::{Failure, Result};
use elsecall_host::private_files;
use std::{
    fs::File,
    io::Read,
    path::{Path, PathBuf},
};

pub(crate) const SOURCE: &str = "fleet-tools.toml";

pub(crate) fn path(data: &Path) -> PathBuf {
    data.join(SOURCE)
}

/// Reads at most one byte past the document limit. `NotFound` is an empty document.
pub(crate) fn read(path: &Path) -> Result<Vec<u8>> {
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

pub(crate) fn write(path: &Path, bytes: &[u8]) -> Result<()> {
    private_files::replace(path, bytes).map_err(|error| Failure::io("Write fleet tools", error))
}
