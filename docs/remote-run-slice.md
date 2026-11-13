# S3: Remote run on a required member

Step S3 of [Fleet scheduling](fleet-scheduling.md). It delivers:

- a workspace copy with mapped and same-path locations;
- output return;
- environment forwarding;
- consistency warnings;
- lost-connection handling.

It came in three reviewed items:

1. **elsecall#13** (merged): consumer configuration and the copy engine.
2. **elsecall#14** (merged): remote execution, its channel and I/O.
3. **This item:** outputs, consistency warnings and the documentation.

## Decisions subsequent slices can build on

### Copies
- **Copies are git repositories.** A copy is a real working tree. Its history lives in one object store per caller repository (`<root>/.stores/<calling member>/<hash of the common git dir>.git`), which the copy reaches through alternates. So every worktree of a repository shares one history: a second acme worktree's first sync sent no history at all.
- **What syncs.** The caller sends HEAD and its upstream as validated names, and the remote-tracking refs and tags the copy carries ([workspace refs](workspace-refs-slice.md)), then its ignore rules (`info/exclude` and the global excludes file), the index as `ls-files -s` entries, and a manifest of every tracked or untracked non-ignored file and link with its permissions and SHA-256. History moves as a thin pack against tips the member holds; a member may first fetch it from repositories it already has ([fast first sync](first-sync-slice.md)). File content moves only when the member has it nowhere else: renamed files are cloned from the copy (`fclonefileat`), and committed files are read from the object store when their digest matches.
- **Warm syncs exchange digests only.** Both sides remember what was last synced: the caller in `workspaces/` in its state directory, the member in `copies/`. A sync is a difference between the two when the member still holds exactly what the caller last pushed. The member detects a run's changes by inode, size, modification time, change time and mode, and puts them back.
- **Ignored files are never listed,** so they are never copied and never touched in the copy. Untracked non-ignored files a run leaves are removed, judged by the caller's ignore rules, which the copy's configuration points at.
- **Locations.**
  - Mapped copies live at `<root>/<calling member>/<name>-<hash of the caller's path>`.
  - Same-path copies live at the caller's path. The member refuses one that would hold anything but this calling member's copy of this workspace.
  - Each copy is marked before git initializes it, so an interrupted first setup is recognized: mapped ones are cleared, same-path ones are completed.
- **One run per copy** is an in-memory claim on the member, held from admission until the run ends.

### Admission and placement
- **Placement waits on the master.** A placed request has its own phase in the master's scheduler, `Placing`, queued per target member in arrival order. A task offers the queue every 500 ms. A refusal for room blocks those behind it; a refusal because the copy is busy lets the next request try.
- **Members admit or refuse at once.** A member admits a placed run only when its own queue is empty, the load gate is open and the size fits now. It then holds the capacity in its own ledger. An admission nobody attaches to within 15 s lapses.
- **Authority and the run channel.**
  - Offers and attaches go through `PlacementAuthority`.
  - The attach is an HTTP upgrade on the member listener. The master's ephemeral X25519 key travels in the signed request body. The member signs both ephemerals, both identity keys, the request nonce and the run ID (`peer_auth::ChannelAcceptance`).
  - The stream is sealed with ChaCha20-Poly1305, with a key per direction and counter nonces (`secure_channel`).
  - The CLI has no key, so the master's service relays the CLI's upgraded local connection over the channel (`run_relay`) without reading it.
- **The run protocol** runs end to end between the CLI and the member's runner (`remote_run`):
  - first the sync, then `Start`;
  - streams with 1 MiB windows per stream, and one pipe when stdout and stderr share a destination;
  - signals to the command's process group;
  - then `Exited`, the outputs, and `Done`.
  - Heartbeats every second; ten seconds of silence, or the stream closing, is `connection_lost`.
- **The member owns the command's lifetime.** The runner reports start, warnings and end to the member's scheduler. The command's process is bound as the run's holder, so a lease the command takes nests inside the run. When the run ends, normally or not, the member stops the command's whole process group, so nothing outlives the run in the copy. After a service restart, recovery kills placed commands still running and closes their records as `connection_lost`.
  - Later (leftovers, so placed runs behave like local ones): once the caller has the exit, processes left in the group keep running, capped (`[placed_runs] leftovers_cap`, 10 minutes), watched by `remote_run::leftovers` and recorded as `placed.leftovers`; the copy's claim is released with them, and the next claim waits up to 10 s, then stops them before its sync. A lost connection before the exit still stops the whole group at once, and a restart stops leftovers through the processes their record names. See "Leftovers" in [fleet-scheduling.md](fleet-scheduling.md).

### Outputs, environment and records
- **Outputs.**
  - The member surveys declared outputs before and after the command, without hashing. A changed output is described with digests, as a blob, so large outputs never overflow a frame.
  - The caller validates the description: declared outputs only, each path once and inside its output, no file inside a file, bounded.
  - The caller rebuilds each changed output beside its own, reusing local content whose digest it re-checks. Only when every wanted byte has arrived does it swap the rebuild in atomically (`renameatx_np` with `RENAME_SWAP`).
  - An interrupt, a lost connection or a bad description leaves the caller's outputs as they were.
  - Later (after `r-b3832e93b295` lost its outputs to a leftover process): the member snapshots changed outputs into a run-private staging area the moment the command exits (`fclonefileat`, or a copy), and describes and sends them from there. Return is best effort per blob: the member replaces a blob it cannot send with `OutputNotSent`, the caller leaves that path out, and the caller reports what did not come back (`Returned`) for the member's record. See "Outputs" in [fleet-scheduling.md](fleet-scheduling.md).
- **Consistency.**
  - The caller sends what to fingerprint and a salt chosen for the run. The member returns its own fingerprints under that salt; the caller compares, writes the warnings and tells the member which names differed.
  - The member records warnings only for names the caller declared, in its own words.
  - The caller's fingerprints never leave it, and member-supplied text never reaches a warning.
  - Probes run in their own process group with a 10 s deadline. Location hashing follows links but reads only regular files, without blocking, up to 64 MiB.
- **Environment.**
  - The caller forwards variables matching the allowlist.
  - Both sides drop identity variables and `ELSECALL_*`.
  - The member starts from its service's environment, with `[placed_runs] path` overriding `PATH`, and adds the run's `ELSECALL_*` variables.
- **Records.**
  - Both members record the run under one run ID.
  - `placed` gives the side, both members, the caller's workspace, the copy and its location, how far the run got, the sync summary (time, bytes, files, content taken from the copy or from history, and the copy's size) and the exit when known.
  - Warnings are in `warnings`. `elsecall fleet` shows each other member's latest placed run with its warnings.
- **Member names.** A member in a fleet uses its roster name in records and in `ELSECALL_MEMBER`. Outside one, it keeps S1's host name.

## Automated evidence

`mise run check` passes. Beyond unit tests of every codec, validator and the channel, the suites below cover the behaviour.

### `tests/workspace_copy.rs`
Real repositories and the real sync protocol over an in-memory stream.

- **Fidelity.** A dirty tree: staged, unstaged and partially staged edits; a deletion, a staged deletion and a rename; untracked files; binary content; an executable bit; links inside the tree and pointing outside it; two local-only commits; a subdirectory as working directory. `git status --porcelain=v2 --branch`, `diff --cached`, `diff`, `log`, `ls-files -s` and every file's bytes and mode match.
- **Ignored files and URLs.** Files ignored by `.gitignore` and `info/exclude` are not copied, and the remote's URL never reaches the copy.
- **Warm and incremental behaviour.**
  - A second sync sends a 14-byte delta and hashes nothing.
  - A run's edit, stray file, staged change and commit are undone, while its ignored build state survives.
  - A rename moves no content, and a new commit sends a pack under 4 KiB.
- **Recovery and states.**
  - A lost caller cache falls back to full listings but moves no content.
  - An unborn and a detached HEAD.
  - Submodule entries appear as not checked out, so `git status` matches.
- **Review regressions.**
  - An edit that restores size and mtime is still undone.
  - A second calling member on a same-path copy is refused.
  - An unmarked or unfinished setup is cleared or completed.
  - Strangers' directories are refused, even when empty.

### `tests/remote_run.rs`
Real services in temporary homes on loopback; only tailnet discovery and machine load are replaced.

- **Streams.** A 300 KB binary round trip with separate binary stderr. 600 interleaved lines with `2>&1` to one file, in order.
- **Exits and failures.** Exit 7. The command's own 125 told apart by the status file. `member_unavailable` and `invalid_request` envelopes that never echo arguments.
- **Signals.** SIGINT reaches a trap (exit 42). SIGTERM kills the command, and Elsecall dies of SIGTERM.
- **Environment.** The allowlist, the member's HOME and TMPDIR, and all five `ELSECALL_*` values, for a mapped member and a same-path member with a configured PATH.
- **Authority and placement.**
  - A non-master runs locally, with the reason recorded.
  - Outside a working tree, the request runs locally.
  - A member with a free core refuses while its own request waits, giving `fleet_busy` after a 2 s hold limit with the member's state in the decision.
  - `request_too_large`.
  - Concurrent runs of one copy never overlap.
- **Lost connection.**
  - A killed member service: the caller fails within 5 s and the restarted service kills the orphan; both records say `connection_lost`.
  - A killed caller: the member stops the command.
  - A member stopped with SIGSTOP: the caller fails after 10 to 20 s, and after SIGCONT the member stops the command.
  - A background process the command left ends with the run. (Later: leftovers run on, capped; see "Leftovers" in fleet-scheduling.md.)
- **Outputs.**
  - Returned after exit 3, with an executable bit.
  - Links skipped with warnings.
  - An unchanged output leaves the caller's own edit alone, a removed one is removed, and a changed directory is replaced whole.
  - A link in the caller's tree is never written through.
  - 12,000 output files come back.
  - Nothing comes back after a lost connection.
- **Warnings.** A differing toolchain and then a differing configuration file produce one warning each. They appear in the status file, both records and `elsecall fleet`, but never in output and never with the values.
- **Records and nesting.** Both records, and a lease inside the placed command nesting in it on the member.

### Performance
`repeated_sync_of_a_large_unchanged_tree_is_fast`: 20,000 files, release build, on this Mac at load average 21.

| Sync | Time | Bytes sent |
| --- | --- | --- |
| Cold (the baseline) | 9.2 s | 4.1 MB |
| Warm, unchanged | 209, 217, 248 ms | 14 each |
| One file changed | 251 ms | 95 |

The test asserts warm < cold / 3 in every build, and warm < 1 s in release builds.

## Installed-system evidence

On 2026-11-02 the release build of this item was installed on both members:

- **`mac-mini`:** master; desktop session; `reserved_cores = 3`.
- **`macbook-pro`:** account `devbox`, background session, mapped locations.

Both reported `member_listener_firewall: allowed`. Every command below ran on the Mac with `--member macbook-pro`, in a scratch repository with a staged edit, an unstaged edit, an untracked file, a local-only commit and an ignored `build/` file.

- **Fidelity.**
  - `git status --short --branch` on the devbox was identical to the Mac's.
  - The command ran in `/Users/devbox/.elsecall/workspaces/mac-mini/scratch-de124f4205ebef55`.
  - The ignored file was not copied.
  - The status file named `macbook-pro`.
- **Binary round trip.** A random 4 MiB file through `cat` came back byte-identical (SHA-256 `209ff93e…76cd`) in 0.99 s of wall time.
- **Exit codes.**
  - `exit 7` exited 7.
  - The command's own `exit 125` exited 125, with the status file `state: exited, exit_code: 125`.
  - `--member nobody` exited 125, with a `member_unavailable` envelope.
- **Output.** A command that wrote `build/report.txt` and exited 3: Elsecall exited 3, and the file read `made on MacBookPro`.
- **Ctrl-C.** Under a real pseudo-terminal, `^C` reached the command on the devbox, which printed `trapped on MacBookPro` and exited 42.
- **Lost connection, member side.** Mid-run, `kill -9` of the devbox's service:
  - the caller exited 125 with `connection_lost` within the same second;
  - launchd restarted the service;
  - recovery stopped the orphaned `sleep 120` before the next check, 5 s later;
  - both records say `failed: connection_lost`, reached `started`, and the member's says the service restarted during the run.
- **Lost connection, caller side.** Mid-run, `kill -9` of the caller: the devbox had stopped the command within 3 s.

### An acme-ios worktree on the devbox
The command was `git status --short`, read-only. Nothing was built or tested there.

Worktree `elsecall-leases`: 12,857 tracked files, 12 GB on disk, nearly all of it ignored build state.

| Sync | Time | Sent | Notes |
| --- | --- | --- | --- |
| First | 43.2 s | 331 MB | a 327 MB pack; 10,671 of 12,854 files taken from history, the rest received |
| Warm, unchanged (5 runs, this Mac at load 36) | 475–577 ms | 14 bytes each | |
| With a few files changed | 1.16 s | 334 KB | six files changed in the worktree meanwhile |

- `git status --short` on the devbox matched the Mac's exactly, submodules included.
- Copy size: 99 MB on disk for the copy, holding 63.8 MB of content, plus a 325 MB shared object store.
- **Second worktree of the same repository** (`feature-worktree`): its first sync took 5.8 s, sent 3.5 MB of listings and no pack, and took 10,690 files from the shared history.

**Warm sync time.** It is dominated by git's own work. On the caller, `git ls-files --others` of that worktree took 159 to 506 ms, depending on load; `git status --short` there takes 216 ms. On the loaded devbox, verifying the copy and its untracked files costs about 200 ms more. The two run concurrently.

## Limits

- **Submodules** are not copied (see "Contract questions").
- **Content filters** (git-lfs, eol conversion) are not applied: worktree bytes are copied verbatim.
- **Not carried:** other branches, tags and stashes; skip-worktree and intent-to-add flags. [Workspace refs](workspace-refs-slice.md) later carried remotes' default branches and declared refs.
- **Paths** that are not UTF-8 fail the sync.
- **Housekeeping.** This slice does not clean up copies or object stores; [housekeeping-slice.md](housekeeping-slice.md) adds that. Their sizes are in each record (`placed.sync.copy_content_bytes`, `copy_files`) to plan it. A member's disk is not checked before a sync.
- **No terminal on the member.** The command sees pipes, not a pseudo-terminal, even when the caller has one.
- **Ctrl-Z.** SIGTSTP is not forwarded. A caller stopped for more than ten seconds loses the run.
- **Restarts.** A service restart on the master ends its placed runs with `connection_lost`, and its callers must run them again.
- **Unreachable members fail at once.** A required member that cannot be reached fails at the first offer; it is not retried within the hold limit.
- **Lapsed admissions.** An admission that lapses because its caller vanished holds the member's capacity for up to 15 s.
- **Untrusted messages.** Outputs problems the member reports are summarised by the caller, never quoted. (Later: for outputs that did not come back, causes are typed, paths are checked against the declared outputs, and the member's error text is escaped and bounded to 200 characters; the caller reports what did not come back, so both records name the same paths.)
- **Test seams.** `ELSECALL_MACHINE_LOAD=idle` and `ELSECALL_SAME_PATH_BASE` exist for services that tests start, like S2's tailnet overrides.

## Contract questions

The most literal reading was implemented in each case.

1. **Submodules.** "The git working tree" was read as excluding submodules' own working trees. A build that needs submodule content fails on the member. Should a later step copy submodules as nested workspaces?
2. **History.** All history reachable from HEAD and its upstream is carried, so `git log`, `describe` and `rev-list --count` match. The cost is a first sync of the whole history, once per repository.
3. **Required member outside a working tree.** It runs locally, because the contract says placeable work outside a tree does, although a required member is otherwise a hard requirement.
4. **Nested requests.** A `--member` request from inside a local lease or run nests and runs on the calling member ("runs on the same member").
5. **"Only if they changed"** was read as changed by the run on the member, compared before and after the command. A changed output replaces the caller's whole; one the run removed is removed from the caller.
6. **Process groups.** Forwarded signals go to the command's process group, as a terminal's Ctrl-C does. Leftover processes in the group are stopped when the run ends, so that one run at a time uses a copy. (Later: they run on up to a cap, and the next run of the copy waits for them or stops them.)
7. **One copy per path on a same-path member.** A second calling member's workspace at the same absolute path is refused rather than sharing the copy.
8. **Run-made directories.** When a run turns a tracked file into a directory, the next sync removes that directory, including ignored files the run put inside it.
9. **Grace period** is fixed at ten seconds on both sides, and a stopped command gets five seconds between SIGTERM and SIGKILL.
10. **A connection lost after the exit.** "The caller fails with `connection_lost`" was applied up to the command's exit. Once the exit has arrived, Elsecall exits with it, so a caller never re-runs a command that ran, and warns that outputs were not returned. Both records keep the exit.
