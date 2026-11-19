//! The `SKILL.md` stub `setup` puts where each agent loads skills, and how a
//! `SKILL.md` found there is told apart: this tool's stub, a whole copy it
//! exported, or someone else's skill under the same name.
use super::guide;

/// Bumped with any change to the stub's text, including the guide description it
/// embeds: refresh replaces a stub only with a higher version, so a downgrade or
/// an older binary on PATH never rewrites a newer stub.
pub const VERSION: u32 = 1;

const MARKER: &str = "elsecall-stub: \"";

/// How every description Elsecall has shipped begins; a whole exported copy is
/// recognised by it. A new description keeps it, or this check is extended.
const SHIPPED: &str = "Run builds, tests and other heavy commands through Elsecall";

pub fn text() -> String {
    format!(
        "---
name: elsecall
description: {description}
metadata:
  {MARKER}{VERSION}\"
---
<!-- Written by `elsecall skill setup`, which keeps it current; `elsecall skill remove` removes it. -->

# Elsecall

When `elsecall` fails, follow the hint it prints, and never retry it in a loop.

Run `elsecall skill show` and follow the guide it prints. It comes from the installed `elsecall`, so it describes that version's commands; `elsecall skill show NAME` prints each topic the guide names.
",
        description = guide::description()
    )
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Owner {
    /// A stub of this tool, at its version.
    Stub(u32),
    /// A whole copy of the skill this tool exported.
    Exported,
    Foreign,
}

pub fn owner(skill: &str) -> Owner {
    let Some(frontmatter) = skill
        .strip_prefix("---\n")
        .and_then(|rest| rest.split_once("\n---\n"))
        .map(|(frontmatter, _)| frontmatter)
    else {
        return Owner::Foreign;
    };
    if !frontmatter.starts_with("name: elsecall\n") {
        return Owner::Foreign;
    }
    if let Some((_, after)) = frontmatter.split_once(MARKER) {
        return after
            .split_once('"')
            .and_then(|(version, _)| version.parse().ok())
            .map_or(Owner::Foreign, Owner::Stub);
    }
    let exported = frontmatter
        .lines()
        .any(|line| line.starts_with(&format!("description: {SHIPPED}")));
    if exported {
        Owner::Exported
    } else {
        Owner::Foreign
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    /// Editing the stub, or the description it embeds, without bumping VERSION
    /// fails here: update both, then this digest.
    #[test]
    fn the_stub_text_is_pinned_to_its_version() {
        let digest = Sha256::digest(text().as_bytes());
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            (VERSION, hex.as_str()),
            (
                1,
                "df29611b975c64beb460ac1b75972005292b429e40f9fc051bcfb54d21d4e94f"
            ),
            "the stub changed: bump VERSION and pin the new digest"
        );
    }

    #[test]
    fn stubs_exported_copies_and_other_skills_are_told_apart() {
        assert_eq!(owner(&text()), Owner::Stub(VERSION));
        assert_eq!(
            owner(&text().replace("elsecall-stub: \"1\"", "elsecall-stub: \"7\"")),
            Owner::Stub(7)
        );
        let exported = guide::files()[0].1;
        assert_eq!(owner(exported), Owner::Exported);
        assert_eq!(
            owner("---\nname: elsecall\ndescription: mine\n---\n"),
            Owner::Foreign
        );
        assert_eq!(
            owner("---\nname: other\n---\nelsecall-stub: \"1\"\n"),
            Owner::Foreign
        );
        assert_eq!(owner("no frontmatter"), Owner::Foreign);
    }
}
