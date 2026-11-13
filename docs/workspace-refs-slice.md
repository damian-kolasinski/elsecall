# Workspace refs: the caller's remote-tracking refs in copies

A follow-up to [S3](remote-run-slice.md) ("Copies") and the
[fast first sync](first-sync-slice.md). The first real placed run of a consumer,
acme-ios's `test.sh` on `macbook-pro`, failed before doing any work:

```
lint: check unavailable: git merge-base failed: fatal: Not a valid object name origin/main
```

The copy held `refs/heads` and the upstream's tracking ref, nothing else. Consumer
scripts use the default branch for merge bases, "affected" test selection and
"behind main" checks, so a placed run behaved differently from a local one.

## Contract change

"Workspace" → "Same state" in [fleet-scheduling.md](fleet-scheduling.md): a copy
carries, as remote-tracking refs with the caller's names,

- the upstream of HEAD;
- each remote's default branch: `refs/remotes/<remote>/HEAD` and the branch it
  points to, such as `origin/main`;
- refs the consumer declares in `.elsecall.toml`:

  ```toml
  [workspace]
  refs = ["refs/remotes/origin/release/*", "refs/tags/v*"]
  ```

They point at the caller's commits as of the sync and follow it on every sync.
Other remote branches are not carried: a project can have hundreds, and each needs
history. A remote's URL still never reaches the copy, so `git fetch` and
`git push` in a copy fail; fetching is the caller's.

## What changed

- **`workspace::carried_refs`** owns the refs a copy carries.
  - `RefPattern` is a declared pattern: a full name under `refs/remotes/` or
    `refs/tags/`, where `*` matches within one name component, matched as
    `git for-each-ref` matches. `**`, which git lets cross components, is
    refused. At most 32.
  - `CarriedRefs` holds names and objects, and remotes' default branches. Every
    state it can hold is one git can: names only under `refs/remotes/` and
    `refs/tags/`, unique, none inside another, no direct `refs/remotes/<r>/HEAD`,
    and each default branch is the remote's own and itself carried. Deserializing
    enforces it, so the member validates what the caller sends.
- **The caller** lists `refs/remotes/*/HEAD` and the declared patterns in one
  `for-each-ref`, beside its other reads of the repository, and adds the
  upstream. A default branch that is not the remote's own, or a name git could not
  write the same way, is left out, as an upstream is. More than 256 refs is
  `invalid_request` before the run, naming the patterns to narrow.
- **The protocol.** `Begin` carries `refs`. The member requires the upstream, when
  carried, at the same commit as HEAD's upstream.
  - The member checks the carried objects with HEAD's and its upstream's commits,
    in one `cat-file`. The caller's pack includes their history.
  - Sources are now searched when the store lacks the base of the caller's work,
    as before, **or a carried ref's commit**: those are what a member's clone most
    likely holds. A warm sync whose refs did not move has no extra round trip.
  - Candidates for a source: the missing HEAD and upstream, then remotes' default
    branches, then HEAD's ancestors, then declared refs, which may lie far from
    HEAD's history.
  - A source now supplies more than one commit per sync: after each import, the
    HEAD, upstream and default-branch commits the store still lacks are asked for
    again, up to four imports. A member's clone that holds both a feature
    branch's upstream and `origin/main` supplies both, and the caller sends no
    history.
- **The copy.** On every sync the member makes the copy's `refs/remotes/` and
  `refs/tags/` exactly the carried refs and the upstream's. The copy's refs are
  listed with one `for-each-ref` right after the copy is verified, beside the
  caller's tree listing, so a warm sync whose refs did not move waits for no extra
  git process. Whatever a run did there (moved, added or deleted refs,
  repointed `origin/HEAD`) is undone. Removals go in their own transaction first,
  so a ref can replace a directory of refs. A run can also leave what git does not
  list but still blocks a ref, such as a symbolic ref to nothing where a carried
  ref needs a directory: when applying fails, both namespaces are cleared and
  written again, as they hold only what the caller sends.
- **The store** keeps each carried object under
  `refs/elsecall/<copy>/carried/<name>`, so later syncs send history against them
  and "the store holds this commit" still means its whole history. Store
  references change only when the carried refs do; the copy's record remembers
  them in an optional trailing block, so existing records stay valid.
- **Records.** `placed.sync.refs` lists the carried names on both members; the
  caller records those it sent that the member reports it set, so a member that
  predates this item records none. On the caller,
  `placed.sync.refs_only_commits` counts commits sent only because a carried ref
  needs them, when there were any.

## Found on the way: children inheriting each other's pipes

Stress runs of the new tests hung about once in a hundred: a caller's
`git cat-file --batch-check` waited for the end of its input, and the member's
`git index-pack` waited for the pack the caller would send after it. On macOS a
pipe becomes close-on-exec only after it is created, neither the standard
library nor `pipe` can do both at once, so a child started on another thread in
that moment inherits the pipe: the `index-pack` held the write end of
`cat-file`'s input. The tests run caller and member in one process. In the
service, two copies syncing at once, a sync and a placed command, or a sync and a
probe can do the same: a placed command's output would not end while a sync's
git lives.

The new `child_start` module holds one lock from creating a child's pipes until
the child exists. Every start in the service that creates pipes takes it: git in
syncs and object sources, placed commands (whose own pipes are made under it),
consistency probes, and the Tailscale and firewall queries. Waiting for children
happens outside it. Before the fix, 30 rounds of 6 concurrent runs of the four
new engine tests hung in round 26 with exactly that pair of processes; after it,
all 30 rounds (180 runs) passed.

## Automated evidence

`mise run check` passes.

- **Placed run** (`tests/remote_run.rs`,
  `a_placed_run_compares_against_the_callers_default_branch_and_upstream`). The
  caller works on `work` (upstream `origin/work`) while others moved `origin/main`
  on and pushed `release/1.0` and `their-feature`.
  - `git merge-base HEAD origin/main`, `rev-parse origin/HEAD`,
    `rev-list --count HEAD..origin/main`, `rev-parse @{upstream}` and the declared
    `origin/release/1.0` print exactly what they print on the caller.
  - `origin/their-feature` is absent, `remote.origin.url` is unset and
    `git fetch origin` fails.
  - Both records list the four carried refs, and the caller's counts 4 commits.
  - After `origin/main` moves and the caller fetches, the next run matches again.
  - `[workspace] refs = ["refs/heads/*"]` fails with `invalid_request` (exit 125)
    naming `[workspace] refs`.
- **Copy engine** (`tests/workspace_copy.rs`):
  - `a_copy_carries_the_callers_upstream_default_branch_and_declared_refs`: the
    copy's `refs/remotes` and `refs/tags` equal the caller's under the carried
    names (upstream, default branch, `release/*`, the annotated tag `v1.0`), and
    not `their-feature` or `v0.9`. `describe` sees the tag; `fsck` passes. The
    URL is in neither the copy's configuration nor the store, and fetches fail.
    The caller's refs then move (main on, `release/2.0` pruned) while a run had
    moved `origin/main`, added a ref and a tag and repointed `origin/HEAD`; the
    next sync restores the caller's exactly, with a pack under 4 KiB. An
    unchanged sync sends digests only. Without the declaration, only the defaults
    remain, in the copy and in the store's references.
  - `a_default_branch_comes_from_an_object_source_first`: a member's clone that
    holds main supplies it; only the branch's own commits come from the caller.
    After main moves and both fetch, the source supplies it again and no pack is
    sent.
  - `declared_refs_that_match_too_many_are_refused`: 300 matching tags fail with
    `invalid_request` naming `[workspace] refs`; a narrower pattern syncs.
- **Unit tests**: patterns (`refs/heads/*`, `?`, `[`, `..`, options and other
  namespaces are refused), every refused `CarriedRefs` state, and
  `[workspace] refs` in `.elsecall.toml`, including the README's example.

### Performance

Release builds on this Mac, base `mergegate/main` (c599e42) against this item,
run one after the other. The Mac's load average was 25–70 throughout, so
differences under about 20% are noise.

| Existing test | Base | This item |
| --- | --- | --- |
| 20,000 files, warm unchanged (3 runs) | 314, 209, 210 ms | 252, 216, 221 ms |
| 20,000 files, one file changed | 249 ms | 302 ms |
| 20,000 files, cold | 9,667 ms | 9,791 ms |
| First sync of 300 commits, without a source | 4,345 ms, 31.0 MB | 2,577 ms, 31.0 MB |
| First sync, with the member's clone | 1,074 ms, 191 KB | 735 ms, 191 KB |
| Warm after it | 270 ms, 14 bytes | 103 ms, 14 bytes |

First-sync bytes are identical: those callers have no remote HEAD and nothing
declared, so they carry only their upstream, as before.

The new `warm_syncs_carrying_refs_cost_what_they_did_without` runs warm syncs of
a 5,000-file tree whose caller has 300 remote branches, as a busy project does.
It measures seven syncs in each of three phases: carrying the 2 defaults plus 64
declared release branches (each with history of its own), carrying only the
defaults, and carrying nothing after the caller dropped its remote-tracking refs.

| Warm syncs, 7 each | Median | Range |
| --- | --- | --- |
| 66 refs carried | 142 ms | 131–164 ms |
| 2 defaults carried | 138 ms | 123–145 ms |
| None | 131 ms | 115–137 ms |

The test asserts each of the first two is within 25% plus 15 ms of none. A first
version listed the copy's refs while applying, and 66 refs cost 20 ms more
(132 against 112 ms). The copy's refs are now listed beside the caller's tree
listing. The cold sync with 64 declared branches sent a 215 KB pack, with
`refs_only_commits` 64.

## Installed-system evidence

On 2026-11-03 the release build of this item was installed on
`mac-mini` (master) and `macbook-pro` (account `devbox`, mapped
locations, no object sources configured). Every run below was from the Mac, in
the acme-ios worktree `elsecall-leases`: 12,965 files, upstream
`mergegate/main`, 9 commits behind `origin/main`, with remotes `origin`,
`mergegate` and `forge`, each with a default branch. Nothing was built or tested
on the devbox.

- **`elsecall run --member macbook-pro -- true`**, on the copy left by the
  first-sync item's evidence:
  - both records list the six carried refs: `origin/HEAD`, `origin/main`,
    `mergegate/HEAD`, `mergegate/main` (also the upstream), `forge/HEAD` and
    the branch it names;
  - the caller sent 6 commits only for them (`refs_only_commits`), in a
    248 KB pack with the worktree's own new history; the sync took 1.43 s.
- **The same again, warm:** 630 ms, 14 bytes sent, no pack. The first-sync
  item measured 0.62–0.63 s for warm syncs of this worktree.
- **`elsecall run --member macbook-pro -- git merge-base HEAD origin/main`**:
  locally it prints `3f1c2a9e8b7d6c5f4e3a2b1c0d9e8f7a6b5c4d3e`. The placed run is
  pending: the authoring session's isolation guard refuses a command that hands
  git to `elsecall run` outside its own checkout, so the operator runs it, and the
  next revision records its output.

## Limits

- **Remotes' default branches come from the caller's
  `refs/remotes/<remote>/HEAD`.** A remote added with `git remote add` and
  fetched has none until `git remote set-head <remote> --auto`; the copy then has
  none either, as on the caller.
- **Remote names** with characters other than letters, digits, `.`, `_` and `-`
  keep their branches out of the defaults, as their upstreams already were.
- **Tags** are carried only when declared.
- **`git fetch` in a copy fails.** A script that fetches before comparing should
  skip the fetch in placed runs (`ELSECALL_CALLER_MEMBER` is set there), or fetch
  on the caller first.
