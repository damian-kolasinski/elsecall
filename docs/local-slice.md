# Local installation and inspection

## Decisions subsequent slices can build on

- One executable provides Clap commands and foreground `serve`. Source builds are
  the initial distribution mechanism. Installation copies the executable to private
  per-user storage and exposes a link in `~/.local/bin`.
- macOS lifecycle is a LaunchAgent in the GUI user domain. No root installation,
  privileged helper, or pre-login operation is introduced.
- The application-facing `MachineInspector` and `ServiceManager` ports isolate OS
  observation and lifecycle management. Separate macOS adapters implement them.
  `Paths` owns this version's macOS layout; Linux needs its own layout and adapters.
- Axum exposes `GET /v1/health` and `GET /v1/machine` over a Unix socket. The CLI uses
  the same API. `schema_version: 1` envelopes and typed payloads are the interface
  contract; no remote/network transport or web surface is enabled.
- Machine access requires the same effective UID at both ends of the socket, plus
  a user-owned 0700 state directory and 0600 socket. Installation checks each
  directory below HOME for ownership, symlinks and other-user write access. Same-UID
  processes share authority; this is not a sandbox against the account's own code.
- The service collects only hostname, OS version, architecture, CPU count/load,
  memory totals and uptime. It does not enumerate processes, read environment
  variables other than HOME for layout, or inspect credentials.
- Installation and service instance locks prevent competing operations and stale
  socket deletion by a second service. Updates replace owned files atomically.
  A startup failure leaves a repairable installation; it does not silently claim
  success or automatically roll back a prior executable.
- Ownership is proven by the installation receipt or by a CLI/LaunchAgent link that
  points at its owned target. Install repairs, and uninstall removes, an installation
  whose receipt or state directory was deleted. Owned files or a loaded LaunchAgent
  with neither proof are a conflict, since the job may belong to another HOME.
- There is no database yet because this slice has no durable operational state.
  A future slice can add SQLite when it has behavior that requires it.

## Automated evidence

`cargo fmt --check`, warning-free Clippy, and the locked Cargo test suite cover:

- Same-user authorization and safe LaunchAgent configuration encoding.
- Install/update/restart/uninstall using real files and a fake lifecycle boundary,
  failed startup, conflicting links, unknown-file preservation and unsafe parents,
  recovery after a lost receipt or deleted state directory, and refusal of
  concurrent lifecycle operations.
- Real binary/service health and machine queries, missing service, duplicate
  instances, crash/stale-socket recovery, graceful shutdown and socket permissions.
- API propagation of an external observation failure, unknown routes, invalid,
  incompatible or oversized responses and request timeout.

## Installed-system evidence

Verified on the development Mac on 2026-10-31, using the release build on macOS
26.4 / Apple Silicon:

- `elsecall` resolves through `~/.local/bin`; install returns only after the API is
  ready. Status, machine and doctor returned valid JSON and doctor was healthy.
- Machine inspection reported 14 logical CPUs and 68,719,476,736 bytes of RAM,
  plus current load averages, memory accounting and machine uptime.
- `service restart` produced a new service PID and a healthy API.
- Killing only the Elsecall service with SIGKILL caused launchd to restart it with
  a new PID; the stale socket recovered automatically.
- Explicitly unloading the LaunchAgent made status return `service_unavailable`
  with exit 1; doctor reported `launch_agent_loaded: false`. Restart recovered it.
- Uninstall removed both links and the entire owned state directory, and launchd
  reported the registration absent. Repeated uninstall succeeded.
- Reinstall followed by an update produced a healthy service with a new PID.
  The final installation was left running. Directory/socket modes were 0700/0600.

All automated tests passed separately from these real installation checks.
Formatting and Clippy with warnings denied passed. The architecture, behavior and
security review found two issues (restart ordering and diagnostic integrity);
both were fixed with regression coverage, and the follow-up review found no
remaining blocker. Login persistence is configured but was not tested by logging
out or rebooting the operator's Mac. Follow-up fixes for ownership recovery
and lock contention are covered by the automated suite. The LaunchAgent adapter
now waits for launchd to unload the job before bootstrap; tests fake that
boundary. On 2026-11-02 a release build updated the installed service, then
`service restart` produced a new PID and doctor reported healthy.

## Deliberate limits

macOS desktop accounts only. Service availability begins at login. HOME must be an
absolute UTF-8 path and the full Unix-socket path must be shorter than 104 bytes.
No CPU percentage sampling, thermal data, history, fleet enrollment, dispatch,
synchronization, secrets, web UI or Linux implementation. Login persistence is
configured through launchd; a logout/reboot test is a separate disruptive manual
check. Logs are small lifecycle records. Subsequent work added service-owned
size rotation (10 MiB, five archives by default), global sync backup retention,
SQLite space reclamation, and disk-size diagnosis. See [State retention](state-retention.md)
for current limits; the original scope and installed checks above describe this
initial slice only.
