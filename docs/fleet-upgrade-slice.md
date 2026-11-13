# Upgrading the fleet

`elsecall fleet upgrade`: the master distributes its Elsecall build to every member,
which installs it and restarts, so the fleet runs one build without copying binaries
by hand. README "Upgrading the fleet" is the operator's guide.

## Decisions

- **Build identity is the executable's digest.** A build ID is the first 16 hex
  digits of the SHA-256 digest of the executable (`build_identity`). The service
  computes it from its own executable at start, before anything could replace it,
  and reports it in `status`, its member status, `fleet` and `doctor`. Two members
  with one build ID run byte-identical builds, which is exactly what an upgrade
  installs, so no build script or embedded stamp is needed and any change of code
  is a visible change of build. Reading the executable costs about 30 ms for a
  release build.
- **Only the recorded master distributes, as a type.** The upgrade channel's route
  requires `UpgradeAuthority`, which only `Fleet::authorize_upgrade` creates from the
  recorded master's key, like placement and sync authority. The member checks it
  again after waiting for its work and before handing over, in case the role moved.
  The local command refuses on a non-master with `not_master`, and the master is not
  upgraded over the fleet (`install` updates it).
- **A signed manifest, verified content.** The master signs, with its member key and
  a domain-separated transcript, the upgrade ID, the recipient's key, the digest,
  the size, the OS, the architectures, the wait bound and the issue time. The member
  verifies it against its recorded master's key, refuses a manifest for another
  member (`manifest_unverified`) or issued more than 10 minutes away from its clock
  (`manifest_expired`), then receives exactly the signed size and compares the
  digest (`digest_mismatch`). The content arrives on a sealed channel
  (`elsecall-upgrade/1`, keys from an ephemeral exchange both enrolled keys sign, as
  placed runs and sync use), so it is also authenticated in transit; the signed
  digest additionally binds what is installed to what the master chose. At most
  128 MiB is accepted.
- **Platform from the bytes, not the claim.** The manifest must name this member's
  OS and architecture, and the content itself must be a 64-bit Mach-O executable
  (`MH_EXECUTE`) for it, thin or a universal binary with such a slice whose own header
  agrees with the table (`executable_format`, unit-tested on crafted headers and the
  test binary). Anything else is `wrong_platform`.
- **Install through the installation path, from a helper.** Installing stops the
  service, so the service cannot do it itself. It stages the build and a copy of the
  installed executable owner-only in `upgrade/`, then starts its own executable's
  hidden `upgrade-helper` command in a new session (`setsid`), which outlives the
  service's `bootout` (checked with a throwaway LaunchAgent before building on it).
  The helper calls `installation::install` with the staged build, exactly what
  `elsecall install` does: the same ownership and conflict checks, atomic replacement
  of the executable at the same path, the same session and LaunchAgent, the restart.
  The executable never moves, so the Application Firewall rule keeps applying.
- **Readiness, then rollback.** The helper waits up to 20 seconds for the local API
  to answer with the new build ID and, if the build it replaces had one, for the
  fleet identity (fleet state opened, member listener bound), so a build that would
  cut the member off from its master is never confirmed; the same process must
  answer again 2 seconds later. Otherwise it installs the previous executable the
  same way and confirms that build, and records `rolled_back` with the build's
  error. Only if that fails too is the outcome `rollback_failed`, with the previous
  build kept at a path the hint names. Before installing, the helper checks the
  staged file against the full digest the master signed. If `install` refuses before
  writing anything (another install holds the installation lock, a conflict), the
  service that staged the build still runs as the same process and the installed
  executable is still its build: the outcome is `interrupted`, with no rollback. Any
  other failure, such as one stopping the service after the executable was written,
  is rolled back. The helper holds a lock while it works, and the staging service learns
  when it exits, so an attempt never stays open on a running service.
- **Staging requires a real installation.** The member stages only when its service
  runs from the installed executable and that file's build is the running build, so
  a rollback restores exactly what ran.
- **In-flight work: wait, then defer; never stop.** Once a build is staged the member
  answers placement quotes with `member_upgrading`, so placement leaves it out as it
  does any member it cannot ask, and refuses placed offers as busy (a run required on
  that member waits, within its hold limit). It waits until no run or lease runs or
  queues there (requests it is placing elsewhere count as queued),
  up to the manifest's bound (`--wait`, default 5 minutes, at most an hour). With
  work left at the bound the member is `deferred`: the staged build is discarded,
  placed runs are accepted again, and the report lists what ran. Local requests are
  never refused while waiting, so the operator's own work is not blocked; one made in
  the second between idle and restart behaves as at any service restart (grants
  survive, queued requests end with `service_unavailable`). Policy chosen over
  stopping admission for the whole wait (which turns the operator's work away for
  minutes) and over a forced restart (which kills placed runs).
- **The outcome survives the restart.** The member's `upgrade/state.json` (owner-only,
  written durably, frozen format) moves through `draining` and `installing` to a
  terminal state: `upgraded`, `rolled_back`, `rollback_failed`, `deferred` or
  `interrupted`. `/peer/v1/upgrade/status` (open to any enrolled member) answers the
  running build and that record. The master uses it to skip members that already
  run the build, then polls it across the restart until the state for its upgrade ID
  is terminal, within the wait plus 90 seconds, else `unknown`. If the answer to the
  content is lost, the master follows the record too, rather than assuming nothing
  changed; the route also names the attempt the service is still receiving or
  checking, so one not recorded yet is waited for. A service that starts, or receives a new offer, and finds an attempt that
  cannot finish (it was waiting when its service stopped, or its helper's lock is
  free) closes it as `interrupted`. An unreadable record is set aside
  (`state.json.unreadable`) rather than disabling upgrades.
- **What is frozen.** Upgrading a member on any build with upgrades must keep
  working, so later builds must keep: the upgrade channel and its messages, the
  manifest transcript, `/peer/v1/upgrade/status` and the record's format, the
  member request and response signatures, the `schema_version: 1` envelope, and the
  local `/v1/health` `build` field the old build's helper reads to confirm the new
  one.
- **Members in parallel.** Each member verifies, waits, installs and rolls back on
  its own, so one member's work or failure does not hold the others. Outcomes are
  reported per member; the command exits 0 only if every member runs the build.
- **Version skew: work refuses, repair stays open.** Every service names its build
  on each member request (`x-elsecall-build`). Members refuse work between
  different builds with `version_skew` (`version_skew::refusal`, unit-tested):
  placement, configuration sync, workspace checks and reports, whose messages change
  between builds without compatibility guarantees. Status, the roster, the
  placement-authority probe and the upgrade routes stay open across builds, so a
  skewed fleet can always see itself, change membership and be upgraded. Equality of
  builds, not of versions, decides, because nothing keeps the other wire formats
  compatible within a version. The header is not signed: it decides compatibility,
  not authority, and a member lying about it only refuses itself work. Requests
  that name no build come from builds that predate upgrades and are not refused.
- **A process service manager for tests.** `ELSECALL_SERVICE_MANAGER=process`
  (`process_service`, logged as `test_seam_in_use`, honoured by debug builds only)
  runs the installed service as a plain background process instead of a
  LaunchAgent, so tests install, upgrade and roll back real services in temporary
  homes through the same `install` code. It signals only a process that runs the
  installed executable.

## Automated evidence

`mise run check` passes (formatting, Clippy with warnings denied, every suite). Two
timing comparisons in `tests/workspace_copy.rs`
(`warm_syncs_carrying_refs_cost_what_they_did_without`,
`a_first_sync_with_an_object_source_sends_only_local_work`) failed in some runs
while the development Mac ran production Elsecall work; they fail the same way on
`mergegate/main` under that load (2 of 3 runs). Test members now get 15 seconds to start instead of 5,
since every service reads its executable to identify its build and dozens of
debug builds start at once.
`tests/fleet_upgrade.rs` installs real services in temporary homes with each build's
own `install`, enrols them over loopback with fake tailnet discovery, and uses real
builds: this test build, copies re-signed under another identifier (a different
build ID) and `/usr/bin/false` as a build that never becomes ready:

- **One member, then all.** With two members on an older build, `fleet` and `doctor`
  flag both as skewed and `report` gets `version_skew` from them. `--member bravo`
  upgrades only bravo (new PID, the installed file is the master's build, nothing
  staged left behind) and clears its skew; `fleet upgrade` then upgrades charlie and
  reports bravo `already_current`, never the master; the skew clears.
- **Non-master.** `fleet upgrade` on a non-master fails `not_master`; `--member` of
  the master is `invalid_request`; a non-master's key opening the upgrade channel on
  another member is refused `not_master`.
- **Tampered and foreign builds.** Acting as the master with its key: one flipped
  byte after signing (`digest_mismatch`), a manifest signed by another member or for
  another member (`manifest_unverified`), a declared other architecture, a script and
  a Mach-O header for the other architecture (`wrong_platform`). Afterwards the
  member runs the same process and executable with nothing staged, and the genuine
  build still upgrades it.
- **Rollback.** `--executable /usr/bin/false` targeted at one member: `rolled_back`
  with `startup_failed`; the member serves its previous build from the same
  installation and stays reachable, with no skew.
- **In-flight work.** With a lease held on the member, `--wait 1s` is `deferred`,
  listing the lease; the service did not restart and the lease kept running. With
  `--wait 60s` the member reports `draining` with the lease, refuses a second offer
  with `upgrade_in_progress`, and does not restart until the lease is released, then
  upgrades.
- **Attempts that cannot finish.** A service restarted while it waits reports
  `interrupted` (`service_restarted`), installs nothing, and the lease survives the
  restart. An install refused before anything stopped (the test holds the
  installation lock) is `interrupted` (`operation_in_progress`) with the same
  service still running, and the next upgrade succeeds.
- Also: an expired manifest (`manifest_expired`) and a size beyond the limit
  (`build_too_large`) are refused before any content.
- Unit tests for build IDs, Mach-O classification, manifest signing (every field is
  signed), upgrade IDs, blob limits, the journal and the skew policy.

## Installed-system evidence

Deferred to the batch release verification. Both machines carry live production
Elsecall work, so restarting their services for evidence would disrupt placed runs.
The steps to run then, at a quiet time, with the master `mac-mini` and the
member `macbook-pro` (account `devbox`, `ssh -o BatchMode=yes devbox`):

1. **Bootstrap once.** The devbox runs a build from before upgrades
   (`upgrade_unsupported`). Copy the release build there and run its `install`:
   `scp target/release/elsecall devbox:/tmp/elsecall-next`, then
   `ssh devbox /tmp/elsecall-next install`, then `ssh devbox ~/.local/bin/elsecall status`
   (it shows the `build`).
2. **A visible build change.** Build a release containing any further commit; on
   the Mac run `./target/release/elsecall install`. `elsecall fleet` then lists
   `macbook-pro` under `version_skew`; `elsecall doctor` there shows
   `version_skew`. In a git working tree, `elsecall run --member macbook-pro -- true`
   fails with `version_skew`.
3. **Upgrade.** On the Mac: `elsecall fleet upgrade`. Expect `macbook-pro`
   `upgraded` with `running.build` equal to the Mac's `status` build, and
   `elsecall fleet` without `version_skew`. On the devbox:
   `launchctl print user/501/dev.elsecall.agent` still runs in the background
   session with a new PID, and the account's other agents keep their PIDs.
4. **Firewall.** `elsecall doctor` on both machines: healthy, with
   `member_listener_firewall` `allowed`; `elsecall fleet` from the Mac reaches the
   devbox over the tailnet.
5. **Rollback, safely.** Build a deliberately broken release (a copy of the tree
   whose `serve` exits at once), then on the Mac
   `elsecall fleet upgrade --member macbook-pro --executable <broken build>`.
   Expect `rolled_back` with `startup_failed` and `running.build` the build from step
   3; the devbox's service answers again and `doctor` there is healthy with the
   firewall `allowed`. The devbox's `service.log` shows `upgrade_rolling_back`.
6. **Work in flight.** With a lease held on the devbox
   (`elsecall lease acquire --size 1 --holder $$` in a shell that stays open),
   `elsecall fleet upgrade --member macbook-pro --wait 10s` is `deferred` listing
   the lease, and the lease keeps running.

## Limits

- macOS only, like the rest of Elsecall. Builds are distributed, never built on
  members. Nothing upgrades automatically, and configuration is not synced.
- The first build with upgrades must be installed on each member by hand.
- A build is ready when its local API answers with its build ID and keeps running for
  2 seconds. A build that passes that and fails later (for example only when it
  first places work) is not rolled back; `elsecall fleet` shows its state.
- If the helper itself dies mid-install (the machine loses power), the member may
  run either build; its next service start records `interrupted`, and launchd keeps
  whichever build is installed running. Run `doctor` there.
- Placed runs are turned away while a member waits for its work; if every member is
  upgraded at once, placeable work runs on its calling member meanwhile.
- The build header is advisory: an enrolled member that sends another member's build
  ID gets work it cannot handle, which hurts only itself.
- A rollback restores the previous executable, not state: it relies on state files
  (`fleet.json`, the SQLite databases) staying readable by the previous build. No
  build migrates them yet; one that does must keep them readable by its predecessor
  or accept that its rollback may fail.
- The helper is covered end to end with the process service manager; the launchd
  path (the helper surviving `bootout` and bootstrapping the service) is covered by
  the installed-system evidence.
- A member's upgrade record names the runs and leases it waited for (IDs and
  labels), readable by any enrolled member, as `fleet` already shows their records.
