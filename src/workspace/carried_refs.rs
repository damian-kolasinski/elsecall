//! The caller's refs a copy carries besides HEAD and its branch, so scripts that
//! compare against the default branch behave in a placed run as they do locally:
//! - the upstream of HEAD;
//! - each remote's default branch: `refs/remotes/<remote>/HEAD` and the branch it
//!   points to, such as `refs/remotes/origin/main`;
//! - refs the consumer declares in `.elsecall.toml` (`[workspace] refs`).
//!
//! Only names under `refs/remotes/` and `refs/tags/` are carried, as the caller has
//! them, at the objects the caller's refs point to. A remote's URL never is: refs
//! answer `rev-parse`, `merge-base`, `rev-list` and `log`, but `git fetch` in a
//! copy fails. Names are validated where they are read, so no untrusted name
//! reaches git's arguments, and a copy never holds two refs git could not.
use crate::{
    failure::{Failure, Result},
    workspace::{
        git::{Git, ObjectFormat, Oid},
        head::{Reference, RemoteName, Upstream},
        sync_failed,
    },
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Most patterns a consumer may declare.
pub const MAXIMUM_PATTERNS: usize = 32;
/// Most refs one copy carries, defaults included.
pub const MAXIMUM_REFS: usize = 256;

const REMOTES: &str = "refs/remotes/";
const TAGS: &str = "refs/tags/";

/// A ref pattern a consumer declares: a full name under `refs/remotes/` or
/// `refs/tags/`, where `*` matches within one name component. A name without `*`
/// matches that ref and every ref under it, as `git for-each-ref` matches.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefPattern(String);

impl RefPattern {
    pub fn parse(text: &str) -> Option<Self> {
        let namespaced = [REMOTES, TAGS].iter().any(|space| {
            text.strip_prefix(space)
                .is_some_and(|rest| !rest.is_empty())
        });
        // `*` is git's only wildcard here; with it replaced, the pattern is a name.
        // `**` would match across components, so it is refused.
        (namespaced && !text.contains("**") && Reference::parse(&text.replace('*', "x")).is_some())
            .then(|| Self(text.to_owned()))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for RefPattern {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The refs a copy carries. Every state this type can hold is one git can hold:
/// names are under `refs/remotes/` or `refs/tags/`, unique, never a file and a
/// directory at once, and every remote's default branch is itself carried.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Listing", into = "Listing")]
pub struct CarriedRefs {
    /// Full names and the objects they point to: commits, or tags for annotated
    /// tags.
    refs: BTreeMap<Reference, Oid>,
    /// Each remote's default branch, which `refs/remotes/<remote>/HEAD` names.
    defaults: BTreeMap<RemoteName, Reference>,
}

/// How carried refs travel and are recorded.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Listing {
    refs: Vec<CarriedRef>,
    defaults: Vec<RemoteDefault>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CarriedRef {
    name: Reference,
    object: Oid,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoteDefault {
    remote: RemoteName,
    branch: Reference,
}

impl From<CarriedRefs> for Listing {
    fn from(carried: CarriedRefs) -> Self {
        Self {
            refs: carried
                .refs
                .into_iter()
                .map(|(name, object)| CarriedRef { name, object })
                .collect(),
            defaults: carried
                .defaults
                .into_iter()
                .map(|(remote, branch)| RemoteDefault { remote, branch })
                .collect(),
        }
    }
}

impl TryFrom<Listing> for CarriedRefs {
    type Error = String;
    fn try_from(listing: Listing) -> std::result::Result<Self, String> {
        if listing.refs.len() > MAXIMUM_REFS || listing.defaults.len() > MAXIMUM_REFS {
            return Err("too many carried refs".into());
        }
        let mut carried = CarriedRefs::default();
        for CarriedRef { name, object } in listing.refs {
            if carried.refs.insert(name, object).is_some() {
                return Err("a carried ref is listed twice".into());
            }
        }
        for RemoteDefault { remote, branch } in listing.defaults {
            if carried.defaults.insert(remote, branch).is_some() {
                return Err("a remote's default branch is listed twice".into());
            }
        }
        carried.check()?;
        Ok(carried)
    }
}

/// The fields of `for-each-ref --format=%(refname)%00%(symref)%00%(objectname)`:
/// name, symbolic target (empty for a direct ref) and object. Lines that are not
/// UTF-8 name refs no `Reference` can hold, and are skipped.
fn listed(output: &[u8]) -> impl Iterator<Item = (&str, &str, &str)> {
    output.split(|b| *b == b'\n').filter_map(|line| {
        let mut fields = std::str::from_utf8(line).ok()?.split('\0');
        Some((fields.next()?, fields.next()?, fields.next()?))
    })
}

/// Whether `branch` is one of `remote`'s remote-tracking refs.
fn belongs_to(remote: &RemoteName, branch: &Reference) -> bool {
    branch
        .as_str()
        .strip_prefix(REMOTES)
        .and_then(|rest| rest.strip_prefix(remote.as_str()))
        .is_some_and(|rest| rest.starts_with('/'))
}

/// `refs/remotes/<remote>/HEAD`, which names the remote's default branch.
fn remote_head(remote: &RemoteName) -> String {
    format!("{REMOTES}{remote}/HEAD")
}

impl CarriedRefs {
    fn check(&self) -> std::result::Result<(), String> {
        let mut names: BTreeSet<&str> = BTreeSet::new();
        for name in self.refs.keys() {
            let name = name.as_str();
            let remote_head = name
                .strip_prefix(REMOTES)
                .and_then(|rest| rest.split_once('/'))
                .is_some_and(|(_, branch)| branch == "HEAD");
            if !(name.starts_with(REMOTES) || name.starts_with(TAGS)) || remote_head {
                return Err("a carried ref is outside refs/remotes/ and refs/tags/".into());
            }
            names.insert(name);
        }
        let heads: Vec<String> = self.defaults.keys().map(remote_head).collect();
        for ((remote, branch), head) in self.defaults.iter().zip(&heads) {
            if !belongs_to(remote, branch) || !self.refs.contains_key(branch) {
                return Err("a remote's default branch is not one of its carried refs".into());
            }
            names.insert(head);
        }
        // git stores refs as files: no name may be a directory of another.
        for name in &names {
            let mut prefix = *name;
            while let Some((parent, _)) = prefix.rsplit_once('/') {
                if names.contains(parent) {
                    return Err("one carried ref is inside another".into());
                }
                prefix = parent;
            }
        }
        Ok(())
    }

    /// Whether every object name fits the repository's hash.
    pub fn valid(&self, format: ObjectFormat) -> bool {
        self.refs.values().all(|oid| oid.fits(format))
    }

    /// Whether the upstream of HEAD, when carried here, is at the same commit.
    pub fn agrees_with(&self, upstream: Option<&Upstream>) -> bool {
        upstream.is_none_or(|u| {
            self.refs
                .get(&u.tracking)
                .is_none_or(|oid| *oid == u.commit)
        })
    }

    pub fn is_empty(&self) -> bool {
        self.refs.is_empty()
    }

    /// The objects the copy needs with their history, each once: remotes'
    /// default branches first, then the rest. An object source most likely holds
    /// the default branches; the rest may be far from HEAD's history.
    pub fn objects(&self) -> (Vec<Oid>, Vec<Oid>) {
        let mut seen = BTreeSet::new();
        let defaults = self
            .defaults
            .values()
            .map(|branch| &self.refs[branch])
            .filter(|oid| seen.insert(*oid))
            .cloned()
            .collect();
        let rest = self
            .refs
            .values()
            .filter(|oid| seen.insert(*oid))
            .cloned()
            .collect();
        (defaults, rest)
    }

    /// Every name carried, remotes' `HEAD`s included, for records.
    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .refs
            .keys()
            .map(|name| name.as_str().to_owned())
            .chain(self.defaults.keys().map(remote_head))
            .collect();
        names.sort();
        names
    }

    /// Names and objects, for references kept elsewhere, such as an object store.
    pub fn entries(&self) -> impl Iterator<Item = (&Reference, &Oid)> {
        self.refs.iter()
    }

    // ----- Caller --------------------------------------------------------------

    /// Lists the caller's refs that may be carried: each remote's default branch
    /// and the refs `declared` patterns match. [`CarriedRefs::read`] reads them.
    pub fn list(git: &Git, declared: &[RefPattern]) -> Result<Vec<u8>> {
        let mut args = vec![
            "for-each-ref".to_owned(),
            "--format=%(refname)%00%(symref)%00%(objectname)".to_owned(),
            format!("{REMOTES}*/HEAD"),
        ];
        // Patterns start with `refs/`, so none reads as an option.
        args.extend(declared.iter().map(|pattern| pattern.0.clone()));
        git.output(&args)
    }

    /// The refs to carry: what [`CarriedRefs::list`] listed, and `upstream`. A ref
    /// git could not name the same way on the member is left out, as an upstream
    /// is.
    pub fn read(output: &[u8], upstream: Option<&Upstream>) -> Result<Self> {
        let mut carried = Self::default();
        let mut matched = 0;
        for (name, target, object) in listed(output) {
            let Some(object) = Oid::parse(object) else {
                continue;
            };
            if target.is_empty() {
                if name.ends_with("/HEAD") && name.starts_with(REMOTES) {
                    continue;
                }
                if let Some(name) = Reference::parse(name) {
                    matched += 1;
                    carried.refs.insert(name, object);
                }
                continue;
            }
            // A symbolic ref is carried only as a remote's default branch.
            let Some(remote) = name
                .strip_prefix(REMOTES)
                .and_then(|rest| rest.strip_suffix("/HEAD"))
                .and_then(RemoteName::parse)
            else {
                continue;
            };
            // A default branch that is not the remote's own is left out.
            let Some(branch) = Reference::parse(target).filter(|b| belongs_to(&remote, b)) else {
                continue;
            };
            carried.refs.insert(branch.clone(), object);
            carried.defaults.insert(remote, branch);
        }
        if let Some(upstream) = upstream {
            carried
                .refs
                .insert(upstream.tracking.clone(), upstream.commit.clone());
        }
        if carried.refs.len() > MAXIMUM_REFS {
            return Err(too_many(carried.refs.len(), matched));
        }
        // Refs git holds never nest, so this fails only on a damaged repository.
        carried
            .check()
            .map_err(|_| sync_failed("The caller's refs cannot be reproduced on a member."))?;
        Ok(carried)
    }

    // ----- Member --------------------------------------------------------------

    /// Lists a copy's `refs/remotes/` and `refs/tags/`, for [`CarriedRefs::apply`].
    /// A sync lists them early, beside other work: nothing else changes them
    /// before it applies the caller's.
    pub fn list_copy(git: &Git) -> Result<Vec<u8>> {
        git.output(&[
            "for-each-ref",
            "--format=%(refname)%00%(symref)%00%(objectname)",
            "refs/remotes",
            "refs/tags",
        ])
    }

    /// Makes the copy's `refs/remotes/` and `refs/tags/`, which `current` lists,
    /// exactly these refs and the upstream's tracking ref, undoing whatever a run
    /// changed there. Returns whether anything had to change.
    pub fn apply(&self, git: &Git, upstream: Option<&Upstream>, current: &[u8]) -> Result<bool> {
        let mut direct: BTreeMap<&str, &Oid> = self
            .refs
            .iter()
            .map(|(name, oid)| (name.as_str(), oid))
            .collect();
        if let Some(upstream) = upstream {
            direct.insert(upstream.tracking.as_str(), &upstream.commit);
        }
        let symbolic: BTreeMap<String, &str> = self
            .defaults
            .iter()
            .map(|(remote, branch)| (remote_head(remote), branch.as_str()))
            .collect();
        let mut deletions = Vec::new();
        let mut current_direct = BTreeSet::new();
        let mut current_symbolic = BTreeSet::new();
        for (name, target, object) in listed(current) {
            let wanted = if target.is_empty() {
                direct.get(name).is_some_and(|oid| oid.as_str() == object)
            } else {
                symbolic.get(name) == Some(&target)
            };
            if wanted {
                if target.is_empty() {
                    current_direct.insert(name.to_owned());
                } else {
                    current_symbolic.insert(name.to_owned());
                }
            } else if !direct.contains_key(name) || !target.is_empty() {
                // Anything else is removed; a direct ref at the wrong commit is
                // simply updated below.
                deletions.push(name.to_owned());
            }
        }
        let updates: Vec<(&str, &Oid)> = direct
            .iter()
            .filter(|(name, _)| !current_direct.contains(**name))
            .map(|(name, oid)| (*name, *oid))
            .collect();
        let links: Vec<(&String, &str)> = symbolic
            .iter()
            .filter(|(name, _)| !current_symbolic.contains(*name))
            .map(|(name, target)| (name, *target))
            .collect();
        // Removals go first and on their own, so a ref may replace a directory of
        // refs that a run created, or the other way round.
        if !deletions.is_empty() {
            let input: String = deletions.iter().map(|n| format!("delete {n}\n")).collect();
            git.output_with(
                &["update-ref", "--no-deref", "--stdin"],
                Some(input.as_bytes()),
            )?;
        }
        if !updates.is_empty() {
            let input: String = updates
                .iter()
                .map(|(name, oid)| format!("update {name} {oid}\n"))
                .collect();
            git.output_with(
                &["update-ref", "--no-deref", "--stdin"],
                Some(input.as_bytes()),
            )?;
        }
        for (name, target) in &links {
            git.output(&["symbolic-ref", name.as_str(), target])?;
        }
        Ok(!(deletions.is_empty() && updates.is_empty() && links.is_empty()))
    }
}

fn too_many(total: usize, matched: usize) -> Failure {
    Failure::new(
        "invalid_request",
        format!(
            "The copy would carry {total} refs, more than the {MAXIMUM_REFS} a copy carries; {matched} of them match [workspace] refs in .elsecall.toml."
        ),
        "Narrow the patterns under [workspace] refs in .elsecall.toml, then rerun the same command.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oid(n: u8) -> String {
        format!("{n:040x}")
    }
    fn listing(refs: &[(&str, u8)], defaults: &[(&str, &str)]) -> String {
        serde_json::json!({
            "refs": refs.iter().map(|(name, n)| serde_json::json!({"name": name, "object": oid(*n)})).collect::<Vec<_>>(),
            "defaults": defaults.iter().map(|(remote, branch)| serde_json::json!({"remote": remote, "branch": branch})).collect::<Vec<_>>(),
        })
        .to_string()
    }
    fn parse(refs: &[(&str, u8)], defaults: &[(&str, &str)]) -> Option<CarriedRefs> {
        serde_json::from_str(&listing(refs, defaults)).ok()
    }

    #[test]
    fn patterns_are_names_under_remotes_or_tags() {
        for valid in [
            "refs/remotes/origin/release/*",
            "refs/remotes/origin/main",
            "refs/remotes/origin",
            "refs/tags/v*",
            "refs/tags/*",
        ] {
            assert!(RefPattern::parse(valid).is_some(), "{valid}");
        }
        for invalid in [
            "",
            "main",
            "origin/main",
            "refs/heads/main",
            "refs/heads/*",
            "refs/remotes/",
            "refs/tags/",
            "refs/remotes/../x",
            "refs/remotes/origin/a b",
            "refs/tags/v[0-9]",
            "refs/tags/v?",
            "refs/elsecall/x",
            "-refs/tags/x",
            "refs/notes/commits",
            "refs/stash",
            "refs/remotes/origin/**",
        ] {
            assert!(RefPattern::parse(invalid).is_none(), "{invalid:?}");
        }
    }

    #[test]
    fn the_caller_carries_remotes_own_default_branches_and_its_upstream() {
        let line = |name: &str, target: &str, n: u8| -> Vec<u8> {
            format!("{name}\0{target}\0{}\n", oid(n)).into_bytes()
        };
        let mut listing = Vec::new();
        for part in [
            line("refs/remotes/origin/HEAD", "refs/remotes/origin/main", 1),
            // Another remote's branch, a local branch, and a remote git cannot
            // name the same way are no default branches.
            line("refs/remotes/mirror/HEAD", "refs/remotes/origin/main", 1),
            line("refs/remotes/local/HEAD", "refs/heads/main", 2),
            line("refs/remotes/we:ird/HEAD", "refs/remotes/we:ird/main", 2),
            // A direct remote HEAD is never carried; a declared match is.
            line("refs/remotes/plain/HEAD", "", 3),
            line("refs/remotes/origin/release/1", "", 4),
        ] {
            listing.extend(part);
        }
        listing.extend(b"refs/tags/\xff\0\0".iter().chain(oid(5).as_bytes()));
        listing.push(b'\n');
        let upstream = Upstream {
            remote: RemoteName::parse("origin").unwrap(),
            merge: Reference::parse("refs/heads/work").unwrap(),
            tracking: Reference::parse("refs/remotes/origin/work").unwrap(),
            commit: Oid::parse(&oid(6)).unwrap(),
        };
        let carried = CarriedRefs::read(&listing, Some(&upstream)).unwrap();
        assert_eq!(
            carried.names(),
            [
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/main",
                "refs/remotes/origin/release/1",
                "refs/remotes/origin/work",
            ]
        );
        assert!(carried.agrees_with(Some(&upstream)));
        let moved = Upstream {
            commit: Oid::parse(&oid(7)).unwrap(),
            ..upstream
        };
        assert!(!carried.agrees_with(Some(&moved)));
    }

    #[test]
    fn carried_refs_only_hold_states_git_can() {
        let carried = parse(
            &[("refs/remotes/origin/main", 1), ("refs/tags/v1", 2)],
            &[("origin", "refs/remotes/origin/main")],
        )
        .unwrap();
        assert_eq!(
            carried.names(),
            [
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/main",
                "refs/tags/v1"
            ]
        );
        let (defaults, rest) = carried.objects();
        assert_eq!(
            defaults.iter().map(Oid::as_str).collect::<Vec<_>>(),
            [oid(1)]
        );
        assert_eq!(rest.iter().map(Oid::as_str).collect::<Vec<_>>(), [oid(2)]);
        let round_trip: CarriedRefs =
            serde_json::from_str(&serde_json::to_string(&carried).unwrap()).unwrap();
        assert_eq!(round_trip, carried);

        type Refs<'a> = &'a [(&'a str, u8)];
        type Defaults<'a> = &'a [(&'a str, &'a str)];
        let refused: &[(Refs, Defaults)] = &[
            (&[("refs/heads/main", 1)], &[]),
            (&[("refs/elsecall/x/head", 1)], &[]),
            (&[("refs/remotes/origin/HEAD", 1)], &[]),
            (&[("--upload-pack=x", 1)], &[]),
            (
                &[
                    ("refs/remotes/origin/main", 1),
                    ("refs/remotes/origin/main", 2),
                ],
                &[],
            ),
            (&[("refs/remotes/o/a", 1), ("refs/remotes/o/a/b", 2)], &[]),
            // A default branch that is not carried, or not the remote's own.
            (&[], &[("origin", "refs/remotes/origin/main")]),
            (
                &[("refs/remotes/up/main", 1)],
                &[("origin", "refs/remotes/up/main")],
            ),
            (
                &[("refs/remotes/origin/main", 1)],
                &[("origin", "refs/tags/main")],
            ),
            // The HEAD of a remote inside another carried ref.
            (
                &[
                    ("refs/remotes/origin/HEAD/x", 1),
                    ("refs/remotes/origin/main", 1),
                ],
                &[("origin", "refs/remotes/origin/main")],
            ),
        ];
        for (refs, defaults) in refused {
            assert!(parse(refs, defaults).is_none(), "{refs:?} {defaults:?}");
        }
        let many: Vec<(String, u8)> = (0..=MAXIMUM_REFS)
            .map(|i| (format!("refs/tags/t{i}"), 1))
            .collect();
        let many: Vec<(&str, u8)> = many.iter().map(|(n, o)| (n.as_str(), *o)).collect();
        assert!(parse(&many, &[]).is_none());
        assert!(parse(&many[..MAXIMUM_REFS], &[]).is_some());
    }
}
