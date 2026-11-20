//! `.xcode-version`: the Xcode a repository builds with, one version such as
//! `26.2`, as xcodes and fastlane read it.
use crate::pins::{
    Pin, PinSource, Reading, SourceFile, SourceKind, Target, dotted_version, invalid,
    source_file::read_workspace_file,
};
use elsecall_contract::failure::Result;
use std::path::Path;

const FILE: &str = ".xcode-version";

pub(super) struct XcodeVersionFile;

impl PinSource for XcodeVersionFile {
    fn read(&self, workspace: &Path) -> Result<Reading> {
        let mut reading = Reading::default();
        let Some(text) = read_workspace_file(workspace, FILE)? else {
            return Ok(reading);
        };
        let mut lines = text.lines().map(str::trim).filter(|line| !line.is_empty());
        let version = match (lines.next(), lines.next()) {
            (Some(line), None) => dotted_version(line, 1),
            _ => None,
        }
        .ok_or_else(|| {
            invalid(
                FILE,
                "",
                "the file holds one Xcode version of digits and dots, such as 26.2",
            )
        })?;
        let source = SourceFile {
            kind: SourceKind::XcodeVersion,
            file: FILE.to_owned(),
        };
        reading.pins.push(Pin {
            target: Target::Xcode,
            version,
            source: source.clone(),
        });
        reading.files.push(source);
        Ok(reading)
    }
}

#[cfg(test)]
mod tests {
    use crate::pins::xcode_version::*;
    use std::fs;

    fn read(text: Option<&str>) -> Result<Reading> {
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        if let Some(text) = text {
            fs::write(root.join(FILE), text).unwrap();
        }
        XcodeVersionFile.read(&root)
    }

    #[test]
    fn one_version_is_one_xcode_pin() {
        assert_eq!(read(None).unwrap(), Reading::default());
        for (text, version) in [
            ("26.2\n", "26.2"),
            ("  26.0.1  \n\n", "26.0.1"),
            ("26", "26"),
        ] {
            let reading = read(Some(text)).unwrap();
            assert_eq!(reading.pins.len(), 1);
            assert_eq!(reading.pins[0].target, Target::Xcode);
            assert_eq!(reading.pins[0].version.as_str(), version);
            assert_eq!(reading.files[0].kind, SourceKind::XcodeVersion);
        }
    }

    #[test]
    fn anything_else_is_invalid_without_echoing_it() {
        for text in ["", "26.2\n26.3\n", "26.2 beta SECRET", "latest", "26.2b"] {
            let error = read(Some(text)).unwrap_err();
            assert_eq!(error.code, "pin_source_invalid", "{text:?}");
            assert!(!error.message.contains("SECRET"));
        }
    }
}
