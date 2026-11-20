//! Asking before syncing secrets: a change to a synced path that adds or changes
//! a secret-looking file (`secret_files`) waits until a person confirms it.
//!
//! The master scans its files, then holds back every path whose change touches
//! such a file and that no consent covers: the path keeps the tree last issued,
//! and a `ConsentRequest` names its waiting files. A person confirms one request
//! by its `consent_id`, which names exactly those files at exactly that content,
//! so what is confirmed is what was shown. With "always", the files' names join
//! the manifest's `consented` set and their later changes sync unasked; a new
//! secret-looking file in the path is asked about again.
use crate::config_sync::{
    manifest::{FileKey, Manifest, Tree},
    report,
    secret_files::{Inspected, classify},
};
use elsecall_contract::failure::Result;
use elsecall_fleet::config_sync_model::{
    consent::{ConsentRequest, SecretFile},
    synced_path::SyncedPath,
};
use elsecall_workspace::digest::Digest;
use std::collections::BTreeMap;
use zeroize::Zeroizing;

/// How consent reads the master's files.
pub trait Files {
    /// One file's content and full permission bits.
    fn inspect(&self, path: &SyncedPath, key: &FileKey) -> Result<(Zeroizing<Vec<u8>>, u32)>;
    /// The permission bits of the directory enclosing `path`'s files.
    fn enclosing_mode(&self, path: &SyncedPath) -> Result<u32>;
    /// The master's home folder, as an absolute path.
    fn home(&self) -> String;
}

impl Files for crate::config_sync::replica::Replica {
    fn inspect(&self, path: &SyncedPath, key: &FileKey) -> Result<(Zeroizing<Vec<u8>>, u32)> {
        crate::config_sync::replica::Replica::inspect(self, path, key)
    }
    fn enclosing_mode(&self, path: &SyncedPath) -> Result<u32> {
        crate::config_sync::replica::Replica::enclosing_mode(self, path)
    }
    fn home(&self) -> String {
        self.home_folder().to_string_lossy().into_owned()
    }
}

/// A request a person confirmed: its path issues even though it waits.
pub struct Confirmed<'a> {
    pub path: &'a SyncedPath,
    pub consent_id: &'a str,
}

/// Holds back, in `trees` (what the master would issue now), each path whose
/// change from `issued` adds or changes a secret-looking file that its consent
/// does not cover, and returns the requests for them, in path order. A path
/// `issued` lacks is being enabled from a plan a person confirmed, so it passes.
pub fn hold(
    issued: &Manifest,
    trees: &mut BTreeMap<SyncedPath, Tree>,
    files: &dyn Files,
    confirmed: Option<Confirmed<'_>>,
) -> Result<Vec<ConsentRequest>> {
    let home = files.home();
    let mut requests = Vec::new();
    for (path, tree) in trees.iter_mut() {
        let Some(before) = issued.paths.get(path) else {
            continue;
        };
        if before == tree {
            continue;
        }
        let consented = issued.consented.get(path);
        let waiting = waiting(
            path,
            tree,
            before,
            |name| consented.is_some_and(|c| c.contains(name)),
            files,
            &home,
        )?;
        if waiting.is_empty() {
            continue;
        }
        let identity: Vec<_> = waiting.iter().map(|(_, id)| id).collect();
        let consent_id = Digest::of(
            &serde_json::to_vec(&(path, &identity)).expect("names and fingerprints serialize"),
        )
        .hex()[..16]
            .to_owned();
        if confirmed
            .as_ref()
            .is_some_and(|c| c.path == path && c.consent_id == consent_id)
        {
            continue;
        }
        *tree = before.clone();
        requests.push(ConsentRequest {
            path: path.clone(),
            consent_id,
            files: waiting.into_iter().map(|(file, _)| file).collect(),
        });
    }
    Ok(requests)
}

/// What pins a waiting file to the content shown: its name, keyed fingerprint
/// and mode.
type Identity = (String, String, u32);

/// Every secret-looking file in `tree`, as a plan shows the files enabling a
/// path would sync.
pub fn secret_looking(
    path: &SyncedPath,
    tree: &Tree,
    files: &dyn Files,
) -> Result<Vec<SecretFile>> {
    let all = waiting(path, tree, &Tree::Absent, |_| false, files, &files.home())?;
    Ok(all.into_iter().map(|(file, _)| file).collect())
}

/// The files of `tree` that are new or changed since `before`, not consented,
/// and look secret, each with what identifies it at this content.
fn waiting(
    path: &SyncedPath,
    tree: &Tree,
    before: &Tree,
    consented: impl Fn(&str) -> bool,
    files: &dyn Files,
    home: &str,
) -> Result<Vec<(SecretFile, Identity)>> {
    let previous = before.files();
    let mut waiting = Vec::new();
    let mut enclosing = None;
    for (key, node) in tree.files() {
        let name = report::file_name(path, &key);
        if previous.get(&key) == Some(&node) || consented(&name) {
            continue;
        }
        let (content, mode) = files.inspect(path, &key)?;
        let enclosing = match enclosing {
            Some(mode) => mode,
            None => *enclosing.insert(files.enclosing_mode(path)?),
        };
        let reasons = classify(&Inspected {
            path,
            name: own_name(path, &key),
            owner_only: mode & 0o077 == 0 && enclosing & 0o077 != 0,
            content: &content,
            home,
        });
        if !reasons.is_empty() {
            let identity = (name.clone(), node.fingerprint.short(), node.mode);
            waiting.push((
                SecretFile {
                    file: name,
                    reasons,
                },
                identity,
            ));
        }
    }
    Ok(waiting)
}

/// The file's own name: its last component, or the path's for a single file.
fn own_name<'a>(path: &'a SyncedPath, key: &'a FileKey) -> &'a str {
    match key {
        Some(inner) => inner.as_str().rsplit('/').next().unwrap_or_default(),
        None => path.file_name(),
    }
}

#[cfg(test)]
mod tests {
    use crate::config_sync::consent::*;
    use crate::config_sync::{
        fingerprint::SyncKey,
        manifest::{FileNode, Node},
    };
    use elsecall_fleet::config_sync_model::consent::SecretReason;
    use elsecall_workspace::path::WorkspacePath;
    use std::collections::BTreeSet;

    /// Files in memory, by `~/…` name.
    struct Memory(BTreeMap<String, (Vec<u8>, u32)>);
    impl Files for Memory {
        fn inspect(&self, path: &SyncedPath, key: &FileKey) -> Result<(Zeroizing<Vec<u8>>, u32)> {
            let (content, mode) = &self.0[&report::file_name(path, key)];
            Ok((Zeroizing::new(content.clone()), *mode))
        }
        fn enclosing_mode(&self, _: &SyncedPath) -> Result<u32> {
            Ok(0o755)
        }
        fn home(&self) -> String {
            "/Users/me".into()
        }
    }

    fn tree(key: &SyncKey, files: &[(&str, &[u8])]) -> Tree {
        Tree::Directory(
            files
                .iter()
                .map(|(name, content)| {
                    (
                        WorkspacePath::parse(name).unwrap(),
                        Node::File(FileNode {
                            mode: 0o600,
                            size: content.len() as u64,
                            fingerprint: key.fingerprint(content),
                        }),
                    )
                })
                .collect(),
        )
    }

    fn manifest(key: &SyncKey, path: &SyncedPath, tree: Tree) -> Manifest {
        Manifest {
            generation: 1,
            issued_by: "mac-mini".parse().unwrap(),
            key: key.clone(),
            paths: BTreeMap::from([(path.clone(), tree)]),
            service: None,
            consented: BTreeMap::new(),
        }
    }

    #[test]
    fn a_secret_change_waits_until_confirmed_or_always_consented() {
        let key = SyncKey::generate().unwrap();
        let path = SyncedPath::parse("~/.config/acme-ios").unwrap();
        let issued = manifest(&key, &path, tree(&key, &[("settings.json", b"{}")]));
        // A new .env and a changed settings file.
        let now = tree(
            &key,
            &[
                (".env", b"LOG_LEVEL=debug\n"),
                ("settings.json", b"{\"a\":1}"),
            ],
        );
        let files = Memory(BTreeMap::from([
            (
                "~/.config/acme-ios/.env".into(),
                (b"LOG_LEVEL=debug\n".to_vec(), 0o644),
            ),
            (
                "~/.config/acme-ios/settings.json".into(),
                (b"{\"a\":1}".to_vec(), 0o644),
            ),
        ]));
        let mut trees = BTreeMap::from([(path.clone(), now.clone())]);
        let requests = hold(&issued, &mut trees, &files, None).unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].files,
            [SecretFile {
                file: "~/.config/acme-ios/.env".into(),
                reasons: vec![SecretReason::Name],
            }],
            "names and reasons only"
        );
        assert_eq!(trees[&path], issued.paths[&path], "the whole change waits");

        // The same change asks with the same ID; confirming it lets it through.
        let mut again = BTreeMap::from([(path.clone(), now.clone())]);
        let id = requests[0].consent_id.clone();
        assert_eq!(
            hold(&issued, &mut again, &files, None).unwrap()[0].consent_id,
            id
        );
        let mut confirmed = BTreeMap::from([(path.clone(), now.clone())]);
        let confirm = Confirmed {
            path: &path,
            consent_id: &id,
        };
        assert!(
            hold(&issued, &mut confirmed, &files, Some(confirm))
                .unwrap()
                .is_empty()
        );
        assert_eq!(confirmed[&path], now);
        // A stale ID confirms nothing.
        let mut stale = BTreeMap::from([(path.clone(), now.clone())]);
        let wrong = Confirmed {
            path: &path,
            consent_id: "0000000000000000",
        };
        assert_eq!(
            hold(&issued, &mut stale, &files, Some(wrong))
                .unwrap()
                .len(),
            1
        );

        // Once issued without "always", the next change to .env asks again.
        let mut once = manifest(&key, &path, now.clone());
        let changed = tree(
            &key,
            &[
                (".env", b"LOG_LEVEL=info\n"),
                ("settings.json", b"{\"a\":1}"),
            ],
        );
        let files = Memory(BTreeMap::from([(
            "~/.config/acme-ios/.env".into(),
            (b"LOG_LEVEL=info\n".to_vec(), 0o644),
        )]));
        let mut trees = BTreeMap::from([(path.clone(), changed.clone())]);
        assert_eq!(hold(&once, &mut trees, &files, None).unwrap().len(), 1);
        // With "always", it syncs unasked.
        once.consented = BTreeMap::from([(
            path.clone(),
            BTreeSet::from(["~/.config/acme-ios/.env".to_owned()]),
        )]);
        let mut trees = BTreeMap::from([(path.clone(), changed.clone())]);
        assert!(hold(&once, &mut trees, &files, None).unwrap().is_empty());
        assert_eq!(trees[&path], changed);
    }

    #[test]
    fn ordinary_changes_unchanged_files_and_newly_enabled_paths_pass() {
        let key = SyncKey::generate().unwrap();
        let path = SyncedPath::parse("~/.config/acme-ios").unwrap();
        let secret = b"export API_TOKEN=abc\n";
        let issued = manifest(
            &key,
            &path,
            tree(&key, &[(".env", secret), ("a.json", b"1")]),
        );
        let now = tree(&key, &[(".env", secret), ("a.json", b"2")]);
        let files = Memory(BTreeMap::from([(
            "~/.config/acme-ios/a.json".into(),
            (b"2".to_vec(), 0o644),
        )]));
        let mut trees = BTreeMap::from([(path.clone(), now.clone())]);
        assert!(hold(&issued, &mut trees, &files, None).unwrap().is_empty());
        assert_eq!(trees[&path], now, "the .env did not change");

        let other = SyncedPath::parse("~/.config/other").unwrap();
        let mut trees = BTreeMap::from([(other.clone(), now.clone())]);
        assert!(
            hold(&issued, &mut trees, &Memory(BTreeMap::new()), None)
                .unwrap()
                .is_empty()
        );
    }
}
