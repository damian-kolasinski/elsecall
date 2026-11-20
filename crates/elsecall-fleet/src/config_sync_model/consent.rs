//! What the master asks a person about before syncing: files whose changes wait
//! because they look like secrets. The master's sync status carries the open
//! requests, so every member's fleet view can show them as `secret_consent`
//! attention items. Files are named, never their content.
use crate::config_sync_model::synced_path::SyncedPath;
use serde::{Deserialize, Serialize};

/// Why a file is asked about before it syncs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretReason {
    /// Its name is a secret's: `.env*`, `*.pem`, `id_*`, `.netrc`, keychains…
    Name,
    /// It lies in `~/.ssh`, `~/.gnupg` or `~/Library/Keychains`.
    SensitiveLocation,
    /// The master keeps it readable by its owner only.
    OwnerOnly,
    /// It holds a private key's header.
    PrivateKey,
    /// It holds a word with a well-known token prefix.
    Token,
    /// It assigns a value to a secret's name, or a generated-looking value.
    SecretAssignment,
    /// It names the master's home folder, which differs on other members.
    HomePath,
    /// A reason a newer build added.
    #[serde(other)]
    Other,
}

/// One file waiting, by name.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretFile {
    /// As `~/…`.
    pub file: String,
    pub reasons: Vec<SecretReason>,
}

/// A synced path whose latest change waits for a person.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsentRequest {
    pub path: SyncedPath,
    /// Names exactly these files at exactly this content; `sync consent
    /// --confirm` takes it.
    pub consent_id: String,
    pub files: Vec<SecretFile>,
}
