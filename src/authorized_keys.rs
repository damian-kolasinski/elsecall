//! The master's SSH key in a member's `~/.ssh/authorized_keys`: the line Add Mac
//! adds, and removing the lines Elsecall added when the member leaves the fleet.
//!
//! The line accepts the key only from tailnet addresses and is marked with a
//! comment, so Elsecall removes only lines it added.
use crate::failure::{Failure, Result};
use std::path::Path;

/// Every line Elsecall adds ends with this comment, followed by the master's name.
const MARK: &str = "elsecall-master:";
/// Tailscale's address ranges, as `tailnet::is_tailnet_address` accepts them.
const FROM: &str = "from=\"100.64.0.0/10,fd7a:115c:a1e0::/48\"";
const KEY_TYPES: [&str; 4] = [
    "ssh-ed25519",
    "ecdsa-sha2-nistp256",
    "ecdsa-sha2-nistp384",
    "ssh-rsa",
];

/// The `authorized_keys` line for `public_key`, the content of a `.pub` file on
/// the master named `master`.
pub fn line(public_key: &str, master: &str) -> Result<String> {
    let mut fields = public_key.split_whitespace();
    let (Some(kind), Some(blob)) = (fields.next(), fields.next()) else {
        return Err(unreadable());
    };
    let blob_ok = blob
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'='));
    if !KEY_TYPES.contains(&kind) || !blob_ok || public_key.lines().count() != 1 {
        return Err(unreadable());
    }
    Ok(format!("{FROM} {kind} {blob} {MARK}{master}"))
}

fn unreadable() -> Failure {
    Failure::new(
        "ssh_key_unreadable",
        "The master's SSH public key is not a single OpenSSH key line.",
        "Check ~/.ssh/id_ed25519.pub on the master.",
    )
}

/// The key blob of an `authorized_keys` line, ignoring its options and comment.
pub fn key_of(line: &str) -> Option<&str> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    let kind = fields.iter().position(|f| KEY_TYPES.contains(f))?;
    fields.get(kind + 1).copied()
}

/// `content` without the lines Elsecall added; `None` when there are none.
pub fn without_ours(content: &str) -> Option<String> {
    let ours = |line: &str| {
        line.split_whitespace()
            .last()
            .is_some_and(|comment| comment.starts_with(MARK))
    };
    if !content.lines().any(ours) {
        return None;
    }
    let kept: Vec<&str> = content.lines().filter(|line| !ours(line)).collect();
    Some(if kept.is_empty() {
        String::new()
    } else {
        kept.join("\n") + "\n"
    })
}

/// Removes the lines Elsecall added from the account's `authorized_keys`, keeping
/// every other line and the file's mode. A missing file is fine.
pub fn remove_ours(home: &Path) -> Result<()> {
    let path = home.join(".ssh/authorized_keys");
    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(Failure::io("Read ~/.ssh/authorized_keys", error)),
    };
    let Some(kept) = without_ours(&content) else {
        return Ok(());
    };
    // Written next to it and renamed over it, so a failure leaves the old file.
    let staged = path.with_extension("elsecall");
    let permissions = std::fs::metadata(&path)
        .map_err(|error| Failure::io("Read ~/.ssh/authorized_keys", error))?
        .permissions();
    std::fs::write(&staged, kept)
        .and_then(|()| std::fs::set_permissions(&staged, permissions))
        .and_then(|()| std::fs::rename(&staged, &path))
        .map_err(|error| {
            let _ = std::fs::remove_file(&staged);
            Failure::io("Update ~/.ssh/authorized_keys", error)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIExampleKeyBlob me@example.com\n";

    #[test]
    fn the_line_limits_the_key_to_the_tailnet_and_marks_it() {
        let added = line(KEY, "mac-mini").unwrap();
        assert_eq!(
            added,
            "from=\"100.64.0.0/10,fd7a:115c:a1e0::/48\" ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIExampleKeyBlob elsecall-master:mac-mini"
        );
        assert_eq!(
            key_of(&added),
            Some("AAAAC3NzaC1lZDI1NTE5AAAAIExampleKeyBlob")
        );
        assert_eq!(
            key_of(KEY.trim()),
            Some("AAAAC3NzaC1lZDI1NTE5AAAAIExampleKeyBlob")
        );
        for bad in [
            "",
            "ssh-ed25519",
            "ssh-dss AAAA",
            "ssh-ed25519 AA\"AA",
            "ssh-ed25519 A\nssh-rsa B",
        ] {
            assert_eq!(
                line(bad, "m").unwrap_err().code,
                "ssh_key_unreadable",
                "{bad:?}"
            );
        }
    }

    #[test]
    fn only_elsecalls_lines_are_removed() {
        let ours = line(KEY, "mac-mini").unwrap();
        let other = "ssh-ed25519 AAAAOther devbox@example.com";
        let content = format!("{other}\n{ours}\n# a note\n");
        assert_eq!(
            without_ours(&content).unwrap(),
            format!("{other}\n# a note\n")
        );
        assert_eq!(without_ours(&format!("{other}\n")), None);
        assert_eq!(without_ours(&format!("{ours}\n")).unwrap(), "");
    }

    #[test]
    fn removing_keeps_other_lines_and_the_mode() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        assert!(remove_ours(home.path()).is_ok(), "no file is fine");
        std::fs::create_dir(home.path().join(".ssh")).unwrap();
        let path = home.path().join(".ssh/authorized_keys");
        let ours = line(KEY, "mac-mini").unwrap();
        std::fs::write(&path, format!("ssh-rsa AAAAOther\n{ours}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        remove_ours(home.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "ssh-rsa AAAAOther\n"
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
