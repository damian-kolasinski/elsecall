# Integrating a project

An existing repository adopts Elsecall with a two-line guard and one line per
script or one prefix per heavy command. Workload settings belong to the user,
without any configuration file committed to the repository. An optional personal
default policy:

```toml
# ~/Library/Application Support/Elsecall/workloads.toml
[admission]
when_unavailable = "run"   # keep working, unmanaged, while the service is down

[[labels]]
match = "build *"
size = 4
[[labels]]
match = "test *"
size = 3
hold_limit = "5m"
[[labels]]
match = "warm *"
size = 4
urgency = "deferrable"
deadline = "30m"
at_deadline = "start"
```

```sh
# Scripts/lib/elsecall.sh, sourced by the repository's scripts.
# Without Elsecall installed, `ec` runs only the command after --, with
# ELSECALL_ADMISSION=missing (a lease is a no-op).
EC=$(command -v elsecall || echo "$HOME/.local/bin/elsecall")
ec() { if [ -x "$EC" ]; then "$EC" "$@"; else while [ $# -gt 0 ] && [ "$1" != -- ]; do shift; done; [ $# -gt 0 ] && shift; [ $# -eq 0 ] || ELSECALL_ADMISSION=missing "$@"; fi; }
```

```sh
#!/bin/sh
# Scripts/build.sh: the whole script is one request, entered at its top.
. Scripts/lib/elsecall.sh
[ -n "${ELSECALL_ADMISSION:-}" ] || check_inputs "$@" || exit 2   # cheap checks: once, before waiting
[ -n "${ELSECALL_ADMISSION:-}" ] || [ ! -x "$EC" ] || exec "$EC" run --label "build $1" -- "$0" "$@"
generate_project && xcodebuild -scheme "$1" build
```

```sh
. Scripts/lib/elsecall.sh

# A heavy command: one prefix. Its output and exit code are the command's.
ec run --label "build $SCHEME" -- xcodebuild -scheme "$SCHEME" build

# Machine-bound deferrable work, such as a cache warm a scheduler starts: the
# "warm *" rule makes it wait while interactive work waits, and start at 30 minutes.
ec run --label "warm $SCHEME" -- Scripts/warm-cache.sh "$SCHEME"

# A heavy section of shell that is not one command: a lease.
ec lease acquire --label "test $MODULE" >/dev/null || exit 1
generate_project && run_tests "$MODULE"
ec lease release >/dev/null

# The run ID for your own telemetry, without parsing JSON.
id_file=$(mktemp)
ec run --label "test $MODULE" --run-id-file "$id_file" -- Scripts/test.sh "$MODULE"
status=$?
read -r elsecall_run_id < "$id_file" || elsecall_run_id=""
rm -f "$id_file"
```

Test the scripts without a service, and switch Elsecall off in an emergency:

```sh
ELSECALL=off Scripts/build.sh   # every run executes directly; leases succeed, doing nothing
```

Set `ELSECALL=off` in front of a script or in a subshell: in POSIX `sh`, a variable
set in front of a shell function such as `ec` stays set after it. Likewise, do not
pipe `ec lease acquire` into another command: the pipeline's subshell would hold
the lease and end it at once. Redirect it instead.

What consumers no longer write:

- **Finding the binary.** The CLI link is always `~/.local/bin/elsecall`; the
  guard above uses PATH first, then that location, then runs the work directly.
- **Running anyway.** `when_unavailable = "run"` (or `ELSECALL_WHEN_UNAVAILABLE=run`,
  or `--when-unavailable run`) makes a `run` whose service cannot be reached run the
  command directly, its output and exit code unchanged, after exactly one notice
  line on stderr. `lease acquire` then succeeds with `state` `unmanaged`, holding
  nothing, and `lease release` succeeds too. The default is still to fail with
  `service_unavailable`. It applies whenever the request ends with
  `service_unavailable`, including when the service stops while the request
  waits, but never to `--member` work, which is bound to that member. During a
  handover to another build the request first waits for the new service (see
  ["Admission and leases"](https://github.com/damian-kolasinski/elsecall#admission-and-leases) in the README), so it runs managed, not unmanaged.
- **Formatting failures.** On exit 125 the stderr line before the envelope already
  tells the agent what to do (see "Failure lines"); pass it on unchanged.
- **Re-exec retries.** A script that re-execs itself and acquires again gets its
  own lease back instead of queueing (see "Acquiring again" under
  ["Admission and leases"](https://github.com/damian-kolasinski/elsecall#admission-and-leases) in the README).
- **Sizes and deadlines per operation.** They live in the label rules.
- **Private re-exec guards.** Every command `run` starts, in every mode, sees
  `ELSECALL_ADMISSION` (see "Wrapping a whole script").

## Wrapping a whole script

The entry line in `Scripts/build.sh` above re-executes the script through
Elsecall, so the whole script is one request: one record, one size, one place
to run. It is the recommended way to wrap a script that agents call.

- **Exactly once, in every mode.** `run` sets `ELSECALL_ADMISSION` in the
  command's environment however the command came to run, so the re-executed
  script skips the line. Without Elsecall installed there is nothing to re-execute
  through, so `[ ! -x "$EC" ]` lets the script continue in place.

  | `ELSECALL_ADMISSION` | The command runs |
  | --- | --- |
  | `granted` | admitted with capacity of its own, here or on the member it was placed on |
  | `nested` | inside another run or lease, sharing its grant |
  | `unmanaged` | without admission: the service was unavailable and the consumer chose to run anyway |
  | `off` | directly, under `ELSECALL=off` |
  | `missing` | through the guard's `ec`, because Elsecall is not installed |

  The first four are the status file's `admission`. Only Elsecall and the guard
  set the variable, and Elsecall never reads it: nesting follows processes, not
  variables. Placed work receives the member's value, never the caller's.
  `lease acquire` sets nothing: it cannot change its caller's environment, and a
  lease covers the caller where it already runs, so nothing re-executes.
- **Signals.** The line `exec`s the Elsecall binary, so the script's PID stays
  the process to signal. Elsecall forwards hang-up, termination and interrupt to
  the re-executed script, here or on the member, and exits as it did. Do not
  call the `ec` function there instead: a function cannot be `exec`ed, so a shell
  would stay in between, and a signal to the script's PID would stop only that
  shell and orphan the run. What the script then does with its own children is as
  without Elsecall: `exec` the last step, or `trap`, to pass signals on.
- **Checks before admission.** Guard cheap checks, such as validating arguments
  or linting, with the same condition and put them before the entry line. They
  run once, before the request waits for capacity, and not again inside the run.
- **Call the script by its path.** Elsecall runs `"$0"` itself, so the script
  must be executable with a `#!` line, and invoked by a path with a slash, as
  `Scripts/build.sh` or `./build.sh`. `sh build.sh` would leave a bare name that
  Elsecall looks up on PATH, and `bash Scripts/build.sh` re-executes under the
  `#!` interpreter, not bash. A placed run re-executes it in the member's copy:
  use a path relative to the working directory, because an absolute `$0` names
  your tree, not the copy.
- **Under another run it runs in place.** The marker is inherited like any
  variable, so a wrapped script started inside a run, whether by another wrapped
  script or through an `ec run` prefix, sees it and runs inside that run's grant.
  It queues for nothing and adds no record, as any nested request would, but its
  guarded checks are skipped too. Call a wrapped script directly, not through
  `ec run`: its own line admits it.

Which to use:

- **Entry line** for a script that is one unit of heavy work, such as a build or
  test wrapper. It is placeable as a whole when its label rule says so, keeps its
  steps together on one member, and gives agents one PID to signal. `ec run`
  calls inside it nest at once.
- **Per-command prefix** (`ec run -- …`) for one heavy command among light steps,
  when each heavy command should be its own request with its own label and size.
- **Lease** for a heavy section of shell that is not one command and must stay on
  this machine, or wherever the script cannot be re-executed, such as a sourced
  script or a function. Leases are always pinned.

## Failure lines

Elsecall's own failures of `run` and `lease acquire` write one plain line on stderr
before the JSON envelope: for `run`, the envelope is the last stderr line after it;
for `lease acquire`, the envelope is the JSON document on stdout. The line has the
form `elsecall: <code>: <what happened> <what to do>`, contains no control
characters, and its `<code>` is the envelope's. For `fleet_busy`:

```text
elsecall: fleet_busy: No capacity within the 90 s hold limit: 0 of 12 cores are free and 2 other requests wait. Wait about 40 s, then rerun the same command once. `elsecall runs` shows what is running and queued; `elsecall explain r-3f9a0c41d2b7` shows why.
```

The seconds are the envelope's `details.retry_after_seconds`. Other codes end with
their fix, such as correcting `workloads.toml` for `invalid_request`,
and with `elsecall explain RUN_ID` when a record exists. For `config_sync_failed`,
whose `details` hold the `member` and the underlying `cause` code:

```text
elsecall: config_sync_failed: The synced configuration on macbook-pro could not be brought up to date before the run (unsafe_sync_path), so the command did not run there. Run `elsecall sync push --member macbook-pro` to see why and retry the sync, fix what it reports, then rerun the same command. `elsecall explain r-3f9a0c41d2b7` shows the record.
```

Work that proceeds
without admission writes the notice
`elsecall: unmanaged: <reason> The command runs without admission. Run `elsecall
doctor` to restore admission.` (for a lease: `No lease is held, and the work runs
without admission.`). Branch on the envelope's `code`, not on the line's wording.

## Environment controls

| Variable | Values | Effect |
| --- | --- | --- |
| `ELSECALL` | `off`, `on` | `off`: `run` executes the command directly with exact pass-through (no notice; only `ELSECALL_ADMISSION=off` added to its environment) and `lease acquire`/`release` succeed with `state` `off`. New work still validates options and `workloads.toml`. Lease release can return an existing grant even if a later configuration edit is invalid. |
| `ELSECALL_HOLD_LIMIT` | a duration such as `90s` | The hold limit of interactive requests without `--hold-limit`. Deferrable requests ignore it. |
| `ELSECALL_WHEN_UNAVAILABLE` | `run`, `fail` | What to do when the service cannot be reached. |

An empty value counts as unset; any other value fails with `invalid_request`.

## Precedence

Each request property comes from the first layer that sets it: **command-line
flags, then the environment, then the matching label rule of the selected personal `workloads.toml` policy,
then defaults.** `--interactive`/`--deferrable` and `--pinned`/`--placeable`/
`--member` override a rule's urgency and placement; `--hold-limit`, `--deadline`
and `--at-deadline` only set values, so `--hold-limit` on a label whose rule is
deferrable fails with `invalid_request` and asks for `--interactive`.

## Run IDs for your telemetry

`--run-id-file PATH` (on `run` and `lease acquire`) empties the file when the
request starts and writes the run ID and a newline once admission decides,
including for failures with a record such as `fleet_busy`. It stays empty when
there is no record (unmanaged or `ELSECALL=off` work, or failures before
admission). Commands that `run` admits also see `ELSECALL_RUN_ID`. Correlate with
`elsecall explain RUN_ID`.

## Outputs of placed work

A placed run executes in a copy of the working tree on another member and brings
back only the paths `workloads.toml` declares under `[outputs] paths` (see
["Running on another member"](https://github.com/damian-kolasinski/elsecall#running-on-another-member) and
["Workload configuration"](https://github.com/damian-kolasinski/elsecall#workload-configuration) in the README). Declare the
files the caller needs afterwards, such as test reports and logs, as paths inside
the working tree.

- **No path through a symbolic link.** Elsecall never follows a link, on either
  side, so a link cannot redirect a write outside the tree. An output whose path
  passes through one is reported as not returned, and the command's exit code is
  unchanged.
- **Rust projects that build through mbx.** mbx replaces `target/` with a link
  into its cache, on the caller and on the member alike. Write what should come
  back outside `target/`, and declare that path. For nextest, whose JUnit report
  defaults to `target/nextest/<profile>/junit.xml`, set `[store] dir =
  "build/nextest"` in `.config/nextest.toml`. Otherwise copy the file out of
  `target/` as the run's last step.
- **Ignored paths are fine.** Outputs usually sit in ignored build directories.
  The copy keeps its own ignored state between runs, and only declared outputs
  replace yours.
