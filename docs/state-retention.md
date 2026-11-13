# Local state retention

Elsecall's state is `~/Library/Application Support/Elsecall/`. Retention is local
to each member; configuration changes take effect after service restart. Build
stdout/stderr pass to callers and never enter the service log. Logs contain
lifecycle events and errors, never environment dumps, request payloads or secret
values. Synced content and backups intentionally store secrets owner-only.

| State | Retention or bound |
| --- | --- |
| `service.log`, `service.log.1` … | Current file plus `[logs] keep` archives (default five); rotate before a complete record would exceed `[logs] max_size` (default 10 MiB). Newest archive is `.1`. A single oversized record is retained intact; it can exceed the threshold. Reducing the archive count removes excess archives on the next rotation. Files and the shared `service.log.lock` are 0600. |
| `records.sqlite`, `load.sqlite`, `outside-pressure.sqlite` and their WAL/SHM sidecars | Ended requests, minute load summaries and ended outside pressure episodes retain 30 days. Episodes hold process names and PIDs only, never arguments, environment or paths; an episode a stopped service left open ends at its last attribution on the next start. Requests expire by request time; active work is never pruned. Prune at startup and daily while running. SQLite reuses freed pages and vacuums after expiry when at least 16 MiB and 25% of the database are free, then attempts a truncating checkpoint. Readers can delay WAL truncation until a later pass. Volume within the retention window depends on traffic; there is no hard byte cap that could discard active work or recent reports. |
| `outside-processes.sqlite` and its WAL/SHM sidecars | Trend rounds (outside processes using at least a quarter core or 256 MiB, every 5 minutes) retain 12 hours; ended early warnings retain 30 days. Processes that were ever Elsecall's own work are remembered by PID and start time until they exit. Process names, PIDs and start times only, never arguments, environment or paths. Prune on the first judged round after a start (the second sample) and hourly while running, with the same reclaim as above. At most a few dozen rows per round, so the rounds stay in the low megabytes. |
| `config-sync/history.jsonl` | Latest 500 events after compaction; compacts when exceeding 1,000 events. Names, sizes, short fingerprints and error codes only. Manifest limits bound event size. |
| `config-sync/backups/` | Latest 20 versions per filename, also capped globally by `[retention] sync_backups` (default 1,000). Oldest timestamps expire first, including files/paths no longer synced. The global cap applies on startup and each new backup. Empty timestamp/path directories disappear with expired backups. Failed deletions remain registered for retry. Startup removes unregistered timestamp directories only after seven days, allowing recovery of a tree moved before its record was committed; registered whole-tree backups follow version retention. Cross-volume whole-tree backups kept beside the live path follow the same version caps. This is a version-count limit, not a byte limit: normal files are at most 16 MiB each, but a whole tree saved because sync cannot carry some entries may be larger. Doctor flags large storage; lower retention after recovering anything needed. |
| `config-sync/state.json`, `config-sync/objects/` | Current/applying manifests and current written trees; removed paths leave written state. At most 16 synced paths, 4,000 entries, 16 MiB per file and 64 MiB total content in a manifest. Obsolete objects are removed after applying/issuing, except objects pinned by active transfers. Backups are accounted separately above. |
| `copies.sqlite`, `copies/`, `workspaces/` | Existing housekeeping owns these: current copy/store metadata, copy state, caller stat/push caches; removal history retains 90 days and is pruned on the next removal. Caller caches disappear when their workspace is deleted, a member leaves, or its copy is removed. Unavailable workspace volumes are preserved. SQLite reuses freed catalog pages. No new copy/object-store policy is introduced here. |
| `upgrade/` | One latest journal, one unreadable journal aside, at most one incoming and one previous executable. Builds are discarded after completion/recovery, replaced on the next upgrade if interrupted. No per-upgrade archive. |
| Executable, `agent.plist`, `config.toml`, receipt, `member.key`, `fleet.json`, socket, service/installation/helper/log locks and test process PID | Current installation and membership only; overwritten/reused, never appended. Locks are empty persistent coordination files. |
| Atomic-write scratch | Removed after successful writes and ordinary errors. Startup removes recognized metadata scratch files older than seven days in the state root, `upgrade/` and `config-sync/`, without traversing stored content. Unregistered backup timestamp directories expire after seven days on sync startup. Workspace/copy scratch remains under its existing housekeeping lifecycle. |
| Unrecognized operator files | Preserved, including by uninstall. No automatic deletion policy; doctor measures and flags large entries so their owner can decide. |

The service and upgrade helper own log writes. They serialize each full record
with an OS lock shared across processes, rotate, reopen the current pathname and
append while holding that lock. launchd and the helper launcher redirect raw
stdout/stderr to `/dev/null`; this prevents continued writes to an archived inode
and prevents arbitrary process output from becoming a log record. Foreground
`serve` writes the same rotating file; CLI failure envelopes still go to stdout.
Invalid configuration leaves a code-only failure record using default log limits.
I/O or permission failures can prevent retention; fix the underlying access or
disk problem before expecting cleanup to succeed.

`doctor.state` reports logical `bytes`, `allocated_bytes`, each top-level entry,
its `warning_above_bytes`, `unusually_large`, and warnings. It reads metadata only,
never follows symlinks, works with the service stopped, and marks incomplete
measurements. Thresholds are the configured log-file size, 512 MiB for each records
DB/sidecar, 128 MiB for each load DB/sidecar, 1 GiB for config-sync, 256 MiB for other
entries, and 2 GiB for total state. These are advisory investigation thresholds,
not deletion caps, and do not affect API-health verdicts. Copy trees outside the
state directory are measured by `elsecall copies` under existing housekeeping.

Installed-system verification is deferred to the release verification session:

- Upgrade/restart the installed LaunchAgent and confirm raw stdout/stderr paths
  are `/dev/null`; confirm new lifecycle events appear in current `service.log`.
- Use a small log threshold temporarily; exercise service failures and an upgrade
  across rotation. Confirm complete JSON lines, 0600 modes, archive count and new
  events in the current file while the service continues answering requests.
- Check `doctor` sizes/warnings, including stopped-service diagnosis; restore
  normal configuration and restart.
- Inspect normal retention after startup/daily expiry, database reclamation, and
  old sync backups after lowering the cap. Confirm retained secrets stay private
  and never appear in logs or diagnostic output.

Fleet service-source recovery has its own fixed budget: `service-config-backups/`
keeps at most 20 versions, trimmed on startup and when a member's unpublished
source is replaced. These are separate from ordinary mirrored-file backups and
from `retention.sync_backups`; see [service-config-sync-slice.md](service-config-sync-slice.md).
