//! Which machines an executable can run on, read from its Mach-O header, so a
//! member never installs a build made for another platform or architecture.
use crate::failure::{Failure, Result};
use serde::{Deserialize, Serialize};

/// The processor architectures Elsecall builds for on macOS.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Architecture {
    Aarch64,
    X86_64,
}
impl Architecture {
    /// The architecture this process was built for.
    pub fn this() -> Option<Self> {
        match std::env::consts::ARCH {
            "aarch64" => Some(Self::Aarch64),
            "x86_64" => Some(Self::X86_64),
            _ => None,
        }
    }
    fn cpu_type(self) -> u32 {
        match self {
            Self::Aarch64 => 0x0100_000c,
            Self::X86_64 => 0x0100_0007,
        }
    }
    fn from_cpu_type(cpu_type: u32) -> Option<Self> {
        [Self::Aarch64, Self::X86_64]
            .into_iter()
            .find(|a| a.cpu_type() == cpu_type)
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Aarch64 => "aarch64",
            Self::X86_64 => "x86_64",
        }
    }
}

const MH_MAGIC_64: u32 = 0xfeed_facf;
const MH_EXECUTE: u32 = 2;
const FAT_MAGIC: u32 = 0xcafe_babe;
const FAT_MAGIC_64: u32 = 0xcafe_babf;
/// More slices than any real universal binary has.
const MAXIMUM_SLICES: u32 = 16;

fn le(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}
fn be(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}
fn be64(bytes: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_be_bytes(bytes.get(at..at + 8)?.try_into().ok()?))
}

/// The architecture of a 64-bit Mach-O executable image, if `image` is one.
fn thin(image: &[u8]) -> Option<Architecture> {
    if le(image, 0)? != MH_MAGIC_64 || le(image, 12)? != MH_EXECUTE {
        return None;
    }
    Architecture::from_cpu_type(le(image, 4)?)
}

/// The macOS architectures `bytes` is an executable for: one for a thin Mach-O
/// executable, each runnable slice for a universal one, none for anything else.
pub fn architectures(bytes: &[u8]) -> Vec<Architecture> {
    if let Some(architecture) = thin(bytes) {
        return vec![architecture];
    }
    let (wide, entry) = match be(bytes, 0) {
        Some(FAT_MAGIC) => (false, 20),
        Some(FAT_MAGIC_64) => (true, 32),
        _ => return Vec::new(),
    };
    let Some(count) = be(bytes, 4).filter(|count| (1..=MAXIMUM_SLICES).contains(count)) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for index in 0..count as usize {
        let at = 8 + index * entry;
        let (offset, size) = if wide {
            (be64(bytes, at + 8), be64(bytes, at + 16))
        } else {
            (
                be(bytes, at + 8).map(u64::from),
                be(bytes, at + 12).map(u64::from),
            )
        };
        let (Some(offset), Some(size)) = (offset, size) else {
            return Vec::new();
        };
        let image = usize::try_from(offset)
            .ok()
            .zip(usize::try_from(offset.saturating_add(size)).ok())
            .and_then(|(start, end)| bytes.get(start..end));
        // A slice counts only if its own header agrees with the table.
        if let Some(architecture) = image.and_then(thin)
            && be(bytes, at) == Some(architecture.cpu_type())
            && !found.contains(&architecture)
        {
            found.push(architecture);
        }
    }
    found
}

/// Refuses `bytes` unless it is a macOS executable that runs on `architecture`.
pub fn require(bytes: &[u8], architecture: Architecture) -> Result<()> {
    let found = architectures(bytes);
    if found.contains(&architecture) {
        return Ok(());
    }
    let what = if found.is_empty() {
        "is not a macOS executable".to_owned()
    } else {
        format!(
            "is a macOS executable for {}",
            found
                .iter()
                .map(|a| a.name())
                .collect::<Vec<_>>()
                .join(" and ")
        )
    };
    Err(Failure::new(
        "wrong_platform",
        format!(
            "The build {what}; this member runs macOS on {}.",
            architecture.name()
        ),
        format!(
            "Build Elsecall for macOS on {} and distribute that build.",
            architecture.name()
        ),
    ))
}

#[cfg(test)]
pub mod fixtures {
    use super::*;

    /// A minimal thin Mach-O executable header for `architecture`.
    pub fn thin_header(architecture: Architecture, filetype: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&MH_MAGIC_64.to_le_bytes());
        bytes.extend_from_slice(&architecture.cpu_type().to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&filetype.to_le_bytes());
        bytes.resize(64, 0);
        bytes
    }

    /// A universal binary holding one thin executable per architecture.
    pub fn universal(architectures: &[Architecture]) -> Vec<u8> {
        let mut table = Vec::new();
        table.extend_from_slice(&FAT_MAGIC.to_be_bytes());
        table.extend_from_slice(&(architectures.len() as u32).to_be_bytes());
        let mut images = Vec::new();
        let start = 4096;
        for architecture in architectures {
            let image = thin_header(*architecture, MH_EXECUTE);
            let offset = start + images.len();
            table.extend_from_slice(&architecture.cpu_type().to_be_bytes());
            table.extend_from_slice(&0u32.to_be_bytes());
            table.extend_from_slice(&(offset as u32).to_be_bytes());
            table.extend_from_slice(&(image.len() as u32).to_be_bytes());
            table.extend_from_slice(&12u32.to_be_bytes());
            images.extend_from_slice(&image);
        }
        table.resize(start, 0);
        table.extend_from_slice(&images);
        table
    }
}

#[cfg(test)]
mod tests {
    use super::{fixtures::*, *};
    use Architecture::*;

    #[test]
    fn thin_executables_run_only_on_their_architecture() {
        let arm = thin_header(Aarch64, MH_EXECUTE);
        assert_eq!(architectures(&arm), vec![Aarch64]);
        assert!(require(&arm, Aarch64).is_ok());
        let error = require(&arm, X86_64).unwrap_err();
        assert_eq!(error.code, "wrong_platform");
        assert!(error.message.contains("for aarch64"), "{}", error.message);
    }

    #[test]
    fn libraries_scripts_and_other_formats_are_refused() {
        // A dynamic library (MH_DYLIB) is Mach-O but not an executable.
        assert!(architectures(&thin_header(Aarch64, 6)).is_empty());
        for other in [
            b"#!/bin/sh\nexit 0\n".to_vec(),
            b"\x7fELF\x02\x01\x01\0".to_vec(),
            Vec::new(),
            vec![0xcf, 0xfa],
        ] {
            let error = require(&other, Aarch64).unwrap_err();
            assert!(error.message.contains("is not a macOS executable"));
        }
    }

    #[test]
    fn universal_binaries_run_where_one_slice_does() {
        let both = universal(&[X86_64, Aarch64]);
        assert_eq!(architectures(&both), vec![X86_64, Aarch64]);
        assert!(require(&both, Aarch64).is_ok());
        let intel = universal(&[X86_64]);
        assert!(require(&intel, Aarch64).is_err());
        // A table that claims a slice its image does not hold is not trusted.
        let mut lying = universal(&[X86_64]);
        lying[8..12].copy_from_slice(&Aarch64.cpu_type().to_be_bytes());
        assert!(architectures(&lying).is_empty());
        // Truncated tables and absurd slice counts are refused.
        assert!(architectures(&both[..20]).is_empty());
        let mut many = both.clone();
        many[4..8].copy_from_slice(&1000u32.to_be_bytes());
        assert!(architectures(&many).is_empty());
    }

    #[test]
    fn this_machines_own_executable_runs_here() {
        let own = std::fs::read(std::env::current_exe().unwrap()).unwrap();
        assert!(require(&own, Architecture::this().unwrap()).is_ok());
    }
}
