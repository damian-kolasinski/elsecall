//! Declared outputs: produced paths a placed run brings back to the caller.
//!
//! The member surveys each declared path before the command starts and when it
//! exits ([`output_survey`](super::output_survey)); a path whose survey differs
//! changed. Changed outputs are snapshotted at the exit and described, with
//! digests, from the snapshot ([`output_snapshot`](super::output_snapshot)). The
//! caller treats the description as untrusted ([`validate`]) and replaces each
//! changed output whole with the member's version
//! ([`output_return`](super::output_return)). Whatever does not come back is named
//! with its cause ([`not_returned`](super::not_returned)). Links are neither
//! followed nor returned, on either side.
use crate::workspace::{digest::Digest, path::WorkspacePath};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// Most outputs one run may declare.
pub const MAXIMUM_DECLARED: usize = 64;
/// Most entries one run's outputs may hold, and most bytes.
pub const MAXIMUM_ENTRIES: usize = 200_000;
pub const MAXIMUM_BYTES: u64 = 16 << 30;
/// The largest encoded description or request the other side accepts.
pub const MAXIMUM_LISTING: usize = 64 << 20;

/// One regular file of a returned output.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputFile {
    pub path: WorkspacePath,
    pub mode: u32,
    pub size: u64,
    pub digest: Digest,
}

/// An output the run changed: what it holds now, or `None` if the run removed it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Changed {
    pub output: WorkspacePath,
    pub content: Option<Content>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Content {
    /// Whether the output itself is a directory.
    pub directory: bool,
    pub directories: Vec<WorkspacePath>,
    pub files: Vec<OutputFile>,
    /// Links and other entries, which are not returned.
    pub skipped: Vec<WorkspacePath>,
}

/// Text from the other member, made safe to show: control characters escaped and
/// at most `limit` characters.
pub fn sanitized(text: &str, limit: usize) -> String {
    let mut out: String = text
        .chars()
        .take(limit)
        .flat_map(char::escape_debug)
        .collect();
    if text.chars().count() > limit {
        out.push('…');
    }
    out
}

/// Checks changes a member reported against the outputs the caller declared: each
/// a declared output, each path inside it once, no file inside another file, and
/// within the size bounds.
pub fn validate(changes: &[Changed], declared: &[WorkspacePath]) -> Result<(), String> {
    let mut total = 0u64;
    let mut entries = 0usize;
    let mut outputs = HashSet::new();
    let mut sizes = std::collections::HashMap::new();
    for change in changes {
        if !declared.contains(&change.output) || !outputs.insert(&change.output) {
            return Err("an output is not declared, or is listed twice".into());
        }
        let Some(content) = &change.content else {
            continue;
        };
        let output = &change.output;
        let inside = |path: &WorkspacePath| path.is_within(output) && path != output;
        if content.directory {
            let mut seen = HashSet::new();
            let directories: HashSet<&WorkspacePath> = content.directories.iter().collect();
            for path in content
                .directories
                .iter()
                .chain(content.files.iter().map(|f| &f.path))
                .chain(&content.skipped)
            {
                if !inside(path) || !seen.insert(path) {
                    return Err(format!("{output} lists a path outside it, or twice."));
                }
            }
            // Every parent of a listed path is the output or a listed directory.
            for path in seen.iter() {
                let parent = path.parent().expect("inside the output");
                if &parent != output && !directories.contains(&parent) {
                    return Err(format!(
                        "{output} lists {path} in something that is not a directory."
                    ));
                }
            }
        } else {
            let single_file = content.files.len() == 1
                && &content.files[0].path == output
                && content.skipped.is_empty();
            let skipped_itself = content.files.is_empty() && content.skipped == [output.clone()];
            if !(single_file || skipped_itself) || !content.directories.is_empty() {
                return Err(format!("{output} was described inconsistently."));
            }
        }
        for file in &content.files {
            if file.mode & !0o777 != 0 {
                return Err(format!("{output} lists invalid permissions."));
            }
            // One content, one size: otherwise a small total could stand for many
            // copies of a large file.
            if *sizes.entry(file.digest).or_insert(file.size) != file.size {
                return Err(format!("{output} lists one content with two sizes."));
            }
            total = total.saturating_add(file.size);
        }
        entries += content.files.len() + content.directories.len() + content.skipped.len();
    }
    if total > MAXIMUM_BYTES || entries > MAXIMUM_ENTRIES {
        return Err("The outputs are larger than Elsecall returns.".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(text: &str) -> WorkspacePath {
        WorkspacePath::parse(text).unwrap()
    }
    fn file(text: &str) -> OutputFile {
        OutputFile {
            path: path(text),
            mode: 0o644,
            size: 1,
            digest: Digest::of(b"x"),
        }
    }
    fn directory(files: &[&str], directories: &[&str]) -> Changed {
        Changed {
            output: path("out"),
            content: Some(Content {
                directory: true,
                directories: directories.iter().map(|d| path(d)).collect(),
                files: files.iter().map(|f| file(f)).collect(),
                skipped: Vec::new(),
            }),
        }
    }

    #[test]
    fn refuses_untrusted_descriptions() {
        let declared = [path("out")];
        assert!(validate(&[directory(&["out/a", "out/d/b"], &["out/d"])], &declared).is_ok());
        for bad in [
            directory(&["elsewhere/a"], &[]),
            directory(&["out/a", "out/a"], &[]),
            directory(&["out/a", "out/a/b"], &[]),
            directory(&["out/d/b"], &[]),
            directory(&["out"], &[]),
        ] {
            assert!(validate(&[bad], &declared).is_err());
        }
        let twice = [directory(&["out/a"], &[]), directory(&["out/b"], &[])];
        assert!(validate(&twice, &declared).is_err());
        let file_at = |path: &str, size: u64| OutputFile { size, ..file(path) };
        let amplified = [Changed {
            output: path("out"),
            content: Some(Content {
                directory: true,
                directories: Vec::new(),
                files: vec![
                    file_at("out/big", 1 << 34),
                    file_at("out/a", 0),
                    file_at("out/b", 0),
                ],
                skipped: Vec::new(),
            }),
        }];
        assert!(
            validate(&amplified, &declared).is_err(),
            "one digest, two sizes"
        );
        assert_eq!(sanitized("a\nb\u{1b}c", 3), "a\\nb…");
        let undeclared = [Changed {
            output: path("other"),
            content: None,
        }];
        assert!(validate(&undeclared, &declared).is_err());
    }
}
