# Configuration sync (E1)

Fleet service settings now also travel with E1 generations through an optional typed
attachment; see [service-config-sync-slice.md](service-config-sync-slice.md) for its
schema, migration and automatic application. The original slice decisions below
describe ordinary mirrored paths and pre-migration local configuration.

Step E1 of [fleet-scheduling.md](fleet-scheduling.md)'s "Secrets and environment
sync" track: the master mirrors declared configuration and secret files and
directories to every member. Consistency warnings only report drift in such
machine-level consumer configuration; E1 removes it for the paths the operator
enables. README "Configuration sync" is the operator's guide.

## Decisions

- **Scope.** Declared files and directories only, each opted in. No environment
  variables, tool logins, keychain items or Elsecall's own `config.toml`.
- **Declared on the master, enabled by the operator.** `[sync] paths` in the
  master's service `config.toml` lists what may be synced; a repository's
  `.elsecall.toml` never can, since it is untrusted. `sync` commands read the section
  when they run. Declaring is not enough: `sync enable <path> --confirm <plan_id>`
  enables exactly the plan the operator saw, and refuses with `plan_changed` if the
  plan's files, classes or members changed since.
- **The synced set travels with the role; declarations gate it.** The enabled paths
  are part of every replica, so no member loses them when the role moves. The master
  syncs only those its own `[sync] paths` declares; the rest are paused (kept as last
  issued, listed as `paused`) until declared there or disabled. A move therefore
  never fails or loses anything for lack of a declaration, and the master never
  syncs a path its configuration does not name.
- **Path rules** (`config_sync::synced_path`, unit-tested): home-relative, printable
  ASCII, plain components, not home itself, no overlap between declared paths, and
  never equal to, inside or containing `~/.ssh`, `~/.gnupg`, `~/Library/Keychains`,
  `~/Library/LaunchAgents`, `~/Library/Application Support/Elsecall`, `~/.elsecall`
  (workspaces, overrides) or `~/.local/bin` (the CLI), compared case-insensitively.
  ASCII is required because APFS folds non-ASCII letters onto ASCII ones (`~/.ſsh`
  opens `~/.ssh`), which no lexical rule can follow. The rules run on the master's
  configuration, on every manifest a member receives (deserializing validates it),
  and again on each member against where the path really resolves: links above a
  synced path (a `~/.config` in a dotfiles repository) are followed, the deepest
  existing directory's real path (`F_GETPATH`) plus the rest of the path must pass
  the rules and avoid the member's workspace root before anything is created, missing
  directories are then created without following links, and the synced entry itself,
  if present, is checked as the file system names it. Nothing below a synced path is
  ever followed (`workspace::confined`).
- **Replica store behind a port.** `ReplicaStore` holds content by keyed
  fingerprint; `FileStore` keeps owner-only files in `config-sync/objects/`. It is
  the storage explicitly meant for secret values: it lets a member rebuild the
  master's content without its own overrides, which is what a master move needs. An
  external secret manager can stand in later behind the same port.
- **Manifests, not content, describe the set.** A manifest lists each path's tree
  (directories; files by owner mode, size and fingerprint) with a generation and
  the issuing member. Its `id` is a keyed digest of the trees alone, so members
  holding the same content agree whoever issued it.
- **Keyed fingerprints.** Secrets are short, so a plain hash in `sync status` could
  be reversed by guessing. Every fingerprint is keyed with 32 random bytes the first
  master creates. The key travels only in replicas (`state.json`, owner-only) and the
  sealed channel; `Debug` redacts it. Output shows 16 hex digits.
- **Sync on events, nothing in the background** (an operator decision during review,
  replacing "pushed when the master's files change, and again on reconnect or
  enrolment"). Configuration changes and new members are rare, and once Elsecall
  lands no agents work directly on members, so what matters is that placed runs see
  current files. Sync therefore happens when a path is enabled or disabled, on `sync
  push` (every member) or `sync push --member` (one member, pushed only if it
  differs), at the handover before a master move, and, from E2, before every placed
  run. Each time, the master first scans the enabled paths, stat-ing every entry and
  reading only files whose size, times, inode or fleet key changed, and issues a new
  generation if they changed. A member that was offline or newly enrolled catches up
  at its next placed run or `sync push`. No member runs anything of its own: its
  overrides and an unfinished application are applied at the next push it receives.
- **Transport.** Pushes and plans use a sealed channel: the existing signed member
  request, upgraded (`elsecall-sync/1`), with keys from an ephemeral X25519 exchange
  both members' enrolled keys sign, as placed runs use. `peer_client::channel`
  generalises the run attach; the purpose `config-sync` is signed in place of a run
  ID and can never equal one. The member's attach route requires `SyncAuthority`,
  which only `Fleet::authorize_sync` creates from the recorded master's key, and the
  member checks the authority again at commit, in case the role moved meanwhile.
  Status (`/peer/v1/config-sync/status`) and asking the master to push
  (`/peer/v1/config-sync/push`) are open to every enrolled member, like the fleet
  view: they carry names and fingerprints only.
- **Atomic, all-or-nothing pushes.** The member answers an offer with the content it
  lacks, receives every blob and verifies its size and keyed fingerprint into the
  store, and changes nothing until `Commit`. Applying first checks every path and that
  all content is present, journals the manifest (`applying`), then replaces each path
  in one step: the new tree is built owner-only beside it and swapped in with
  `RENAME_SWAP` (a file is replaced the same way), and the old tree removed. A failure
  before the swap leaves the path as it was; an interrupted application is finished
  at the member's next upkeep.
- **Owner-only storage at the declared path.** Directories are 0700; files keep the
  master's owner bits (0600, or 0700 for an executable, 0400 for read-only) and never
  group or other access, which is how permissions are mirrored without widening them.
- **Overrides** live at `~/.elsecall/sync-overrides/<path below home>`, a directory
  each member creates 0700 and that must stay owner-only. Its files win at the
  declared path (`compare::overlay`); an override that is a single file replaces the
  path. They are applied at each sync and never written by sync.
  Replicas exclude them. On a master, overridden files keep the values last issued
  (`compare::issued_from`, which restores every fleet entry an override's shape
  covers), so a master with overrides never pushes them. A master writes only those
  files, one at a time in place, never a whole tree and never a directory, so its
  other files, the source, including links and `.git`, are never touched. When an
  override on the master is removed, the file still holding its value is restored to
  the fleet's value, never issued from it.
- **Local edits** are classed by comparing what is there with what sync last wrote
  (`written`) and what it will write. Content that is neither is the member's own:
  `sync status` reports it as `locally_edited` (or `changed`/`removed` with
  `backup: true` before a path's first sync), and the next sync backs it up owner-only
  in `config-sync/backups/` before replacing it. Backups are taken once the new tree
  is built, just before the swap, recorded before it, and skipped when the newest
  backup of that file holds the same content, so a failing write cannot churn them.
  Their fingerprints use a key only that member holds. A member that issued its
  manifest as master treats everything it holds as its own at the first push from
  another master, so a former master's unpushed changes are backed up, not
  overwritten. A member whose copy holds entries sync cannot carry (links, special
  files, `.git`) keeps its whole previous tree as a backup.
- **Taking over.** The moment a member's recorded roster makes it master (a
  `RoleObserver` hook on `Fleet`, called wherever the roster changes), it applies its
  replica as a member would, backing up local edits. Drift from before the move
  therefore never becomes the fleet's truth, and edits made on the new master after
  the move are its own. A member that loses the role stands down, so a later return
  restores again. Restoring at the first sync command instead, as an earlier
  revision did, reverted an edit made on the new master between the move and that
  command (found on the installed machines; it was backed up, not lost).
- **Records** (`config-sync/history.jsonl`, owner-only, the latest 500 to 1,000) hold
  what synced: generation, manifest id, member, paths, file names, changes, sizes,
  short fingerprints and the outcome code. Log lines name events, members,
  generations and codes only.
- **Handover before a master move.** `fleet move-master` calls a `RoleHandover` the
  service registers on `Fleet` (a hook, so fleet membership does not depend on sync):
  the old master scans, issues any change and pushes (asked over the member channel
  when the move runs elsewhere), then holds still for 30 seconds, and every member's
  manifest id is checked. Without `--force` a failure refuses the move. Forced, the
  report says what may be lost. A member whose sync state could not be opened
  registers a handover that refuses unless forced, so a move never skips the check.
- **Locking** (changed after E2; first a single lock serialised every sync, so a
  slow push to one member delayed placed runs everywhere). The master scans,
  issues, changes the synced set and follows role changes under one `issuing`
  lock, which covers local work only. Exchanges with members (pushes, generation
  checks, plans) take that member's lock (`member_locks`): one at a time per
  member, independent across members. Each push sends the latest issued
  generation, read under the member's lock, so a member never goes back to an
  earlier one, and pins its content (`pins`), so a generation issued meanwhile
  does not remove what the push still has to send. Every push is a whole issued
  generation; one issued during a round reaches a member it had not reached at its
  next sync, and the round's deliveries carry the generation each member got. The
  role is checked again once a member's lock is held. A handover first takes every
  member's lock, in name order, and only then starts its hold and issues: nothing
  else reaches a member until the round ends, and every member gets exactly the
  checked state. `enable` refuses with `plan_changed` if a path was enabled or
  disabled between its plan and its issue.
- **Removal** stops pushes, since the master pushes only to roster members. The
  removed member keeps its files; after `fleet leave` it is a fleet of one whose own
  files are its source.

## E2 hook

E2 (sync-then-run before placed runs) is [pre-run-sync-slice.md](pre-run-sync-slice.md).
The master calls `ConfigSync::bring_up_to_date(&member)`, the call `sync push --member`
uses, before every offer of a placed run: it issues any change to the master's files,
asks the member which generation it holds, and pushes only if it differs (an
unapplied override or a local edit counts). E1's sync semantics are unchanged.
When it fails, a placeable run is placed again without that member, and a run
required there fails with `config_sync_failed`.

## Security review

A review focused on security found, among others: a bypass of the forbidden paths
through APFS's folding of non-ASCII letters (`~/.ſsh` is `~/.ssh`, verified on the
machine); a master with an override rewriting its whole tree (dropping `.git` and
links) at each generation; a removed master override being pushed as the fleet
value; a new master's local drift becoming truth; members dropping links and `.git`
without a backup; backup churn under a persistent write failure; unverified store
reads; directories created before the safety check; and a handover skipped when sync
could not open. A second pass over the fixes found that a failed master
application could leave its record of what it wrote ahead of the manifest (now all
or nothing), one master-side override conflict stopping all issuing (now skipped and
shown), pruned kept trees not deleted, a takeover ignoring an unfinished
application, issuing during the handover hold, and member upkeep racing an incoming
push's store cleanup. Each is fixed as the decisions above describe, with tests:
`a_masters_overrides_stay_local_and_never_rewrite_its_files`,
`paths_the_master_no_longer_declares_are_paused`, and the kept-tree and new-master
assertions in the plan and move tests, plus unit tests for the path rules, shape
changes under overrides, case collisions and store verification.

## Automated evidence

`mise run check` passes. `tests/config_sync.rs` runs real member services in
temporary homes, loopback standing in for the tailnet, and covers:

- the plan's per-file classes (`added`, `changed` with backup, `removed` with
  backup), that planning changes nothing, `not_master` on a member, `not_declared`,
  and refusal of `~/.ssh` before anything happens;
- enabling only with the plan's ID (`plan_changed` otherwise); the replica's
  content, owner-only modes (0700, 0600, an executable 0700) and backups of the
  member's previous content;
- pushes on change: content, a deletion and a permission change;
- nothing moves without a sync event; `sync push --member` pushes to a member only when it differs (an override not yet applied counts), and asks only for its generation otherwise;
- overrides applied at the next sync, kept through later generations, and listed;
- a local edit reported as `locally_edited`, backed up once at the next push and
  replaced; a second push backs up nothing;
- no synced value or override in logs, `state.json`, `history.jsonl`, `sync status`,
  `sync plan`, `sync history`, `fleet`, `runs` or `explain`, by a sentinel value;
- a push cut off mid-content, and one with all content but no commit, change
  nothing; content not matching its fingerprint is refused; a member that is not
  master is refused (`not_master`); an offer naming `~/.ssh` is refused as invalid
  input; a complete exchange applies, and the real master restores its state;
- moving the master: the final push carries a change made just before the move,
  overrides stay local, the new master pushes and the old one only receives, and the
  move back from the non-master member;
- a forced move with the master down: the warning, the new master's replica as
  truth, and the old master's unpushed change backed up when it returns;
- a removed member keeps its files and gets no more pushes, also after `leave`;
- disabling: files stay, the path leaves status, and replicas are dropped;
- a member whose `~/.config` links into `~/.ssh` refuses the path in the plan and
  the push (`unsafe_sync_path`) and nothing is written there.

Unit tests cover the path rules, keyed fingerprints, manifest validation, the
replica store, reading and atomically replacing trees with links refused, overlays,
classification, state and backups bounds, records and channel framing.

## Installed-system evidence

Verified on 2026-11-03 with release builds of this branch on both members
(`mac-mini`, master, desktop session; `macbook-pro`, the devbox,
background session). `install` updated both in place; both firewalls stayed
`allowed`. `elsecall fleet` showed both reachable, the Mac's `config_sync` as
`source` and the devbox's as `off` before anything was enabled.

**Plan for `~/.config/acme-ios` (nothing enabled).** With the path declared in
the Mac's `[sync] paths`, `sync plan ~/.config/acme-ios` on the Mac reported,
by class only:

- source (Mac): 5 files, 1,424 bytes, nothing skipped, no overrides;
- `macbook-pro`: 5 files `unchanged`, and 1 file `removed` with `backup: true`:
  `agent-device/.agent.staging.env.bak-20261021`, which exists only on the devbox.

So the two machines already hold the same five files. Enabling the path would back up
and then remove the devbox's one extra backup file; keeping it would need it moved
outside the synced directory first. The operator decides whether to enable it.

**Plan for the throwaway `~/.config/elsecall-sync-demo`.** Three files on the Mac
(`.env`, `nested/a.txt`, an executable `run.sh`), none on the devbox: the plan showed
all three `added`, no backups.

**Round trip with the demo path** (on the build that syncs on events only). This
session's command guard refuses command lines containing `sync enable`/`sync disable`
(it reads them as shell builtins), so the operator ran those from a terminal.

- `sync enable … --confirm aff1ddc32664a5d1`: generation 1, the devbox `applied`
  three `added` files. SHA-1 of every file matched on both machines; on the devbox
  the directories were 0700, `.env` and `nested/a.txt` 0600 (0644 on the Mac) and
  `run.sh` 0700 (0755 on the Mac).
- A changed `.env`, a deleted `nested/a.txt` and `run.sh` made 0644 on the Mac
  changed nothing on the devbox until `sync push`, which issued generation 2:
  `changed`, `removed`, `mode_changed`; the devbox then matched, `run.sh` 0600.
- `fleet move-master macbook-pro` on the Mac: `config_sync.status: in_sync` at
  generation 2 (epoch 6). With the devbox's `config.toml` empty, `sync status` listed
  the path as `paused` on the new master. Declared there, an edit on the devbox went
  out as generation 3 and the Mac matched.
- That step first exposed the takeover defect described under "Taking over": the
  edit made right after the move was backed up as `locally_edited` and replaced at
  the devbox's first push. Fixed and covered by the move test; with the fixed build
  installed on both, the same steps (move to the devbox, epoch 8, edit there at once,
  `sync push`) sent the edit as generation 4, and the Mac matched.
- `fleet move-master mac-mini`, run on the Mac while it was not master:
  `in_sync` at generation 3 (epoch 7), and again after the re-check at generation 4
  (epoch 9). The Mac is master again.
- `sync disable … --confirm` (operator): generation 5 with no paths, applied on the
  devbox; both kept their files. A first attempt with the enable plan's ID was
  refused with `plan_changed`, whose message now says the ID does not match the
  current plan rather than that the set changed.

Afterwards the demo directories were deleted on both machines, the empty overrides
directories removed, and the Mac's `config.toml` restored; `sync status` shows nothing
synced or declared.

The devbox's `config.toml` was restored to empty afterwards.

## Limits

- Pushes are not atomic across paths: each path is replaced in one step, in order.
  A failure after some paths were replaced (an I/O error, not a validation or
  missing-content failure, which are checked first) leaves the application journaled
  and finished at the next upkeep.
- The member checks the master's authority when the channel opens and again at
  commit. A role that moves between the commit check and the swap is not detected;
  the old master's content is then replaced at the new master's next push.
- On a master, overridden files cannot change their fleet value; remove the override
  on the master, or change the file from a member that becomes master.
- Links, special files and names that are not UTF-8 or name `.git` inside a synced
  path are not synced. On members they are kept, with the whole previous tree, as a
  backup when sync replaces the path; on the master they stay where they are.
- Directory permissions are not mirrored: members' synced directories are 0700.
- Fingerprints are SHA-256 over the key and content rather than HMAC; output is
  truncated to 64 bits, and length extension does not apply to this use.
- Members verify the content in their store when it is read, so a corrupted object is
  dropped and sent again; replicas are trusted otherwise. On the master, a dropped
  object is stored again only when that content is next issued.
- Names inside a synced directory may be non-ASCII. Manifests refuse names equal
  when lowercased, but not every pair APFS treats as equal (`ſ`/`s`, NFC/NFD); only
  a master could send such names, and a member then holds one of them.
- A member's local edits are shown with fleet-keyed fingerprints, which other
  members could test guesses against; overrides and backups are not.
- An override on the master that does not fit the master's files (a file where a
  directory is) is left unlaid, logged, and shown as a difference in `sync status`;
  the rest keeps syncing.
- A master write interrupted by a crash can leave a `.elsecall-<hex>` temporary
  inside the master's tree; sync ignores such names but does not remove them there.
- Kept whole trees count against the 20 backups per path; their size is not
  bounded beyond the 4,000-entry and 64 MiB limits of what sync itself carries.
- A tool that rewrites a synced file on a member, such as a token refresh, is
  overridden at each sync (with a backup, at most 20 per file). Such files belong in
  the member's overrides, or outside synced paths.
- A member that missed a push (offline, newly enrolled) catches up at its next placed run or the next `sync push`.
- `uninstall` leaves `config-sync/` (replicas and backups) in the state directory
  and reports `files_preserved`, as it does for other state.
