# Housekeeping of workspace copies

[Fleet scheduling](fleet-scheduling.md) says of workspace copies: "Housekeeping
cleans them up, never mid-run." This slice delivers that cleanup. Copies stay warm between runs, so they
keep their ignored build state. An acme worktree copy is 99 MB plus a shared
325 MB object store, and once builds run remotely its DerivedData reaches several GB.
Callers create and delete many worktrees a day. Without housekeeping, a member's disk
fills.

It came in three stacked items:

1. **elsecall#18** (merged): idle expiry, disk pressure and stores, with
   `elsecall copies` and `elsecall copies prune [--dry-run]`.
2. **elsecall#22** (merged): copies whose workspace the calling member deleted, and
   caller-side cleanup.
3. **This item:** copy disk use in `elsecall fleet`, the documentation and
   installed-system evidence.

## Behaviour

A member removes, in housekeeping passes:

| Reason | When |
| --- | --- |
| `caller_deleted` | The copy's calling member says its workspace no longer exists. |
| `idle` | The copy was not used for `idle_after` (14 days). |
| `disk_pressure` | Free space on the copy's volume is below the larger of `min_free_percent` (10%) and `min_free` (50 GB). Copies not in use go, least recently used first, until the volume recovers. |
| `unreferenced` (stores) | No copy uses the object store any more. |
| `missing` | The copy's directory was already gone. Only Elsecall's state of it is removed. |

Every removal is recorded with its reason, its size and its last use, and kept for 90
days. A pass also:
- finishes deletions an interrupted pass started;
- adopts copies it did not track yet;
- measures copies used since they were last measured;
- compacts stores that have `compact_store_packs` (20) packs or more;
- tells each calling member which of its copies went;
- tidies the member's own caller-side sync state.

Passes run every `interval` (1 h) as the service's own deferrable, pinned work. The
first pass comes `interval` after the service starts, or five minutes, whichever is
sooner. A pass that cannot start within its interval is skipped
(`at_deadline = "skip"`). Every default is a setting under `[housekeeping]` in
`config.toml`.

## Decisions subsequent slices can build on

### Never mid-run, never racing a sync
- **One holder per copy.** A run claims its copy from the moment the member admits it,
  so the claim also covers a run that is about to attach. The claim lasts until the
  run ends. Housekeeping *reserves* a copy before removing it, and *locks* an object
  store before removing or compacting it. While it does, offers for that copy, or for
  any copy of that store, are refused as `CopyBusy`, so they wait or go elsewhere.
  Syncing happens only inside a run's claim, so removal and sync are serialized
  (`workspace::copies`).
- **Nested same-path copies.** A same-path copy can sit inside another's directory, as
  a worktree inside its main checkout. Housekeeping never reserves a copy while a run
  holds one nested in it or around it, and the reverse also holds. A copy that holds
  another copy is removed only after the inner one (`contains_copy`).
- **Judged, then rechecked.** A copy is judged on its last use. If a run used it
  between the judgement and the reservation, the copy is judged again next pass.
- **Last use.** A copy counts as used when the member admits a run for it and again
  when that run ends. A refused offer is not a use.

### Removing safely
- **What is checked.** Housekeeping removes a copy only if its directory carries the
  copy's mark: the calling member and caller path written before git initialized it.
  The check runs through an open handle, never through a link. Stores must be bare
  repositories under `<root>/.stores/<calling member>/`.
- **Move, then delete.** The directory is checked first, then moved aside in one
  atomic rename to `<root>/.trash`, or next to itself on another volume. It must be the
  same directory, by device and inode, after the move. Anything else is moved back
  untouched and no longer tracked. Deletion walks from open directory handles and never
  follows links. A pass interrupted after the move finishes the deletion next pass, but
  only while the directory is still the one it moved.
- **With the copy go** its sync record in `copies/` and its references in the store.
  A copy found missing keeps its store references, since its volume may be away.
- **Stores.** A store goes when no tracked copy uses it *and* it holds no references
  from copies the member does not track, such as same-path copies from before
  housekeeping. Both are checked again under the store's lock. Compaction runs
  `git repack -d --geometric=2` on one thread. It combines small packs whole and never
  drops an object, since copies' indexes and their own repositories may name objects
  that no reference reaches.

### Disk pressure
- Free space is read per volume that holds copies or the workspace root. Free space
  below the threshold is pressure.
- Copies are evicted only if evicting every idle copy on the volume, and the stores
  only they use, could make up the shortfall. If not, none is evicted, the pass reports
  `unrecoverable`, and the service logs `disk_pressure_unrelieved`. Evicting warm copies
  that cannot fix the disk would only make the next builds cold. See "Contract
  questions".
- A real pass stops evicting as soon as the volume measures above the threshold. Sizes
  count clones in full, so they may promise more than a removal frees. If what was
  evicted should have been enough but the volume does not show it yet, the outcome is
  `pending`, as when local snapshots hold the data. The next pass checks again.

### Deleted on the caller
- **The owner alone decides.** While `remove_deleted_workspaces` is on (the default),
  each pass asks every calling member that owns copies here which of their workspaces still exist: `POST /peer/v1/workspaces/check`, signed
  and verified like every member request. A copy is removed for `caller_deleted` only
  on its owner's answer. Copies of an unreachable owner are judged by idleness alone,
  and the report names the owner and the error.
- **The owner answers only for itself.** It answers only about workspaces it pushed to
  the member that asks; anything else is `unknown`. No other enrolled member learns
  which of its paths exist, or can make a copy go.
- **Gone means gone.** A workspace is `gone` when its path no longer exists. A path on
  an unmounted `/Volumes/<name>` is `unknown`. A copy used after the question was asked
  is kept, as when the path was reused.
- **Telling the owner.** After a pass, the member tells each calling member which of
  its copies it removed. Pending removals are tracked by their order in the record.
  The owner drops what it pushed to that member:
  - for a workspace that no longer exists, whenever the push was made, since nothing
    can push it again;
  - otherwise only a push older than the removal by more than a minute, so a push the
    member had not yet seen survives.

  An owner this pass could not reach is told next pass.
- **Caller-side cleanup.** A caller now records which path each workspace's sync state
  stands for (`<id>.workspace`). Its own pass removes:
  - the stat caches of deleted workspaces;
  - what it pushed to members no longer in the fleet;
  - what a deleted workspace pushed longer ago than the idle period, in case a
    member never tells;
  - the record of a deleted workspace once nothing else of it is left.

### Visibility
- `elsecall copies`: each copy's calling member, caller path, location, setting, state
  (`in_use` or `idle`), last use, when it becomes idle long enough to remove, size and
  measurement time. Also each store with its size and copy count, the volume's free
  space against its threshold, the settings, the last pass, and the latest 50 removals.
  Sizes are as last measured, so the listing is quick.
- `elsecall copies prune [--dry-run]`: one pass now, reporting what was removed, spared,
  compacted and kept, the owners asked, disk pressure, caller state and problems. It
  exits 1 when the pass met problems. A dry run changes nothing on disk except the
  measurements and adoptions it records. A manual pass runs at once, without admission.
- `elsecall fleet`: each reachable member's `status.copies` shows:
  - its copies and stores with their bytes as last measured, and how many copies are
    not measured yet;
  - its workspace volume's free space against the threshold (`disk`);
  - its last pass since the service started.

  If they cannot be read, `status.copies_error` says why. Members that predate this
  slice leave both out.
- `elsecall runs`: each periodic pass has a record labelled `elsecall housekeeping`.

### Scheduling
- A member now refuses placed work only while its own *interactive* requests wait.
  Without this, a queued housekeeping pass would refuse every placed run for up to an
  hour. `fleet-scheduling.md` says so.

## Automated evidence

`mise run check` passes. Besides unit tests of the policy, settings, catalog, trash,
exchange, caller state and claims, these suites cover the behaviour.

### `tests/housekeeping.rs`
Real services in temporary homes on loopback. Only these are replaced: tailnet
discovery, machine load, the housekeeping clock (`ELSECALL_HOUSEKEEPING_CLOCK`) and,
for disk pressure, the volume (`ELSECALL_SIMULATED_VOLUME`).

- **Listing, dry run and the idle boundary.**
  - The listing's fields.
  - A dry run leaves the copy, its build state, the store, its references and the sync
    record in place.
  - One millisecond before 14 days the copy stays; at 14 days it goes, with its store
    and sync record.
  - The removal records.
  - A fresh copy on the next run.
- **Shared stores.** One worktree's copy goes; the store stays for the other, which
  still reads its history. Then both go.
- **Disk pressure.** Three copies, one used again:
  - The two least recently used go, in that order, and the volume recovers.
  - When even all idle copies could not cover the shortfall, none goes, and the
    outcome is `unrecoverable`.
- **Never mid-run.** A running copy is spared, idle and under pressure alike, and goes
  once the run has ended and is idle long enough.
- **Never racing a sync.** In-process, with the real sync, a sync stalled after
  `Begin` (the copy open and checked) holds its copy and store through a pass, then
  completes into the intact copy.
- **Safety.**
  - A same-path copy that holds a nested copy is removed after it.
  - A stranger's directory at a copy's location is left untouched, with a
    `housekeeping_refused` problem and exit 1.
  - A store with references from an untracked copy is kept, and that copy still reads
    its history.
- **Adoption.** A copy made while no catalog existed is found and listed.
- **Compaction.** Small packs are rolled up and every commit stays reachable.
- **Scheduling.**
  - A periodic pass runs as deferrable, pinned work with `skip`, and removes an idle
    copy.
  - A queued deferrable request does not keep a placed run out. This test fails on the
    old rule with `fleet_busy`.
- **Fleet view.**
  - Another member's `status.copies`, with its disk and its last pass after a real
    pass.
  - A unit test reads statuses with and without the new fields in both directions.

### `tests/housekeeping_callers.rs`
- **Deleted on the caller.**
  - While the worktree exists, the copy stays.
  - Once it is deleted, a dry run plans `caller_deleted`, and the pass removes it while
    the other worktree's copy and the shared store stay.
- **Caller state.** After the member's pass, the caller has dropped what it pushed for
  the removed copy. After the caller's own pass, nothing of the deleted workspace is
  left. A copy removed for idleness drops only that member's push.
- **Only the owner.**
  - With the owner stopped, the copy stays and the report names the error.
  - A third member's signed question about the owner's workspace gets `unknown` and
    cannot make it forget anything.
  - The member that holds the copy gets `gone`, and the owner's answer removes it.
- **A scripted owner at the `Callers` port** (in-process, with the real pass and
  catalog):
  - answers about paths the owner was not asked, and about the same path owned by
    another member, are ignored;
  - a copy used while its owner is asked is kept.

## Installed-system evidence

On 2026-11-03 the release build of this item was installed on both members:

- **`mac-mini`:** master; desktop session; the calling member.
- **`macbook-pro`:** account `devbox`; background session; mapped locations; an empty `config.toml`, so every default applies.

The first build was `0b9667029ea44aef…`. A gap found below was fixed and the result reinstalled as build `904fb82fe4d12863…`. The final build, `fd4eb7474e3baeb0…`, changed only the fleet view's shape, and the fleet view below is from it. Earlier slices' sessions had left four copies on the devbox:
- one acme-ios worktree copy, `elsecall-leases`. S3's second acme copy (`feature-worktree`) was already gone, with the Mac's sync state for it;
- S3's scratch repository;
- two scratch repositories from S4.

Each copy has its own object store, except that acme's history sits in its 325 MB store.

### Disk pressure the copies cannot relieve
The devbox's data volume has 994.7 GB with 58.9 GB free (94% used). The threshold is the larger of 10% and 50 GB, so 99.5 GB: the devbox is under pressure. All its idle copies and stores together hold 0.44 GB, far short of the 40.5 GB deficit. Every pass therefore reports `unrecoverable` with `evicted: 0` and `evictable_bytes: 438874112`, keeps the warm copies, and logs `disk_pressure_unrelieved`. `elsecall fleet` on the Mac shows the devbox's `disk.pressure` as `true`. See contract question 1.

### Listing the existing copies
On the devbox, `elsecall copies prune --dry-run` took 418 ms. It:
- found the 4 copies (`adopted: 4`);
- measured them;
- asked the Mac about their 4 workspaces. The Mac answered `gone` for the 3 scratch repositories, which no longer exist, and `present` for the acme worktree;
- would remove the 3 scratch copies (`caller_deleted`) and their 3 stores, and removed nothing.

`elsecall copies` then listed:

| Calling member | Caller path | Location | Size | Last used | State |
| --- | --- | --- | --- | --- | --- |
| mac-mini | …/s4-hash | `mac-mini/s4-hash-3de1ce849e949309` | 119 KB | 09:07:56 | idle |
| mac-mini | …/s4-scratch | `mac-mini/s4-scratch-05c5f5f354866588` | 119 KB | 08:51:56 | idle |
| mac-mini | …/d39c7533/tmp/evidence/scratch | `mac-mini/scratch-de124f4205ebef55` | 135 KB | 2026-11-02 20:43:58 | idle |
| mac-mini | `/Users/me/Developer/acme-ios/.claude/worktrees/elsecall-leases` | `mac-mini/elsecall-leases-3060b8ae321d1b4d` | 104.4 MB | 09:20:51 | idle |

Times are UTC on 2026-11-03 unless stated. Each copy can be removed for idleness 14 days after its last use. The listing also showed:
- 4 stores, among them acme's `2856e1cffe657239.git` at 334.5 MB;
- 439.6 MB in all;
- the volume under pressure.

Adopted copies take their last use from what Elsecall last wrote to them.

### Pruning a copy whose workspace was deleted
1. A scratch repository with a commit and an edit was created on the Mac.
2. It ran on the devbox: `elsecall run --member macbook-pro`, exit 0. Its copy was `mac-mini/hk-scratch-20d097fc9e556f58`.
3. The scratch repository was deleted on the Mac.
4. `elsecall copies prune` on the devbox took 391 ms:
   - it asked the Mac about 5 workspaces; 4 were gone;
   - it removed the scratch copy and the three older scratch copies (`caller_deleted`), with their 4 stores;
   - it kept the acme copy, whose worktree exists;
   - it reported no problems.
5. The devbox now holds only `elsecall-leases-3060b8ae321d1b4d` and its store `2856e1cffe657239.git`. `.trash` is empty.
6. The 8 removals are recorded: 4 copies, each with its caller path, size and last use, and 4 stores.

**Caller state.**
- **Before the fix.** The Mac dropped its pushes for the three older copies. It kept the push for the new scratch copy, because that push was less than 60 s older than the removal. The devbox had told it, so it would never be told again.
- **The fix.** A removal notice for a workspace that no longer exists now drops the push whenever it was made, since nothing can push it again. As a backstop, a caller's own pass drops pushes of a deleted workspace older than the idle period.
- **With the fixed build** (`904fb82f…`), the same steps with a second scratch repository:
  - the devbox removed its copy and store;
  - the Mac dropped the push;
  - the Mac's own `elsecall copies prune` removed the stat cache and the registry (`caller_state.files: 2`);
  - nothing of that workspace is left in the Mac's `workspaces/`.
- **What remains.** The Mac keeps its state for the acme worktree. It also keeps the first scratch's push and registry, which the backstop removes within 14 days.

### The fleet view
`elsecall fleet` on the Mac shows each member's copies. Just after the restart that installed the final build, `last_pass_at` is null on both:
- the Mac: 0 copies, 1.67 TB free against a 400 GB threshold;
- the devbox: 1 copy of 104.4 MB (`unmeasured_copies: 0`), 1 store of 334.5 MB, 59.2 GB free against 99.5 GB, and `disk.pressure: true`.

### Periodic passes
About five minutes after the service started, each member ran its first periodic pass. Each pass is a deferrable, pinned request labelled `elsecall housekeeping` in `elsecall runs`, and both ended with exit 0:
- the Mac's, `r-37d6aa9ef87d`, waited 107 ms and ran for 115 ms;
- the devbox's, `r-4813c406b93c`, waited 1 ms and ran for 63 ms.

The devbox's pass kept the acme copy, and again reported its disk pressure as `unrecoverable`.


## Limits

- **Stores keep unreachable history.** Compaction never drops objects, so history
  from removed worktrees stays in a store until the store itself goes.
- **Untracked same-path copies** from before housekeeping are not found until a run
  uses them, and their stores are kept while they may exist. Mapped copies are found
  by their marks.
- **A lost catalog** (`copies.sqlite`) loses last uses; adopted copies take the time
  Elsecall last wrote to them.
- **Sizes** are allocated blocks with clones counted in full, measured during passes.
  The listing shows when each was measured.
- **The caller's existence check** looks only at the path. A worktree deleted and
  recreated at the same path counts as present.
- **Manual passes** (`copies prune`) run at once, outside admission.
- **The last pass** is kept in memory. Listings and the fleet view show none after a
  restart until the next pass. Dry runs do not count as passes.
- **One volume in views.** Passes judge pressure on every volume that holds copies,
  but `elsecall copies` and `elsecall fleet` show only the workspace root's. A
  same-path copy on another volume may be under pressure while they show none.
- **Test seams.** `ELSECALL_HOUSEKEEPING_CLOCK` and `ELSECALL_SIMULATED_VOLUME` are
  honoured by any service that has them set, and the service logs `test_seam_in_use`
  when they are.

## Contract questions

The most literal reading was implemented, except where noted.

1. **Disk pressure that copies cannot relieve.** "Evicted until it recovers" was read
   as: evict only when that can recover. When every idle copy together is smaller than
   the shortfall, nothing is evicted and the pass reports `unrecoverable`. The devbox
   shows why: 58.9 GB free of 994.7 GB against a 99.5 GB threshold, with 0.44 GB of
   copies and stores. Should a member instead evict everything idle, or what it can, even when
   that cannot reach the threshold?
2. **Placed work and queued deferrable work.** "A member refuses placed work while its
   own requests are waiting" now means its own *interactive* requests, so housekeeping
   never keeps placed runs out. The contract was updated; please confirm.
3. **Manual passes** run outside admission, because the operator asked for one now.
   Should `copies prune` queue as interactive work instead?
4. **Unreachable owners.** A copy whose owner left the fleet, or never answers, goes
   only when idle. Should leaving the fleet remove its copies at once?
