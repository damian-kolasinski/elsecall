//! Which files in a synced path look like secrets, by name, place, permissions
//! and content, and which name the master's home folder. Configuration sync asks
//! a person before syncing a change to one (`consent`).
//!
//! Content is examined in memory and only for its shape: the result is a set of
//! reasons, never a value. A mistake costs one question, so the rules lean
//! towards asking.
use elsecall_contract::secret_shape;
use elsecall_fleet::config_sync_model::{consent::SecretReason, synced_path::SyncedPath};

/// How much of a file's content is examined: keys, tokens and assignments sit
/// near the start of the files that hold them.
const EXAMINED: usize = 256 << 10;
/// Words longer than this are not examined as tokens.
const LONGEST_WORD: usize = 512;

/// What is known about one file of a synced path on the master.
pub struct Inspected<'a> {
    pub path: &'a SyncedPath,
    /// The file's own name: the path's last component for a path that is a file.
    pub name: &'a str,
    /// The master keeps it from others while the directory around it is not:
    /// its owner restricted this file. Sync writes every file on members
    /// owner-only, so on a member turned master only this comparison says
    /// anything.
    pub owner_only: bool,
    pub content: &'a [u8],
    /// The master's home folder, as an absolute path.
    pub home: &'a str,
}

/// Why `file` is asked about before it syncs, in reason order; empty when it is not.
pub fn classify(file: &Inspected<'_>) -> Vec<SecretReason> {
    let mut reasons = Vec::new();
    if secret_name(file.name) {
        reasons.push(SecretReason::Name);
    }
    if file.path.ask_first().is_some() {
        reasons.push(SecretReason::SensitiveLocation);
    }
    if file.owner_only {
        reasons.push(SecretReason::OwnerOnly);
    }
    let content = &file.content[..file.content.len().min(EXAMINED)];
    let text = String::from_utf8_lossy(content);
    if text.contains("-----BEGIN ") && text.contains("PRIVATE KEY") {
        reasons.push(SecretReason::PrivateKey);
    }
    if words(&text).any(secret_shape::token_prefixed) {
        reasons.push(SecretReason::Token);
    }
    if text.lines().any(secret_assignment) {
        reasons.push(SecretReason::SecretAssignment);
    }
    if names_home(&text, file.home) {
        reasons.push(SecretReason::HomePath);
    }
    reasons
}

/// Names secrets usually have: `.env*`, `*.pem`, `*.p12`, `*.key`, `id_*`
/// (not `*.pub`), `.netrc`, the GitHub CLI's `hosts.yml`, provisioning profiles,
/// keychains and credential files.
fn secret_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    let ends = |suffixes: &[&str]| suffixes.iter().any(|s| lower.ends_with(s));
    lower.starts_with(".env")
        || (lower.starts_with("id_") && !lower.ends_with(".pub"))
        || matches!(lower.as_str(), ".netrc" | "hosts.yml" | ".pgpass")
        || lower.starts_with("credentials")
        || ends(&[
            ".pem",
            ".p12",
            ".pfx",
            ".key",
            ".mobileprovision",
            ".keychain",
            ".keychain-db",
            ".jks",
            ".keystore",
        ])
}

fn words(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| c.is_whitespace() || "\"'`=:,;()[]{}<>".contains(c))
        .filter(|word| !word.is_empty() && word.len() <= LONGEST_WORD)
}

/// `NAME=value`, `name: value` or `"name": "value"`, where the name is a
/// secret's or the value looks generated.
fn secret_assignment(line: &str) -> bool {
    let line = line.trim().trim_start_matches("export ");
    let Some(at) = line.find(['=', ':']) else {
        return false;
    };
    let name = line[..at].trim().trim_matches(['"', '\'']);
    let value = line[at + 1..].trim().trim_matches(['"', '\'', ',']);
    let plain_name = !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-.".contains(&b));
    plain_name
        && !value.is_empty()
        && !value.contains(char::is_whitespace)
        && (secret_shape::secret_name(name) || secret_shape::looks_generated(value))
}

/// Whether `text` names `home` as a folder: followed by `/`, a quote, a space or
/// the end, so `/Users/me` is not found in a longer folder name that starts
/// with it.
fn names_home(text: &str, home: &str) -> bool {
    let home = home.trim_end_matches('/');
    !home.is_empty()
        && text.match_indices(home).any(|(at, _)| {
            text[at + home.len()..]
                .chars()
                .next()
                .is_none_or(|c| !(c.is_alphanumeric() || "._-".contains(c)))
        })
}

#[cfg(test)]
mod tests {
    use crate::config_sync::secret_files::*;
    use SecretReason::*;

    fn reasons(path: &str, name: &str, mode: u32, content: &str) -> Vec<SecretReason> {
        classify(&Inspected {
            path: &SyncedPath::parse(path).unwrap(),
            name,
            owner_only: mode & 0o077 == 0,
            content: content.as_bytes(),
            home: "/Users/me",
        })
    }

    /// Secret-looking samples are assembled at run time, so this file holds none
    /// a scanner would flag.
    fn header(kind: &str) -> String {
        format!("-----{} {kind}PRIVATE KEY-----\nAAAA\n", "BEGIN")
    }

    #[test]
    fn names_places_and_permissions_ask() {
        for name in [
            ".env",
            ".env.staging",
            "signing.p12",
            "cert.pem",
            "id_ed25519",
            ".netrc",
            "hosts.yml",
            "App.mobileprovision",
            "build.keychain-db",
        ] {
            assert_eq!(
                reasons("~/.config/acme-ios", name, 0o644, ""),
                [Name],
                "{name}"
            );
        }
        for name in [
            "id_ed25519.pub",
            "config.toml",
            "README.md",
            "environment.json",
        ] {
            assert!(
                reasons("~/.config/acme-ios", name, 0o644, "").is_empty(),
                "{name}"
            );
        }
        assert_eq!(
            reasons("~/.ssh/config", "config", 0o644, ""),
            [SensitiveLocation]
        );
        assert_eq!(
            reasons("~/.config/acme-ios", "settings", 0o600, ""),
            [OwnerOnly]
        );
        assert_eq!(
            reasons(
                "~/.ssh/id_ed25519",
                "id_ed25519",
                0o600,
                &header("OPENSSH ")
            ),
            [Name, SensitiveLocation, OwnerOnly, PrivateKey]
        );
    }

    #[test]
    fn content_with_keys_tokens_and_secret_assignments_asks() {
        let token = format!("gh{}", "p_aB3dE5fG7hJ9".repeat(3));
        let generated = "0f9e8d7c6b5a".repeat(2);
        assert_eq!(
            reasons("~/.config/a", "x", 0o644, &header("RSA ")),
            [PrivateKey]
        );
        assert_eq!(
            reasons("~/.config/a", "x", 0o644, &format!("upload with {token}\n")),
            [Token]
        );
        assert_eq!(
            reasons(
                "~/.config/a",
                "x",
                0o644,
                &format!("export GITHUB_TOKEN={token}\n")
            ),
            [Token, SecretAssignment]
        );
        assert_eq!(
            reasons(
                "~/.config/a",
                "x",
                0o644,
                &format!("\"build_cache\": \"{generated}\",\n")
            ),
            [SecretAssignment]
        );
        assert_eq!(
            reasons("~/.config/a", "x", 0o644, "DB_PASSWORD: hunter2\n"),
            [SecretAssignment]
        );
        for plain in [
            "LOG_LEVEL=debug\n",
            "SCHEME=Acme Staging\n",
            "name: acme\n",
            "PASSWORD=\n",
        ] {
            assert!(
                reasons("~/.config/a", "x", 0o644, plain).is_empty(),
                "{plain}"
            );
        }
    }

    #[test]
    fn the_masters_home_folder_is_named_but_not_a_longer_one() {
        assert_eq!(
            reasons(
                "~/.config/a",
                "x",
                0o644,
                "cache = \"/Users/me/Library/Caches/acme\"\n"
            ),
            [HomePath]
        );
        assert_eq!(
            reasons("~/.config/a", "x", 0o644, "root=/Users/me\n"),
            [HomePath]
        );
        // A longer folder name, assembled so it reads as no real home.
        let longer = format!("root={}/x\n", ["/Users/me", "g"].concat());
        assert!(reasons("~/.config/a", "x", 0o644, &longer).is_empty());
        assert!(reasons("~/.config/a", "x", 0o644, "root=~/Library\n").is_empty());
    }
}
