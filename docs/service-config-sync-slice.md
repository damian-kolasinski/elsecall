# Fleet service configuration

Elsecall's service settings are an optional typed attachment to an E1 mirrored
configuration generation, rather than an exception to E1's forbidden-path rules.
The master edits `fleet-config.toml` in its Elsecall state directory. No secrets
belong in this file.

## Settings and ownership

The document has `[defaults.<section>]` and `[members.<roster-name>.<section>]`.
Member sections overlay defaults by key; omitted keys use service defaults, not
the member's old local settings. Arrays replace arrays. Unknown sections, keys,
member names and invalid values are rejected. The master checks member names
against its current roster; remove departed member sections before issuing again.

| Settings | Fleet defaults | Per member | Meaning |
| --- | --- | --- | --- |
| `placement`, `estimates` | Yes | Yes | The master's resolved values govern placement; each member's estimates also learn sizes locally. |
| `admission` | Yes | Yes | Each member gates its own admission. |
| `logs`, `retention`, `housekeeping`, `placed_runs.leftovers_cap`, `sync.paths` | Yes | Yes | Each member's own policy; the current master's sync declarations govern mirrored paths. |
| `capacity`, `workspace`, `placed_runs.path` | No | Required | Reserves, workspace location/root/object sources and executable search paths describe that machine. |
| Identity, roster, trust keys, listener discovery/port, service installation/session | Never | Never | Local bootstrap state outside this document, established before trusting the master. |

For example:

```toml
[defaults.placement]
keep_local_margin = 0.2
[defaults.admission]
saturated_utilization = 0.9
[defaults.sync]
paths = ["~/.config/acme-ios"]
[members.mac-mini.capacity]
reserved_cores = 3
[members.macbook-pro.workspace]
location = "mapped"
root = "/Users/devbox/.elsecall/workspaces"
object_sources = ["/Users/devbox/Developer/acme-ios"]
[members.macbook-pro.placed_runs]
path = "/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin"
```

Values have exactly the validation and defaults documented under README "Service
configuration". A document is at most 64 KiB with at most 64 member sections.
Use member sections for machine differences; ordinary E1 sync overrides never
modify this attachment. The local `config.toml` is not a managed overlay.

## Migration and operation

1. Keep the existing local `config.toml` files while preparing the master's
   `fleet-config.toml`. Copy shared settings into `defaults`, reserves and paths
   into the relevant roster names. Explicitly carry over any non-default local
   policy that should remain. Include the current master's `[sync] paths` in the
   document so existing enabled paths continue to be active after migration.
2. Run `elsecall sync service` on the master. It validates the whole document and
   asks each member to validate its future settings over the sealed sync channel.
   The dry run shows changed section names and keyed fingerprints, never values.
   It changes no service settings or mirrored paths. Private replica objects may
   be staged, as with other sync preparation.
3. Review it, then run `elsecall sync service --confirm <plan_id>`. All members
   must be reachable and validate migration. The ID binds the source, roster,
   base generation and reported member baselines. A changed plan is refused.
4. Watch `elsecall fleet` or `elsecall doctor`. Under each member's
   `status.config_sync.service` (doctor: `configuration_sync.members[].sync.service`)
   are `generation`, `active_generation`, `fingerprint`, `pending`, `current` and
   `rejection`. Unreachable members have no observed state. A generation can be
   current even when `active_generation` is older: an intervening generation may
   change ordinary mirrored files while keeping the service attachment identical.
5. Subsequently edit the master's source and run `elsecall sync push`. The normal
   E1 events, including pre-run sync and master handover, refresh this attachment
   too. New/offline members receive it at their next sync. There is no polling of
   the source file. `sync push --member NAME` also delivers changes, but reports
   pending application while that member needs its automatic restart.

Existing `config.toml` files continue to work until opt-in. They are preserved
byte for byte after migration and are unused while service settings are managed;
editing one cannot override the master or prevent a managed restart. There is no
implicit switch back to local settings: an offer omitting an already-managed
attachment is rejected. In particular, after a forced move to a member lacking
it, restore/enable the service configuration on that master before pushing again.
To undo tuning, edit the master's document back to the desired settings and push.

## Validation, replicas and application

`service_configuration` owns the document schema, resolution and comparisons;
`configuration` validates resolved service settings. The sync manifest names the
attachment by owner mode, size and keyed fingerprint, never by value. It shares
E1's replica store, content pinning, sealed channel, master authority at attach
and commit, generations, issuing lock and per-member exchange locks. Ordinary
synced paths still cannot touch Elsecall's state.

The master validates before issuing. A receiver verifies the blob and validates
all overlays, its resolved settings and its future workspace root against mirrored
paths before changing any part of the generation. Rejection preserves the previous
manifest and running configuration, records a reason without values and remains
visible until a valid push is accepted. Invalid edits on the master similarly
leave the last issued generation and running settings intact.

Every member holds the complete document without local overrides, so it can
become master. Applying on a member and taking over restore the accepted source;
unpublished local drift is kept owner-only in `service-config-backups/`, separately
from ordinary mirrored-file backups. The latest 20 source versions are retained,
enforced both at startup and when backing up. `retention.sync_backups` continues to
bound ordinary E1 backups in `config-sync/backups/`.
Edits made after takeover are the new master's source. E1's handover checks include
the attachment; a forced move has E1's documented possibility of losing unissued
or missing generations.

All managed service settings use an **automatic restart at idle**, not live
mutation. Existing running and queued requests finish with their existing
parameters. Supervised processes left by placed commands also finish, or reach
their already-approved cap, before the restart. A sustained queue or long lease
can therefore keep a member pending. Sync exchanges and role handovers finish
before restart; admission closes atomically with the idle test so a new run cannot
race the restart. Upgrades holding admission also defer it. The service execs its
current binary, keeping its supervision and environment; no install or manual
service restart is needed. Decision records remain immutable snapshots of the
parameters actually used. A placed request whose configuration is delivered but
not active follows E2's existing failure/fallback policy instead of running under
old settings. During the brief restart, normal service-unavailable behavior applies.

The handover checks durable replica receipt, not idleness: busy members may still
be applying service settings after the master moves. Their full replica is already
available for future role changes.

## Validation

Automated evidence uses pure schema tests and real services in temporary homes
with loopback replacing tailnet discovery. It covers dry run/confirmation drift,
per-member reserves and paths, master and receiver rejection, previous-generation
retention, parameter snapshots, automatic restart after work, solo startup,
leftover supervision and master moves. The existing E1/E2 tests cover authority,
interrupted transfers and per-member locking for the shared mechanism.

Installed-system verification: left to the release verification session

That session should migrate both real members' current settings through the dry
run, confirm unchanged machine-specific roots/object sources and the mini's reserve
of 3, push a scheduling change, observe generations/current/rejections in fleet and
doctor, prove long-running work and leftovers survive until completion/cap, check
new decision parameters and report/replay, test invalid master edits and recovery,
and move the master both ways while checking that replicas remain authoritative.
No installed services were changed for this item.
