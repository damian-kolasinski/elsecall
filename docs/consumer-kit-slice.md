# C: Consumer integration kit

Delivers the C row of [Fleet scheduling](fleet-scheduling.md): plugging Elsecall
into an existing repository's scripts takes one `.elsecall.toml` and one command
prefix. The first consumer integration needed a 186-line bash helper to find the
binary, run anyway when Elsecall was unavailable, parse JSON, phrase `fleet_busy`
for agents, avoid re-queueing on a re-exec, default the hold limit, switch off for
tests, and keep per-operation sizes and deadlines. Elsecall now does each of these.

## Contract changes

`docs/fleet-scheduling.md` changed in these places:

- **Work requests:** properties follow a precedence (flags, environment, label
  rule, defaults) and records keep their provenance.
- **Entry points:** a holder that acquires again gets its own lease back; one
  release ends it. A new "Consumer controls" section: the off switch, running
  anyway, the hold-limit default, run IDs and finding Elsecall.
- **Input and output:** the failure line before the envelope, "Unmanaged work",
  and `service_unavailable` no longer always means the command did not run.
- **Consumer configuration:** label rules with placement, and the unavailable
  choice.
- **Delivery:** the C row.

## Decisions

- **Modules.**
  - `consumer_configuration` parses label rules into types that cannot hold a
    contradiction: `RuleUrgency::Interactive { hold_limit }` or
    `Deferrable { deadline, at_deadline }`, with both deadline fields required, and
    a deferrable rule never placeable. Errors carry the rule's number and pattern
    and a specific fix, which becomes the failure's hint.
  - `admission_cli::environment` reads `ELSECALL`, `ELSECALL_HOLD_LIMIT` and
    `ELSECALL_WHEN_UNAVAILABLE` once, as untrusted input, never echoing values.
  - `admission_cli::resolution` is the one pure function that merges the layers.
    It returns the validated request with `settings` (provenance) and the
    unavailable choice. `run` and `lease acquire` both call it.
  - `guidance` formats the agent-ready lines; `run_id_file` writes `--run-id-file`.
  - `scheduling::settings` is the provenance type the service validates and
    records.
- **Urgency is chosen, values only set.** `--interactive`, `--deferrable` and a
  rule's `urgency` (or a rule's `hold_limit`, which implies interactive) choose the
  urgency. `--hold-limit`, `--deadline` and `--at-deadline` only set values of the
  chosen urgency, so a flag never silently switches a deferrable rule to
  interactive; the error names the rule and asks for `--interactive`.
  `ELSECALL_HOLD_LIMIT` is a default and is ignored by deferrable work.
- **Placement.** Rules may say `pinned` or `placeable`; `--pinned` is new so the
  caller can override a placeable rule. Leases ignore a rule's placement.
- **Running anyway** triggers only on `service_unavailable`: no connection, or the
  service stopping while the request waited. `service_timeout` and every other code
  still fail, since the service exists and the work may be admitted. `--member`
  work never runs unmanaged, since it is bound to that member. Release follows the
  same choice and succeeds with `state: unmanaged`. Everything a placed run needs
  (the copy's configuration and cache directory) is prepared before admission, so
  no failure falls between a grant and its start without the run's ID.
- **Direct execution.** Unmanaged and off runs spawn the command as a child with
  the same signal forwarding and exit-status mirroring as admitted runs. They
  added no variables at first; since the entry-line follow-up below they add only
  `ELSECALL_ADMISSION`. A child rather than `exec` keeps `--status-file` working:
  it records `admission: unmanaged|off` with null `run_id` and `member`.
- **Off still validates.** `ELSECALL=off` checks options and `.elsecall.toml` and
  that the command exists, so consumers' tests with `off` catch broken
  integrations. It never contacts the service, nor checks the platform or user.
- **Lease reuse** happens in the service, under the admission lock, before nesting:
  an active lease whose holders include the requested holder (PID and start time,
  which survive `exec`) is returned with `reused: true`. Its record counts
  `reacquired` and warns once if a larger size was asked. Only the exact holder
  reuses; descendants still nest.
- **Failure lines** come from the failure's code, message and hint, with
  code-specific next steps for `fleet_busy`, `service_unavailable`,
  `request_too_large`, `deadline_skipped` and `connection_lost`. Control
  characters become spaces, so a line never splits. `lease acquire` writes the line
  on stderr and keeps its JSON on stdout, including for Clap usage errors.
- **Run ID file** is emptied first, so a stale ID is never read, and written once
  admission decides (granted or failed with a record). It is owner-only and
  replaced atomically. A usage error empties a named file too.

## Automated evidence

`mise run check` passes.

- Unit tests:
  - rule parsing, first-match-wins and `?`/`*` globs;
  - every invalid-rule message and fix;
  - the README's two TOML examples parse;
  - environment parsing;
  - resolution precedence for every property, urgency overrides, contradictions
    naming the rule, and leases staying pinned;
  - the exact failure and notice lines.
- `tests/consumer_kit.rs`, with real CLI processes and, where admission is
  involved, a real service:
  - rules applied to `run` and `lease acquire`;
  - flag and environment overrides;
  - `explain`'s `settings`;
  - the run ID file matching `ELSECALL_RUN_ID`;
  - invalid rules exiting 125 with exactly the line and the envelope;
  - with no service:
    - the default `service_unavailable`;
    - running anyway chosen in the file, the environment and a flag, each with a
      200 KB binary round trip, byte-exact stdout, stderr exactly the notice plus
      the command's own bytes, and exit 7;
    - flag-over-environment-over-file precedence;
    - unmanaged lease acquire and release;
  - `ELSECALL=off`:
    - byte-exact pass-through with no added output, even for a size no machine
      has;
    - an untouched environment (since the entry-line follow-up, but for
      `ELSECALL_ADMISSION`);
    - death by the command's signal;
    - no-op leases;
    - invalid values rejected;
  - a script that re-execs itself gets its lease back ahead of a queued outsider,
    with one record, and one release ends it;
  - exact `fleet_busy` lines for `run` and `lease acquire`, `request_too_large`
    and usage errors.
- `tests/admission.rs`: the nested-lease test's inner shell now runs two commands,
  so it stays a child holder rather than exec'ing into the outer holder (which now
  reuses). The deferrable-order test waits for the log it asserts on; it raced
  under full-suite load.

## Real-machine evidence

On 2026-11-03 this Mac mini (macOS 26.4, M4 Pro, capacity 9 after a reserve of
3) ran the release build, installed with `elsecall install` in the desktop
session. A scratch working tree held the README's integration `.elsecall.toml`,
and a `/bin/sh` script used the README's `ec` guard and calls:

```text
== build (rule size 4), run ID file
built in r-973649a48c8d
exit=3
run id file: r-973649a48c8d
4.0 {'rule': {'number': 1, 'match': 'build *'}, 'size': 'rule', 'urgency': 'default', 'hold_limit': 'default', 'placement': 'default'}
== warm (deferrable rule)
exit=0
{'deferrable': {'deadline': '2026-11-03T09:41:45.690Z', 'at_deadline': 'start'}} {'rule': {'number': 3, 'match': 'warm *'}, ...}
== ELSECALL=off
off: run id=none
exit=0
{"schema_version":1,"data":{"state":"off","run_id":null,...}}
== lease around a section, then re-exec
granted r-67e8be0d513a 3.0
== after re-exec: acquire again
reused r-67e8be0d513a 0
1                                   (the record's reacquired)
{"schema_version":1,"data":{"state":"released","released":["r-67e8be0d513a"]}}
```

With another process holding all 9 cores, `elsecall run --label "test Busy"
--hold-limit 1s -- true` exited 125 with exactly two stderr lines:

```text
elsecall: fleet_busy: No capacity within the 1 s hold limit: 0 of 9 cores are free and 0 other requests wait. Wait about 60 s, then rerun the same command once. `elsecall runs` shows what is running and queued; `elsecall explain r-7ba85458973d` shows why.
{"schema_version":1,"error":{"code":"fleet_busy",...,"run_id":"r-7ba85458973d","details":{"retry_after_seconds":60,...}}}
```

`lease acquire` with the same options exited 1 with the same line on stderr.

The missing-binary guard was run in `sh`, `bash`, `zsh` and `dash` with no
Elsecall on the path. In each shell:
- the command after `--` ran with its arguments intact and its exit code (7);
- `lease acquire … || exit 1` and `lease release` succeeded.

Running anyway with the service down was not exercised on the installed service:
stopping it would have disturbed other sessions using it. `tests/consumer_kit.rs`
covers it with no service at all. The devbox was not changed; it runs the S4
build.

Two script mistakes during this check are worth knowing as a consumer:
- `ec lease acquire | python3 …` makes the pipeline's subshell the holder, so the
  lease ends at once. Redirect to a file instead.
- In POSIX `sh`, `ELSECALL=off ec …` in front of a shell function leaves
  `ELSECALL=off` set afterwards. Put it in front of a script, or in a subshell.

## Limits

- Reuse is per holder, as the contract asks. A section inside the same process
  (a shell function, or `sh -c` with one command, which execs into `elsecall`)
  therefore shares the outer lease, and its release ends it. Before this change,
  such a section got a separate nested lease that a release by ID ended alone; a
  release by holder already ended both. A separate nested lease now needs a child
  process.
- Two acquires from one holder that are both still waiting are separate requests:
  only an active lease is reused.
- If an unmanaged command cannot start after its notice (an exec failure that
  `preflight` cannot see, such as a script without a shebang line), stderr has
  the notice, the failure line and the envelope. A status file that cannot be
  written adds its warning line, as for admitted runs.
- `Settings` are validated for consistency with the request, but a caller with
  local socket access can still record provenance that does not match its flags.
  Records are the caller's own.

- A lease that survives a service restart cannot be released while the service is
  down; with running anyway it ends when its holder exits.
- Unmanaged work leaves no record, so `elsecall runs` cannot show it.
- `ELSECALL_*` controls forwarded to placed work are not interpreted there: the
  member's `elsecall` inherits only what `.elsecall.toml` forwards.

## Follow-up: the entry line

The simplest integration re-execs a wrapper through Elsecall at its top. Its
guard was `ELSECALL_RUN_ID`, which only managed runs set, so the wrapper wrapped
itself forever under `ELSECALL=off`, when running unmanaged, and when the guard's
`ec` stood in for a missing binary. acme-ios invented private guard
variables to avoid that. It also found that the entry must `exec` the binary
rather than call `ec`: with a shell function in between, a hang-up sent to the
wrapper's PID stopped only that shell and orphaned the run.

### Contract changes

`docs/fleet-scheduling.md`:

- **Consumer controls:** a new "Admission marker": every command a run starts
  sees one variable saying how it came to run, in every mode and on the member
  a run was placed on, and Elsecall never reads it. Leases set nothing. The off
  switch adds only the marker. "Finding Elsecall": the guard sets the marker too,
  saying Elsecall is missing.
- **Input and output, Environment:** unmanaged and off runs set only the marker,
  and a caller's `ELSECALL_*` variables are never forwarded to placed work (as
  already implemented).
- **Delivery:** the C row names the marker and the entry line.

The README's "Wrapping a whole script" documents the entry line, the values and
when to use it rather than a per-command prefix or a lease. "Running commands",
"Running on another member" and "Environment controls" name the marker where
they listed what Elsecall adds.

### Decisions

- **`ELSECALL_ADMISSION`**, valued as the status file's `admission`: `granted`,
  `nested`, `unmanaged`, `off`. One vocabulary for one fact, already documented
  and already computed for the status file; the variable carries it into the
  command. `ELSECALL_ENTERED=managed|…` was the alternative: it would add a
  second name and a coarser value set for the same thing. Placed runs are always
  `granted`, since nested work is never placed; the member sets it, and never
  takes the caller's.
- **`missing`** is the one value only the guard sets: `ec` runs the command after
  `--` as `ELSECALL_ADMISSION=missing "$@"`. A distinct value tells an agent
  that Elsecall is not installed, rather than switched off or down. With no
  command after `--` (a lease), `ec` still does nothing and succeeds; an empty
  `"$@"` with an assignment in front would otherwise set the variable in the
  calling shell.
- **The entry line keeps the requested shape:**
  `[ -n "${ELSECALL_ADMISSION:-}" ] || [ ! -x "$EC" ] || exec "$EC" run … -- "$0" "$@"`.
  With no binary there is nothing to re-exec through, so the wrapper continues in
  place, unmarked, which is true: nothing ran it. The alternative of exporting
  `missing` when the guard is sourced was rejected: it would mark every command
  the script runs, not only those `ec` ran, and add a third line to the guard.
- **`lease acquire` is unchanged.** It is a child of its holder and cannot set
  the holder's environment. A lease covers the caller where it already runs, so
  there is no re-exec to guard.
- **Module.** `run_admission` owns `Admission` and its variable. The status file,
  the local executor and the member's runner use it.

### Automated evidence

`mise run check` passes. `tests/entry_pattern.rs` builds a consumer repository
whose `Scripts/lib/elsecall.sh` is the README's guard block and whose
`Scripts/build.sh` uses the README's check and entry lines verbatim, so the
README cannot drift from what is tested. The wrapper is started as an agent
would, by a relative path in a process whose PID is the wrapper's, with the
test's own environment cleared. A depth limit makes a wrapper that wraps itself
exit 99 instead of hanging.

- In managed, `ELSECALL=off`, unmanaged (no service, `when_unavailable = "run"`)
  and missing-binary modes, and on a run placed with `--member bravo`, the
  wrapper's check runs once, its body once, and an `ec run` inside it once. The
  inner run sees `nested` when managed or placed, and `off`, `unmanaged` or
  `missing` otherwise. Managed and placed runs record one `build App` request.
  On bravo, the placed command finds `elsecall` on its PATH, so a missing marker
  there would wrap the wrapper again.
- SIGHUP and SIGTERM sent to the wrapper's original PID stop its hanging body in
  every mode, and the original process dies of the same signal.
- Removing the marker from direct and placed starts fails both tests: off
  recurses until the depth limit, and the placed wrapper wraps itself again on
  the member.
- `tests/consumer_kit.rs`: the off-mode environment test now expects `off`
  replacing an inherited `ELSECALL_ADMISSION=granted`, besides the inherited
  `ELSECALL_RUN_ID`.
- `run_admission` unit test: the variable's value equals the status file's.

The README's guard block was also sourced in `sh`, `bash` 5, `zsh` and `dash`
with no Elsecall on PATH or in HOME. In each shell, `ec run … -- /bin/sh -c …
'a b'` ran the command once with `ELSECALL_ADMISSION=missing` and the argument
intact, and exited 7 as the command did. `ec lease acquire` and `ec lease
release` succeeded, and the calling shell was left without the variable.

### Limits

- A wrapper invoked by an absolute path re-executes that path. On a member with
  mapped copies it names the caller's tree, not the copy, so placeable wrappers
  must be invoked by a path relative to the working directory. Mapping an
  absolute program inside the workspace to the copy would remove this.
- The marker is inherited like any variable. A wrapped script started inside a
  run, by another wrapped script or through an `ec run` prefix, runs in place in
  that run's grant without a record of its own, and its guarded checks are
  skipped; the README says to call wrapped scripts directly. Unsetting the marker
  after the entry line was rejected: it would cover only the first case, and
  cost the body the marker. A process that outlives its run, or an environment
  that exports the variable by hand, makes an entry line continue in place
  without admission.
- The entry line needs a `$0` with a slash: a bare name, as from `sh build.sh`,
  is looked up on PATH. The README says so.
