# Fast first sync: history from the member's own repositories

A follow-up to [S3](remote-run-slice.md) ("Copies"). The first sync of a large
workspace was dominated by its history: an acme-ios worktree took 43.2 s and
sent 331 MB, of which 327 MB was a git pack. This item lets a member take that
history from repositories it already has, and records where each sync spends its
time.

## What changed

- **Object sources.** A member's operator may list repositories on the member that
  may already hold callers' history, such as the account's own clones:

  ```toml
  [workspace]
  object_sources = ["/Users/devbox/Developer/acme-ios"]
  ```

  Nothing in Elsecall knows which project a source holds. Any repository of the
  project will do, however stale. Sources are tried in the order listed. At most 16
  sources are accepted, each an absolute path without `..`.
- **The protocol.** The member searches its sources only when it has some and its
  store lacks the base of the caller's work: the upstream, or HEAD when there is no
  upstream. It says so in `Copy` (`search`).
  - The caller then names HEAD's first-parent ancestors at distances 1, 2, 4 and so
    on, up to 8,192 commits back (`History`).
  - The member takes the missing HEAD and upstream, then those ancestors up to the
    first its store holds. It imports the first of them that a source holds.
  - It answers with the commits it now holds, the imported one first (`Tips`). The
    caller then sends a thin pack against them: local-only commits and whatever
    the source lacked.
  - Otherwise `Copy` carries the tips, as before. So warm syncs, and syncs after a
    local commit on a base the store holds, have no extra round trip and ask no
    source.
- **The import.** A quarantine repository reads the store's and the source's
  objects in place through alternates. There, `git pack-objects --revs` writes one
  pack of exactly the commit's history that the store lacks, reusing the source's
  compressed objects and deltas. The pack moves into the store with its index
  last, and the copy's reference names the commit.
- **Overlap.** The caller's tree listing now runs while the member imports, rather
  than before the exchange.
- **Phases.** Each sync records milliseconds per phase in `placed.sync.phases_ms`:
  - caller: `listing`, `history`, `contents`;
  - member: `verify`, `sources`, `pack`, `staging`, `contents`, `apply`.

  `placed.sync.history_from_source` says whether a source supplied history.

## Trust model of a source

A source is read, never written, and **trusted like the member's own object
store**. This is the contract change of this item.

**Why the source is trusted.**
- The source lives in the same account as the store. Placed commands run as that
  account, so whatever could alter a source's objects could alter the store's
  directly.
- Hashing every object again would protect against neither, and it cost most of
  the first sync.
  - A first version verified everything by fetching through git: `index-pack`
    hashed all 318 MB of history, and the source phase took 7–12 s on the devbox.
  - Packing from the source takes about 2.1 s.
- So a damaged pack in a source could, unnoticed, make the copy's history
  (`log`, `blame`, `describe`) differ from the caller's. git in the source trusts
  those bytes in the same way.

**What is still checked.**
- **Only named commits.** The member takes only a commit the caller named: HEAD,
  its upstream, or one of HEAD's ancestors. Since [workspace refs](workspace-refs-slice.md),
  also a carried ref's commit.
- **Only that commit's history.** Packing walks the commit's history in the
  quarantine. It reads every commit, tree and blob it packs, and stops at the
  commits the store's references name, whose history the store holds.
  - Packing fails on anything missing or unreadable:
    - a shallow source;
    - a shallow branch in an otherwise full one;
    - a damaged commit or tree;
    - an object that is missing.

    The quarantine is then deleted, and the history comes from the caller as
    before.
  - Nothing else of the source enters the store. So "the store holds this commit"
    still means its whole history is there.
  - A first version cloned all of the source's packs instead, and review found the
    flaw: a shallow branch's tip came along and counted as held.
    `a_source_with_a_shallow_branch_supplies_only_complete_history` covers it.
- **All at once.** git sees a pack only once its index is in place. The index
  moves last, so the store never shows part of the history.
- **File content.** Every file taken from history is checked against the caller's
  SHA-256 manifest before it is written to the copy, as all content is. A wrong
  blob is sent by the caller instead.
- **Partial clones** are passed over: their git would fetch missing objects from
  their own remotes.
- **Configuration.** Member-side git ignores global and system configuration, and
  overrides `core.fsmonitor`, `core.hooksPath` and `core.sshCommand` (as before).
  It now also sets `GIT_NO_LAZY_FETCH`, `protocol.allow=never` with `file` allowed,
  and an empty `credential.helper`.
- **What enters the store** is only the new pack, which git wrote itself. The
  source's remotes, refs and configuration never reach the store or the copy; the
  tests search the store for the remote's URL.
- **Cancellation.** Every git command run against a source is killed when the
  sync is dropped, as at its deadline, so a stalled source never holds a copy.
- **A malicious caller** can make the member copy into that caller's store
  objects that a source holds and the caller does not. Placed commands already run
  as the member's account and can read its sources directly, so this exposes
  nothing new. The store is not visible to other callers, and outputs return only
  declared paths.

## Automated evidence

`mise run check` passes. In `tests/workspace_copy.rs`, the existing fidelity tests
are unchanged and pass. Their harness gained a parameter for sources and an
optional link-rate limit. New tests:

- `history_comes_from_a_members_own_clone_and_the_copy_is_still_exact`. The dirty
  caller of the fidelity test, with sources listing a missing path, an unrelated
  repository and the member's clone.
  - History comes from the clone, and the pack is smaller than without it.
  - The copy matches the caller exactly, including `branch.ab +2 -0`.
  - The clone's refs and status are unchanged.
  - The remote's URL is nowhere in the copy's configuration or the store, and no
    quarantine is left.
  - A later commit comes from the caller in a pack under 4 KiB, and no source is
    asked.
- `a_shallow_source_supplies_nothing_and_the_copy_is_still_exact`. HEAD is the
  upstream and a depth-1 clone's tip, without its parent, so only the source could
  supply history.
  - It supplies nothing, the caller sends the history, and the copy's `git log`
    matches the caller's.
  - Without the history walk, this test fails because the caller sends no history.
- `a_source_with_a_shallow_branch_supplies_only_complete_history`. The member's
  clone holds main in full and the caller's HEAD as a shallow-fetched branch.
  - The branch fails to import, and main's history comes from the clone.
  - The caller sends the commits above it.
  - The copy's log matches the caller's, and `git fsck --connectivity-only` passes.
- `bases_lie_at_doubling_distances`, a unit test of the candidates the caller names.
- **Performance:** `a_first_sync_with_an_object_source_sends_only_local_work`. A
  300-commit history of 30 MB of incompressible content over a 100-file tree, over
  a link limited to 32 MB/s. The member's clone predates the caller's last pushed
  commit, so history is found through an ancestor of HEAD.
  - The baseline is the same first sync without a source, into its own store.
  - It asserts that:
    - the pack is at least 100 times smaller;
    - receiving history (the `sources` and `pack` phases) costs under a third of
      the baseline's;
    - the whole first sync is faster;
    - a warm sync sends digests only.

  Release build on this Mac at load about 92:

  | First sync | Time | Sent | Pack | Pack phase | Sources phase |
  | --- | --- | --- | --- | --- | --- |
  | Without a source (the baseline) | 2,711 ms | 31.0 MB | 30.9 MB | 2,220 ms | — |
  | With the member's clone | 774 ms | 191 KB | 107 KB | 50 ms | 209 ms |
  | Warm, after it | 125 ms | 14 bytes | — | — | 0 |

  `repeated_sync_of_a_large_unchanged_tree_is_fast` is unchanged and passes. In the
  same run: cold 9.8 s, warm unchanged 218–268 ms (14 bytes each), one file changed
  238 ms.

## Installed-system evidence

On 2026-11-03 release builds were installed on both members, as in S3:
`mac-mini` (master) and `macbook-pro` (account `devbox`, mapped
locations). The devbox's `config.toml` named its own acme-ios clone as the
only source.

Every first sync is of worktree `elsecall-leases`: 12,885 files, 63.9 MB of content,
whose upstream is the clone's `origin/main`. The command was `git status --short`,
whose output matched the Mac's every time. Nothing was built or tested on the
devbox.

A fresh first sync means Elsecall's own state was cleared first:
- on the devbox, the repository's store, its copies and their records;
- on the Mac, the stat and push caches.

The devbox's load swung between 8 and 140 during the session, so runs are compared
within sequences at similar load.

### Fresh first syncs, phase by phase

The final build, alternating the source off and on. Devbox load about 7, Mac load
62–115.

| Phase (ms) | Source off | Source on |
| --- | --- | --- |
| caller listing | 1,077 | 878 |
| member verify | 205 | 189 |
| member sources | — | 2,107 |
| member pack (receive and index) | 66,720 | 68 |
| member staging from history | 1,150 | 1,238 |
| member apply | 2,658 | 2,549 |
| **Sync total** | **71,950** | **6,738** |
| Sent | 330.7 MB | 3.63 MB |
| Pack | 327.2 MB | 70 KB, the worktree's local-only commits |

- **Another fresh first sync with the source on**, just before, at devbox load
  about 7 and Mac load about 51: 7.2 s, with sources at 2,135 ms.
- **After the import**, the store held 333 MB in two packs: the imported history,
  and the caller's pack of local-only commits.
- **Earlier builds of this item**, measured during the session at devbox load
  12–47:
  - fetching and hashing every object: 12.6–19.5 s, with the source phase at
    7.4–12.2 s;
  - cloning the source's packs: 13.0 and 13.3 s;
  - with the source off: 30.4–55.1 s.
- **The unmodified installed build** took 47.1 s and 67.0 s at devbox load
  80–140. At that load the pack phase alone took 45.6 s.

### History through Elsecall

In the copy, a placed command ran:
- `rev-list --count HEAD`;
- `rev-parse HEAD @{upstream}`;
- `log --format=%H -n 2000` and `ls-files -s`, both hashed;
- `git fsck --connectivity-only`.

Results:
- Everything matched the Mac: 4,187 commits, same HEAD and upstream, same last
  2,000 commits and same index.
- `fsck` exited 0.
- `describe --tags` differed only because tags are not carried, a limit of S3.

### Second worktree and warm syncs

- **Second worktree** of the same repository, `feature-worktree`:
  - before: 7.1 s;
  - after: 7.0 s. It sent a 3.4 KB pack, and the source was asked in 88 ms and
    gave nothing, because its upstream is a review-hub branch the clone lacks.
  - With a development build, a first attempt failed with "changed during the
    sync": someone was editing that worktree at the time, and the sync refuses to
    copy a file that changes while it is read.
- **Warm, unchanged**, 14 bytes each:
  - before this item: 2.87, 0.99 and 0.73 s;
  - after: 0.62 and 0.63 s.
  - One run took 7.3 s, when the caller's own listing took 6.4 s as the Mac's load
    jumped from 54 to 74.

### Where the rest goes

- **Apply** (2.5–7 s; 13 s at devbox load 80+) is now the largest phase. It places
  12,885 files one at a time. git's own steps in it are cheap: refreshing the index
  took 0.64 s, listing untracked files 0.23 s. So placing files from several
  threads is the next lever when the member is loaded.
- **The source phase** (about 2.1 s) is `pack-objects` writing 318 MB of history
  from the source's packs without hashing its objects.
- **Staging from history** (1.2–4 s) reads 10,706 blobs through one `cat-file`.

## Limits

- **Local repositories only.** A remote URL, such as the project's GitHub
  repository or a code-review hub, would need credentials for the member's
  service and network access for its git, which is now off. That would be a
  second adapter behind the same `ObjectSources` interface.
- **The store keeps its own copy.** The first import writes the store its own
  pack of the history, as large as the history itself: 318 MB for acme-ios.
  Later imports add only what the store lacks.
- **One commit per sync.** A source supplies one commit per sync: the first
  candidate it can import, of at most three it tries. The first source that holds
  any candidate wins, even if a later one holds a nearer commit.
  [Workspace refs](workspace-refs-slice.md) lifted the first part: up to four
  imports per sync, for HEAD, its upstream and remotes' default branches.
- **A killed service** can leave its quarantine, holding a partial pack, under
  `<store>/elsecall-incoming/` until that copy syncs again. git never reads it.
- **First-parent ancestry.** Candidates follow HEAD's first-parent chain. A
  long-lived branch that merged main finds only its old branch point, so main's
  commits since then come from the caller.
- **Duplicate imports.** Two copies of one repository syncing their first time at
  once each import the history into the store. The result is correct, but the store
  keeps both packs.
- **No reason recorded.** A source that is passed over gives no reason in the
  record: missing, dubious ownership, another hash, partial or shallow.
