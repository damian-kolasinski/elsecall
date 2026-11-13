# Sync before placed runs (E2)

Fleet service settings now also travel with E1 generations through an optional typed
attachment; see [service-config-sync-slice.md](service-config-sync-slice.md) for its
schema, migration and automatic application. The original slice decisions below
describe ordinary mirrored paths and pre-migration local configuration.

Step E2 of [fleet-scheduling.md](fleet-scheduling.md)'s "Secrets and environment
sync" track. Before a placed run starts on another member, the master brings that
member up to date with the current generation of every synced path
([config-sync-slice.md](config-sync-slice.md), E1), so a placed run never reads stale
configuration. README "Running on another member" and "Configuration sync" are the
operator's guide.

## Decisions

- **Where.** The master's offer loop (`scheduling/service/placing.rs`) runs the sync
  immediately before each offer to the target member, for required (`--member`)
  and placeable runs alike. While it runs, the request is in flight
  (`InFlight::Syncing`), so placement does not move it. Syncing just before the
  offer means the member is current when it admits the run. Syncing after
  admission would hold the member's 15 s attach window open during a push.
- **A port in scheduling.** `scheduling::pre_run_sync::PreRunSync` is placement's
  view: "bring this member up to date", answered with `NothingSynced`, `Current` or
  `Pushed` (per path: files changed and bytes). `placement_config_sync` adapts it
  over `ConfigSync::bring_up_to_date`, which E1's `sync_member` now wraps unchanged.
  E1's sync semantics are unchanged: the master scans its enabled, declared paths,
  issues a generation if they changed, asks the member for its generation, and
  pushes only if it differs (a local edit or an override not yet laid counts).
- **Cost.** A member that is already current costs one status request after the
  master's scan of its synced paths. A run on the calling member never reaches the
  offer loop and syncs nothing. A fleet that syncs nothing costs a read of the
  local replica state.
- **Time counts.** The sync runs as its own task, and the offer loop waits for it
  at most until the hold limit. A sync is therefore never cancelled halfway through
  (between E1's blocking steps, or mid-exchange): one that outlasts the hold limit
  finishes on its own, and `sync history` records it. The offer
  is built afterwards, so `start_within_ms` gives the member only what is left. The
  sync falls between the request and the start, so it is part of `waited_ms`, never
  of `run_ms`, and learned run times are unaffected. It is not added to placement's
  estimates: a check takes milliseconds and a push of a few small files well under
  a second (see the installed evidence), far below the scoring margin of
  `max(2 s, 10%)`. A decision is recorded when the run starts, after the sync, so
  its candidates' expected completions already count from after it.
- **Failure.** A sync that fails on a required (`--member`) member fails the run
  with `config_sync_failed`, and the command does not run. So does a sync still
  running when the hold limit passes, provided no offer to that member was
  attempted yet: a request that waited for room, or for an unreachable member, and
  expired during one of the re-checks between offers is `fleet_busy` as before.
  Placeable runs fall back instead (see "Follow-up: fallback and per-member
  locking" below); E2 first failed them too.
  `details` hold the `member` and the underlying `cause` code. The cause is
  repeated only if it is a short snake_case identifier; anything else becomes
  `member_error`, and the cause's message is never repeated, since it may come from
  the member. The decision's verdict is `config_sync_failed`. The failure line ends
  with `elsecall sync push --member <name>`, which shows why and retries.
- **An unreachable member is not a sync failure.** When the member cannot be
  reached (`member_unavailable`, `member_rejected`, `member_unverified`), the
  outcome is the same as an offer that could not reach the member. A required run
  fails with `member_unavailable`, in Elsecall's own words. Placement chooses again
  for a placeable run. When this member lost the master role meanwhile
  (`not_master`), the run fails with `member_unavailable`, saying so, and the
  member is not marked unreachable. This keeps the existing contract for a member
  that is down and for a master move.
- **Broken sync state.** When the master's configuration sync could not be opened,
  every placed run fails with `config_sync_failed`, as a master move refuses
  without `--force`, since the member's configuration cannot be ensured.
- **Consistency warnings.** The grant carries the synced paths the member was just
  brought up to date with (`PlacedGrant.synced_paths`). The caller drops declared
  `~/` locations that are, or lie inside, such a path before fingerprinting
  (`consistency::Declared::without_synced`). Such locations are current by
  construction, and any difference is an override, which is intentional: a
  member's own override, or one on the master, whose runs then read the override
  while members read the fleet value. A location is still compared when:
  - it contains a synced path, or is absolute;
  - it is reached on the caller through a link, a special file or a `.git`
    directory below the synced path, none of which sync carries;
  - it lies inside a paused path (one the master no longer declares), since only
    paths the master issues from its own files count.

  Toolchains and other locations warn as before.
- **Records.** On the master, the record's `config_sync` holds `member`,
  `duration_ms` and `outcome`:
  - `nothing_synced`;
  - `current`, with `generation` and `paths`;
  - `pushed`, which adds `pushed`: per path, the files changed and their bytes;
  - `failed`, with `cause`.

  The member's own overrides are left out of the counts. File names, fingerprints
  and values are not kept here; `sync history` keeps names and fingerprints. `runs`
  shows `placed.config_sync_ms`. A sync that succeeded before an offer that then
  failed (`fleet_busy` while offered, `request_too_large`, a cancel) is kept in the
  record too, but only from the latest attempt: each new sync replaces it. A sync
  that outlives its run's hold limit finishes on its own and shows only in `sync
  history`. The member's record of the hosted run is unchanged. Its
  configuration sync record is in its `sync history`.

## Review

The code-review pass, focused on security, found:
- an expiry during a re-check between offers to a busy member being blamed on the
  sync, with the wrong code and hint;
- the hold-limit timeout cancelling a sync between E1's blocking steps;
- a lost master role surfacing as `config_sync_failed`;
- special files below a synced path not excluded from coverage;
- syncs before a failed offer missing from the record;
- a consistency assertion that held even without the suppression.

A second pass over the fixes found three smaller problems:
- `not_master` marking the healthy target unreachable;
- an expiry after an unreachable attempt still being blamed on the sync;
- a sync from an earlier attempt being recorded after a later attempt failed.

Each is fixed as described above. The warning test now gives the member an
override, so the synced location's fingerprints really differ; removing the
suppression makes it fail (checked). The review also noted the global sync lock
and placeable runs failing on a sync fault; both are under "Limits".

## Automated evidence

`mise run check` passes. `tests/pre_run_sync.rs` runs real member services in
temporary homes, with loopback standing in for the tailnet.

`a_stale_member_is_synced_before_the_run_and_a_current_one_costs_a_check`:
- A file changed on the master reaches the member only with the placed run. The
  run's `cat` prints the new value, and the member's `sync history` grows by one
  push.
- The record's `config_sync` is `pushed` at generation 2, with
  `{path, files: 1, bytes}`. `runs` has `config_sync_ms`.
- Of the two declared locations, only the unsynced, differing one warns. The
  synced one differs too, through the member's own override (laid by the same
  push), and does not warn.
- A second run with nothing changed is `current`, with no push.
- A `--pinned` run on the master records no sync and pushes nothing, even with
  the master's file changed.
- A sentinel value is in no `runs` output, `explain` on either member, service
  log, status file, records database (and its WAL) or `history.jsonl`.

`a_failed_sync_fails_the_run_with_its_code_and_line`:
- The member's `~/.config` is a link into `~/.ssh`.
- The run exits 125 without running the command.
- The stderr line is exactly the failure line, and the envelope is
  `config_sync_failed` with `details.member` and `details.cause`
  (`unsafe_sync_path`). The status file has the same code.
- The record's decision verdict and `config_sync` are `config_sync_failed` and
  `failed`, and nothing is written into `~/.ssh`.

`an_unreachable_member_is_still_member_unavailable`:
- With the member's service down and a path synced, a required run fails with
  `member_unavailable`, as before E2, in Elsecall's own words.

`a_sync_still_running_at_the_hold_limit_fails_the_run`:
- The member's service is stopped with SIGSTOP, so the generation check never
  answers. A run with a 1 s hold limit fails with `config_sync_failed`, cause
  `hold_limit_passed`.
- The record keeps the failed sync with its duration of about 1 s.

Unit tests cover:
- the coverage rules for locations: inside, equal, case, containing, sibling,
  link, FIFO, `.git`, absolute;
- the failure's cause filtering, and unreachable members and `not_master` kept as
  `member_unavailable`;
- the record's serialized form;
- the push summary leaving overrides out.

## Installed-system evidence

Only the master's build matters: the pre-run sync runs on the master and its CLI,
and members receive E1 pushes unchanged. Installing restarts the master's
service, so the release build (sha256 `dad32203…`) was installed on
`mac-mini` only when it held no Elsecall work: 2026-11-03 at 23:40 UTC,
after about four hours of continuous acme work. `install` kept the desktop
session, and the firewall stayed `allowed`. `macbook-pro` (the devbox,
background session) kept its E1 build. Both members were reachable at
generation 5 with nothing synced.

The steps used the throwaway `~/.config/elsecall-sync-demo`, declared temporarily
in the Mac's `config.toml`, and a scratch repository whose `.elsecall.toml`
declares `~/.config/elsecall-sync-demo/.env` for consistency checks.
`~/.config/acme-ios` was neither declared nor enabled. This session's command
guard refuses `sync enable` and `sync disable` (it reads them as shell builtins),
so the operator ran those two from a terminal.

- **Enable.** The plan showed one file `added` on `macbook-pro` (23 bytes, no
  backup). `sync enable … --confirm 5f3ae9ce5a9de449` issued generation 6, and the
  devbox was `in_sync`.
- **Stale member, synced before the run.** `.env` was changed on the Mac only;
  `sync status` still showed generation 6, as nothing syncs without an event. Then
  `elsecall run --member macbook-pro -- sh -c 'cat "$HOME/.config/elsecall-sync-demo/.env"'`
  from the scratch repository:
  - It printed the new value from `/Users/devbox`, in 0.82 s of wall time.
  - `explain` showed `config_sync`: `outcome: pushed`, generation 7,
    `pushed: [{path: ~/.config/elsecall-sync-demo, files: 1, bytes: 40}]`,
    `duration_ms: 143`.
  - `waited_ms` was 163 and the workspace sync (a cold copy) 573 ms. `runs`
    listed `config_sync_ms: 143`.
  - `warnings` was empty, although the declared location had just changed on the
    master: it is within the synced path.
  - The master's `sync history` showed `issued 7`, then `pushed 7` to
    `macbook-pro` with `.env` `changed`, 40 bytes.
- **Current member, one check.** Three more placed runs with nothing changed
  recorded `outcome: current` at generation 7, with `config_sync_ms` 16, 10 and
  12. `sync history` gained no push.
- **No values.** The new value appears nowhere in the master's Elsecall state
  directory (records, logs, history, state) outside the replica object store,
  which is the storage meant for secrets.
- **Cleanup.** `sync disable … --confirm` (operator) issued generation 8 with no
  paths on both members. A placed `rm -rf` removed the devbox's demo directory
  and a placed `ls` confirmed it gone. The Mac's directory was deleted and its
  `config.toml` restored to `[capacity]` only. `sync status` lists nothing synced
  or declared.

These timings support leaving the sync out of placement's estimates: a check costs
10–16 ms and a one-file push 143 ms over the tailnet. Both are far below the
`max(2 s, 10%)` scoring margin.

## Follow-up: fallback and per-member locking

Two operator decisions after E2 shipped: a placeable run is never worse off than
running locally because another member's sync failed, and a slow sync to one member
never delays another.

### Decisions

- **Fall back instead of failing.** When the pre-run sync to the member placement
  chose fails (`Failed::Sync`: any cause but an unreachable member or a lost
  master role, which keep their E2 meaning), the master adds that member to the
  request's exclusions (`Auto::excluded`, scoring leaves it out) and places the
  request again at once (`Scheduler::fall_back`): undecided, then settled as on
  arrival, to another member, to the caller's own queue, or undecided while no
  remaining member can start it in time, re-scored every second as before. The
  caller needs no sync, so it is always a candidate.
- **The hold limit is the request's own.** Falling back keeps the request's
  `expires_at` and timer; nothing restarts. A request with no member in time
  fails with `fleet_busy` at its original hold limit, naming the members left out
  (`Left out: its synced configuration could not be brought up to date (cause).`).
  A sync still running at the hold limit fails the run with `config_sync_failed`
  (`hold_limit_passed`), as in E2: no time is left to fall back.
- **When `config_sync_failed` remains.** For a required member, and for placeable
  work when no candidate left could ever take it (every remaining candidate's start
  is `never`, such as a size only the failed member's capacity holds, and no member
  is unanswered). Then the failure, record and line are E2's, and the decision's
  candidates are those that were left.
- **Exclusions are per request.** The member stays in the fleet and in other
  requests' placement: a sync fault is often one path on one member, and the next
  request tries again (and reports again) rather than hiding it.
- **Records.** The master's record gains `fallbacks`, oldest first, each with
  `failed_sync` (E2's `config_sync` record of the failure: `member`,
  `duration_ms`, `outcome: failed`, `cause`), `decided_at`, `moved_to` (the
  caller, another member, or `null` while it waited) and the scoring's `reason`.
  `config_sync` keeps the sync before the run's start, so a run that fell back to
  the caller has none. Every decision of a placeable request with fallbacks ends
  its `reason` with `<member> was left out: its synced configuration could not be
  brought up to date (<cause>).`, which `runs` lists too. Causes are filtered as in
  E2: never a member's message.
- **Per-member locking** (a change to E1, see [config-sync-slice.md](config-sync-slice.md),
  "Locking"). E1's single lock is split. `issuing` serialises the master's scans,
  issues, the synced set's changes and role changes, which is local work only.
  `MemberLocks` holds one lock per member for every exchange with it (pre-run
  syncs, pushes, plans). A pre-run sync takes its member's lock, then refreshes
  under `issuing`, so pushes to a member never go back to an earlier generation.
  A round (`sync push`, enable, disable, handover) issues once, then pushes to
  each member under that member's lock the latest issued generation. Each push
  pins its manifest's content (`pins::Pins`), so a generation issued meanwhile does
  not remove what the push still has to send. A push therefore never sends a
  half-updated set: a manifest is an immutable, whole generation, and its content
  stays until the push ends.

### Review

The code-review pass found no deadlock (every path takes a member's lock before
`issuing`, never the reverse) and no member message reaching records. It found:

- a handover whose 30 s hold could pass while its round still waited for a slow
  member's lock, so a pre-run sync could issue meanwhile and the move be refused:
  a handover now takes every member's lock before its hold starts;
- `enable` no longer atomic with its plan, so a concurrent `disable` of the same
  path could be undone: it now refuses with `plan_changed`;
- the README claiming a placeable run never fails on a sync, though one still
  running at the hold limit does;
- the decision of a request with no candidate left listing the excluded member;
- members not answering quotes counted as gone, failing a run that one of them
  might still take;
- the role not checked again after waiting for a member's lock;
- a round's report naming one generation when a member got a later one;
- a hold-limit test that could not tell a restarted hold limit (the fallback was
  immediate; now it happens 3 s in) and a timing race in the serialization test
  (it now waits for the push's generation to be issued).

Each is fixed as described. Two points are left as limits below: failures on the
master itself are blamed on the chosen member, and syncs abandoned at a hold limit
still run.

### Automated evidence

`mise run check` passes. New tests in `tests/pre_run_sync.rs`, real member services
in temporary homes on loopback, machines scripted for speed and cores as in
`tests/placement.rs`:

- `a_failed_sync_falls_back_to_another_member_and_then_to_the_caller`: three
  members, bravo fastest and warm, charlie next, alpha the caller. With bravo's
  `~/.config` linked into `~/.ssh`, a placeable run runs on charlie and prints the
  master's new value (charlie was pushed first). `explain` shows `fallbacks[0]`:
  `failed_sync` bravo, `failed`, `unsafe_sync_path`, `duration_ms`; `moved_to`
  charlie with charlie's scoring reason; `config_sync` charlie `pushed`; candidates
  alpha and charlie only; the decision's reason ending with bravo left out, also in
  `runs`. Nothing is written into bravo's `~/.ssh`. With charlie broken too, the
  next run tries bravo again, then charlie, then runs on alpha:
  `fallbacks` bravo → charlie, charlie → alpha, no `config_sync`, no `placed`.
- `a_fallback_stays_within_the_hold_limit`: both members fully leased, bravo warm
  and fast but broken. With an 8 s hold limit the request waits undecided; bravo is
  freed 3 s in, chosen, fails its sync, and the request falls back undecided
  (`moved_to: null`, the caller still full). It fails with `fleet_busy` at 8–9.5 s,
  not 3 + 8, the message naming bravo as left out.
- `a_placeable_run_no_other_member_can_take_still_fails_with_config_sync_failed`:
  size 4, a caller of 2 cores, bravo of 8 and broken: `config_sync_failed` with
  `member` and `cause`, the record's `config_sync` failed, no `fallbacks`.
- E2's `a_failed_sync_fails_the_run_with_its_code_and_line` still covers
  `--member`: `config_sync_failed`, unchanged line and record.
- `a_slow_push_to_one_member_does_not_delay_a_run_on_another`: bravo's service
  paused with SIGSTOP (the test boundary), `sync push` issues generation 2, reaches
  charlie and hangs on bravo. A file changed meanwhile, a `--member charlie` run
  completes in under 5 s while the push still runs, printing the newer value, its
  record `pushed` at generation 3. Once bravo resumes, the push completes:
  `applied` with generation 2, whose content its pin kept.
- `pushes_to_the_same_member_stay_serialized`: bravo paused, `sync push` hangs on
  it, a `--member bravo` run started meanwhile waits. After bravo resumes, the run
  prints the new value with `config_sync` `current` at generation 2 (waited at
  least 1 s), and bravo's history shows one push, not two.

Each test fails without the behaviour it covers (checked): one lock shared by all
members stops the first locking test (the push never reaches charlie), ignoring pins
fails bravo's push (`Read synced content: No such file or directory`), and no member
lock makes the run push a second time (`pushed`, not `current`). Unit tests cover the
member locks and pins.

### Installed-system evidence

Deferred to the batch release verification: both machines carry production
Elsecall work, so this item installed nothing. Only the master's build matters
(fallback and locking run on the master; the member protocol is unchanged). The
checks to run then, between `mac-mini` (master) and `macbook-pro`, with
the throwaway `~/.config/elsecall-sync-demo` (never `~/.config/acme-ios`):

1. Declare the demo path in the Mac's `[sync] paths`, create
   `~/.config/elsecall-sync-demo/.env` there, and enable it (`sync plan`, then
   `sync enable … --confirm <plan_id>`); the devbox is `in_sync`.
2. From a scratch repository, `elsecall run --member macbook-pro -- true` to give
   the devbox a warm copy.
3. On the devbox, `chflags uchg ~/.config/elsecall-sync-demo`: its next push is
   refused at the swap (`EPERM`, checked on this Mac), before anything changes.
4. Change `.env` on the Mac.
5. Hold the Mac's capacity for about 25 s (`elsecall lease acquire --size <capacity>`
   held by a `sleep 25`), so placement chooses the warm devbox first.
6. `elsecall run --placeable -- sh -c 'echo $ELSECALL_MEMBER; cat "$HOME/.config/elsecall-sync-demo/.env"'`:
   expect `mac-mini` and the new value, exit 0; `explain` shows
   `fallbacks[0]` with `failed_sync` `macbook-pro`, `failed` and its cause,
   `moved_to` `mac-mini`, and the decision's reason ending with the devbox
   left out.
7. `elsecall run --member macbook-pro -- true`: expect `config_sync_failed` with
   `details.member` and `details.cause`.
8. Per-member locking needs a third member to observe and is covered by the
   automated tests.
9. Clean up: `chflags nouchg` on the devbox, `sync disable … --confirm`, delete the
   demo directory on both machines and restore the Mac's `config.toml`.

## Limits

- A placed request that waits for a busy member is synced again at every offer,
  about every 500 ms. Each check is one scan of the master's synced paths (which
  rereads only files whose size, times or inode changed) and one status request,
  and a push happens only when something changed.
- A pre-run sync to a member waits for a slow exchange already in progress with
  that same member (up to E1's 120 s). A run that cannot start within its hold
  limit then fails with `config_sync_failed` (`hold_limit_passed`), placeable or
  not, and a placeable request does not move to another member meanwhile.
- A failure on the master itself (an unreadable configuration, a file changing
  while it is read) surfaces as the chosen member's sync failure. The request falls
  back regardless, ending on the caller after each member was tried, but the record
  names those members.
- A sync abandoned at its hold limit still runs to its end, after any exchange
  with that member already in progress.
- A placeable request that fell back retries the failed member at its next request,
  so a lasting fault costs each placeable request one failed sync (a status
  request, or a push refused before anything changes) before it moves on.
- During the 30 s hold after a master-move handover, the master sends the frozen
  state; a synced location the master changed in that window does not warn.
- Sync and offer are two requests. A change on the master between them reaches the
  member at the next placed run or `sync push`.
- A member's local edits or overrides made while a run is in progress are not
  undone until the next placed run.
- A pushed path's sizes are summed from what the member reported applying.
  Overrides are left out; `sync history` on either member has the details.
