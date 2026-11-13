# Elsecall

Elsecall makes a personal fleet of development machines ready for work, observable,
and controllable. The CLI is the primary interface. See [VISION.md](VISION.md) for
the product baseline and stack.

Elsecall installs a **per-user macOS service** that reads its machine's health and
state, **admits work onto this machine** so concurrent heavy work never overcommits
it, and joins it to a **fleet** of machines that authenticate each other over the
tailnet, as [Fleet scheduling](docs/fleet-scheduling.md) specifies. The CLI and
service use Rust, Clap, Tokio, Axum and SQLite.

## Desktop UI

`elsecall-ui` is a native window that lists the fleet the local service already
reports. `elsecall install` does not install it. See
[crates/elsecall-ui/README.md](crates/elsecall-ui/README.md).

## Build and install

Requires macOS, Xcode command-line tools, and Rust 1.99.0. A desktop login is not
required. The toolchain is pinned for both rustup and mise. With mise installed:

```sh
mise install
mise run build
./target/release/elsecall install
elsecall status
elsecall machine
```

With rustup, use `cargo build --locked --release` instead of `mise run build`.
Installation copies the current executable; run the newly built binary's `install`
command again to update. In a fleet, update the master this way, then run
`elsecall fleet upgrade` there to install the same build on every other member (see
"Upgrading the fleet"). No root privileges, shell-profile edits, or network
access are required by the installed application. Dependency downloads during the
build require network access.

The CLI link is `~/.local/bin/elsecall`. If that directory is not on your PATH, use
`~/.local/bin/elsecall` directly or add this to your shell configuration:

```sh
export PATH="$HOME/.local/bin:$PATH"
```

A per-user LaunchAgent starts the service immediately, keeps it alive and restarts it
after a crash. Its `ProcessType` is `Interactive`, so launchd does not throttle the
service's CPU and disk I/O below the user's own processes; commands the service runs
for other members inherit that class. A fleet upgrade installs through the build it
replaces, so a LaunchAgent change reaches a member only with its next install:
`elsecall doctor` reports `launch_agent_current: false` until `elsecall install` runs
there. It runs in one of two **sessions**:

- **desktop** (`gui/<uid>` domain): starts at the user's desktop login and stops at
  logout. New installations use it when the account has a desktop login.
- **background** (`user/<uid>` domain, `LimitLoadToSessionType` `Background`): runs
  without a desktop login and keeps running after the ssh session that installed it
  ends. New installations in an account without a desktop login use it, for example an
  agent account reached only over ssh.

`install` output and `doctor` report the `session`. Updates keep the current session;
`install --session desktop|background` switches it. Asking for the desktop session in
an account with no desktop login is refused with `desktop_unavailable` before the
running service is stopped. Neither session is a system boot daemon. Files live at:

| Purpose | Location |
| --- | --- |
| CLI link | `~/.local/bin/elsecall` |
| LaunchAgent link | `~/Library/LaunchAgents/dev.elsecall.agent.plist` |
| Executable, configuration, receipt, locks, log and socket | `~/Library/Application Support/Elsecall/` |

Installation refuses unrelated files at the CLI and LaunchAgent locations and
unsafe directory ownership or permissions. Failed startup leaves an installation
that can be diagnosed, retried with `install`, or removed with `uninstall`.

## CLI contract

Commands are noninteractive. Each command writes one JSON document to stdout,
including failures. `--help` and `--version` are text; foreground `serve` is a
long-running process and writes lifecycle messages to stderr. **`run` is the one
exception**: its output is the command's (see "Running commands"). Exit codes of
the other commands:

- **0:** success; for `doctor`, all checks pass.
- **1:** operation failed or `doctor` found a problem.
- **2:** invalid command or arguments, including `invalid_request`.

All JSON documents carry `schema_version: 1`. Success uses `data`; failure uses
`error` with stable `code`, explanatory `message`, and actionable `hint`. Consumers
should tolerate new fields, check schema versions, and branch on codes rather than
message text. Example health response (illustrative values):

```json
{"schema_version":1,"data":{"status":"ready","version":"0.1.0","build":"0afc244d79056507","pid":1234,"uptime_seconds":42}}
```

`build` is the running build's ID: the first 16 hex digits of the SHA-256 digest of
its executable, so two machines with the same build ID run byte-identical builds.

| Command | Result |
| --- | --- |
| `elsecall status` | API readiness, service version, PID and process uptime |
| `elsecall machine` | Hostname, OS/version, architecture, logical CPUs and cores by type, 1/5/15-minute load averages, CPU utilization, memory total/used/available in bytes, memory pressure, thermal state, machine uptime in seconds |
| `elsecall tools inventory` | This machine's tool managers, the tools installed through them and which version is the default (see [Fleet tool management](docs/fleet-tools.md)) |
| `elsecall tools inventory --fleet` | Every member's tool inventory: this member first, then the others in roster order. A machine that has not joined a fleet is a fleet of one |
| `elsecall tools inventory --member NAME` | That member's one entry from the fleet inventory |
| `elsecall tools desired [--member NAME]` | Desired tool versions resolved for every fleet member, or one (see [Fleet tool management](docs/fleet-tools.md)) |
| `elsecall tools set TOOL VERSION [--member NAME] [--install-only]` | On the master, set an exact desired version in `fleet-tools.toml`. `TOOL` is `mise`, `xcodes`, `mise:<name>`, or `xcode` |
| `elsecall tools unset TOOL [--version VERSION] [--member NAME]` | On the master, remove a desired version or a whole entry |
| `elsecall doctor` | Installation integrity, launchd registration, API connectivity, relevant paths and recovery guidance |
| `elsecall service restart` | Stop/start the registered service and wait for readiness |
| `elsecall install` | Install/update and wait for readiness |
| `elsecall uninstall` | Stop the service and remove its installation |
| `elsecall run [options] -- COMMAND...` | Run a command once this machine has room, passing its I/O, signals and exit code through (see "Running commands") |
| `elsecall lease acquire [options]` | Wait for capacity, then hold it for the calling script (see "Admission and leases") |
| `elsecall lease release [RUN_ID]` | Release one lease, or every lease of the calling script |
| `elsecall runs [--limit N]` | This machine's capacity, use, queue and load, and the latest run and lease records (default 50, at most 500) |
| `elsecall explain RUN_ID` | One record in full, with the admission decision and its inputs |
| `elsecall trace export [--since D] [--member M] [--output FILE]` | A versioned workload trace with resource-demand and contention inputs ([harness contract](docs/tuning-harness.md)) |
| `elsecall simulate --trace FILE [--set KEY=VALUE ...] [--seed N] [--fidelity] [--day YYYY-MM-DD]` | Offline deterministic fleet simulation through the real scheduler, with sensitivity and recorded-day fidelity results |
| `elsecall replay [--since D] [--set KEY=VALUE ...]` | This member's recorded placement decisions scored again under changed parameters: which flip, and the estimated effect (see "Tuning scheduling") |
| `elsecall report [--since D] [--member M] [--label PATTERN]` | How the fleet did over a period and how accurate estimates were, from every member's records and load history, with findings (see "Fleet reports") |
| `elsecall fleet` | Every member's reachability, role and machine state, fetched over the authenticated tailnet channel |
| `elsecall copies` | The copies of other members' workspaces this machine holds, their object stores and sizes, free space and recent removals (see "Housekeeping of copies") |
| `elsecall copies prune [--dry-run]` | Run a housekeeping pass now, or report what it would remove |
| `elsecall fleet identity` | This member's name, public key, tailnet endpoints and member code |
| `elsecall fleet enroll <code>` / `join <code>` | Enrol another machine (on a member), then join (on that machine) |
| `elsecall fleet remove <name>` / `leave` | Remove a member (on another member), then forget the fleet (on that machine) |
| `elsecall fleet move-master <name>` | Move the master role, updating every member |
| `elsecall fleet check-authority <name>` | Ask a member whether it accepts placement authority from this member |
| `elsecall fleet sync` | Bring every member to this member's roster after a forced change or a conflict |
| `elsecall fleet upgrade [--member NAME] [--wait 5m] [--executable PATH]` | On the master: install its build on every member (or one), each restarting, confirming readiness and rolling back on failure (see "Upgrading the fleet") |

Load averages measure system demand over time, **not CPU utilization percentages**.
`cpu_utilization` is the busy fraction of all logical CPUs (0 to 1) since the
previous sample, which the service takes about once a second; it is null until two
samples exist. `cores` splits logical CPUs into `performance` and `efficiency`
cores; machines without core types report all as performance cores.
`memory_pressure` is macOS's own verdict (`normal`, `warning`, `critical`) and
`thermal_state` its thermal pressure level (`nominal`, `moderate`, `heavy`,
`trapping`, `sleeping`). Memory follows macOS/sysinfo accounting; available memory
includes reclaimable memory, so it need not equal total minus used. OS version and
hostname may be null when the OS does not provide them. Machine data is sampled on
request, with no history. `status` tests API health independently of whether machine inspection
succeeds. Requests time out after three seconds, except `elsecall tools inventory`,
which waits 150 seconds while the service reads this machine's tool managers.
`elsecall tools inventory --fleet` and `--member` wait 170 seconds, longer than
the 150 seconds each other member is given. Startup readiness
has an overall eight-second deadline after launchd accepts the registration.

## Admission and leases

Every run and lease is a **work request** admitted by this machine's service. A
request has a **size** in weighted cores (`--size`) and a **label** (`--label`, free
text such as `test FeatureModule`, used for history and diagnostics). The first label
rule in the working tree's `.elsecall.toml` that matches the label supplies whatever
the flags leave out (see "Consumer configuration" and "Precedence"). Without a
declared size, from `--size` or the rule, a run's size is **learned** from its
label's recent runs, else 1 (see "Learned sizes").
Admission never overcommits: work starts only if its size fits the free capacity,
and keeps that capacity until it ends. While the machine is saturated by load
Elsecall did not start, or under memory pressure, nothing new starts. Outside load
is the CPU utilization averaged over about three seconds minus the share of the
weighted cores that Elsecall's grants hold; saturation means at least 90%.

- **Interactive** requests (the default) start in arrival order. A request that
  does not fit blocks those behind it, so large requests are never starved. Each
  waits at most its **hold limit** (`--hold-limit 90s`, else `ELSECALL_HOLD_LIMIT`,
  else the label rule's); by default ten minutes minus the label's median run time
  over its last 20 completed requests, at least 30 seconds. Then it fails with
  `fleet_busy`, whose `details` hold `retry_after_seconds` and the `running` work,
  and whose hint says what to try.
- **Deferrable** requests (`--deferrable --deadline 30m --at-deadline start|skip`;
  the deadline may also be an RFC 3339 time) start only while no interactive work
  waits. A label rule may make a label deferrable; `--interactive` overrides it. At the deadline, `start` moves the request ahead of interactive work (it
  still waits for room) and `skip` fails it with `deadline_skipped`.
- A request larger than the machine's capacity fails at once with
  `request_too_large`.

A **lease** holds capacity for a section of a script's own work. It belongs to the
**holder**, by default the process that ran `elsecall` (the calling script), and
ends when released or when the holder exits, even by SIGKILL:

```sh
elsecall lease acquire --size 4 --label "test FeatureModule" >/dev/null || exit 1
make -j4 test
elsecall lease release >/dev/null
```

Inside `$(...)` or a subshell, the parent is a short-lived shell: pass
`--holder $$` so the lease belongs to the script. Acquire prints `state` and the
grant (`run_id`, `member`, `size`, `nested_in`, `waited_ms`, `placement_note`); `release RUN_ID`
releases one lease. `state` is `granted`, `nested`, `reused`, `unmanaged` (the
service was unavailable and the caller chose to run anyway, see "Integrating a
consumer") or `off` (`ELSECALL=off`); without a lease, `run_id` and the grant's
other fields are null. Release prints `state` (`released`, `unmanaged` or `off`)
and the `released` IDs.

**Acquiring again.** A holder that already holds an active lease and acquires
again, as a script that re-execs itself for a retry does, gets that same lease
back at once: `state` is `reused`, `waited_ms` is 0, nothing queues and no new
record is made; the record counts it in `reacquired`. The lease keeps its size
(a larger request adds a warning to the record). Acquires are not counted: one
release, by ID or by holder, ends the lease, and so does the holder's exit. So a
section inside the same process (a shell function, or `sh -c 'elsecall lease
acquire'`, which execs straight into `elsecall`) shares the outer lease, and its
release ends the outer lease too. Acquire from a child process instead to get a
separate nested lease. Two acquires that are both still waiting are separate
requests.

**Nested requests.** A request from a process that already runs inside a lease
or run (any descendant of its holder) is admitted at once, without queueing behind
other work, and shares the outer grant's capacity. The outer capacity stays in use
until every nested request has ended too. A nested request larger than its outer
grant still runs within it, and its record carries a warning.

**Records.** Every request leaves a record in `records.sqlite` in the state
directory: the request, every decision with this machine's capacity, use, queue and
load at that moment, waiting and run times, and the outcome. Records never contain
command arguments or environment values; labels are stored as given. Ended records
are deleted after 30 days. Leases and runs whose holders survive a service restart
keep their capacity. When the service stops, queued requests end at once and their
callers receive `service_unavailable`. Hold limits and deadlines follow the wall
clock, so they pass on time after the machine sleeps.

**Learned sizes.** Elsecall measures each run's CPU time, including every process
the command waited for, as the command exits. A run that declares no size (neither
`--size` nor a label default in `.elsecall.toml`) gets the upper quartile of its
label's recent average busy cores (CPU time over run time), rounded up to half a
core. Only completed runs of at least a second from the last 14 days count, at most
the latest 20, and at least 3 are needed; until then the size is 1. A learned size
never exceeds the member's capacity. `explain` shows `size_basis` (`declared`,
`learned` with its sample count, or `default`) and each run's `cpu_ms`.

**Speed.** Each member measures one core's speed about every ten minutes with a short
compile-like workload timed by the measuring thread's own CPU clock, so other
processes' load does not lower it, and reports the best of its last six
measurements as `speed` in `elsecall runs` (about 1 for an M4 Pro performance core).

## Running commands

`elsecall run [options] -- COMMAND [ARGS...]` admits the command as a work request
(the options of "Admission and leases", plus `--pinned`, `--placeable`,
`--member NAME`, `--status-file PATH`, `--run-id-file PATH` and
`--when-unavailable run|fail`) and runs it once it may start. The command behaves as if run
directly:

- It runs as a child of `elsecall` with the caller's stdin, stdout and stderr,
  environment, working directory and process group, so output is byte for byte,
  stdout and stderr stay separate, and `2>&1` keeps their order.
- Elsecall adds `ELSECALL_RUN_ID`, `ELSECALL_MEMBER` and `ELSECALL_ADMISSION`
  (`granted` or `nested`), replacing inherited values, and changes nothing else.
  Unmanaged and `ELSECALL=off` runs add only `ELSECALL_ADMISSION` (see "Wrapping a
  whole script").
- Interrupt, termination, hangup, quit and user signals sent to `elsecall` are
  forwarded to the command. `elsecall` exits with the command's exit code, or dies
  of the same signal the command died of.
- Elsecall prints nothing on a successful pass-through, whatever the command's exit
  code. The one exception is the `unmanaged` notice line written before a command
  that runs without admission because the service is unavailable.

**Elsecall's own failures** (invalid options, a command that cannot be found,
`service_unavailable`, `fleet_busy`, `request_too_large`, `deadline_skipped`,
`member_unavailable`, `config_sync_failed`) exit **125** and write two lines to stderr. The first is the
**failure line** (see "Failure lines"), which says what happened and exactly what
to do; the last is the error envelope
`{"schema_version":1,"error":{"code","message","hint","run_id",...}}`, with `run_id`
null when no record exists. The command did not run, except when waiting for it
failed and its outcome is unknown. Since a command may exit 125 itself,
`--status-file PATH` writes the outcome as JSON: `{"data":{"state":"exited",
"exit_code":125,...}}` for the command's own exit, `{"error":{...}}` for Elsecall's,
including for invalid options. The file reads `waiting`, then `running`, while the
run is in progress. Its `data` carries `run_id`, `member` and `admission`:
`granted`, `nested`, `unmanaged` or `off` (the last two with `run_id` and `member`
null). If the file cannot be written after the command ran, Elsecall
still exits with the command's status and writes one
`{"schema_version":1,"warning":{...}}` line to stderr. Interactive runs stop
waiting with `service_timeout` if the service has not decided 30 seconds after the
hold limit.

Runs default to pinned, unless the label rule sets `placement = "placeable"`;
`--pinned` overrides the rule. `--placeable` (or such a rule) lets the master choose
the member (see "Placement"); `--member NAME` requires one (see "Running on another
member"). Deferrable runs are pinned only.

If `elsecall` itself is killed with SIGKILL while the command runs, the command
keeps its capacity until it exits.

## Placement

`elsecall run --placeable [options] -- COMMAND` on the master runs the command on the
member with the **earliest expected completion**, in that member's copy of your
working tree as described under "Running on another member". On any other member,
outside a git working tree, or in a fleet of one, it runs on the calling member and
its record says why. `--member NAME` always overrides the choice.

A candidate's expected completion is **wait + sync + run**:

- **Wait:** until enough of its capacity is free, from its running work's expected
  remaining time and the requests queued ahead. Each member's reserve is already
  outside its capacity. A member saturated by outside load or under memory pressure,
  or whose copy of this workspace another run is using, is never chosen. Running
  work (runs and leases, here or placed) ends at the run time its member expects
  (its label's median); else at what history expects of its whole run on that
  member, by the same chain as **Run** below (its label, converted, its family);
  else, with no estimate at all, it is assumed to run 60 s more, or half the time
  it has already run once that is longer: the longer work has run, the longer it
  is likely to keep running. Work that has run past its estimate is treated as
  work without one. Queued work without an expected time counts 60 s.
- **Sync:** none on the calling member. Elsewhere it is learned from earlier syncs of
  this workspace to that member, or of first syncs of the same repository, or a prior:
  3 s for a warm copy, 30 s for a first sync where the repository's history is already
  there, 120 s otherwise.
- **Run:** from the first of these with enough history (three runs by default,
  warm and cold apart), else 60 s of work at speed 1 divided by the member's speed:
  1. **label**: the label's own runs on that member;
  2. **converted**: the label's runs on every member, converted to this one;
  3. **plan**: for a request that declares a sequential plan of `n` units
     (`--workload`), a line of run time against units, fitted through the
     latest runs of its label and family whose wrappers reported the units they
     covered (`elsecall workload observe`), at `n`: a fixed overhead plus time
     per unit, each the median over the runs, so that a 39-module batch is not
     estimated from single-module runs. It needs three such runs in at least
     two sizes. Only the label's runs of the same plan size count as its own;
  4. **family**: for a label with too few runs anywhere, the runs of other labels
     that matched the same `.elsecall.toml` rule (its family, such as every
     `acme test *`) on that member, else on every member, converted. Runs under
     a second are left out of a family: a no-op says nothing about its family's
     work. The label's few runs count for themselves, and the family's median
     stands in for the ones it lacks: with one of three, the estimate is the
     geometric mean of that run and the family's median twice. A label no rule
     matched has no family.

  Runs convert between members by the ratio of their run times on labels both
  have run three times or more (each median at least a second; labels of the
  same family first; learned again each minute), else by their measured speeds. A
  cold copy runs a cold build: learned from cold runs, else three times the warm
  estimate. A cold estimate from further from the label than the warm one (a
  family's cold runs against the label's own warm runs) is not used. When outside
  load and running work leave fewer idle cores than the size, the run is assumed
  proportionally slower.
- **Losses:** a placed run whose connection is lost is stopped on its member and
  nothing it did comes back, so it has to run again from the start. On another
  member, sync and run are therefore weighed by how often that member lost runs
  placed on it: with `λ` lost runs per hour of placed work there over the last
  24 hours, counted over at least an hour of it, work of `t` hours takes
  `(e^(λt) − 1) / λ` on average. At one loss an hour, two minutes of work takes 2%
  longer and an hour of it 72%, so a member that keeps losing runs still takes
  short work but long work stays where it cannot be lost. `explain` shows the
  factor and the losses it rests on; the calling member loses nothing, and runs
  the calling member's own service restart ended do not count.

A candidate is eligible when it can start (wait + sync) within what is left of the
hold limit. The calling member keeps the work unless another member completes earlier
by at least 10% (at least 2 s). These numbers, the priors and the gate thresholds are
the defaults of the master's `config.toml` (see "Service configuration"). A new workspace therefore stays on the caller unless
the caller cannot start it in time or is clearly worse off. Every second, the master
asks each member for a fresh quote and scores waiting requests again, oldest first;
a request moves when its place can no longer start it in time, or when another member
can start it now and completes earlier by the same margin. So a request waiting
behind long-running work without an estimate moves to an idle member as soon as
its assumed wait grows past that member's completion by the margin, well before
its hold limit. A new request waits at
most 750 ms for members' answers and uses a slower member's previous answer; a
member that cannot be asked is left out and named in the decision.

Before the run starts on another member, the master brings that member's synced
configuration up to date (see "Running on another member"). If that fails, placement
leaves the member out of this request and places it again at once, within the same
hold limit: on another member, or on the calling member, which needs no sync. A
placeable run fails with `config_sync_failed` only when no member it could ever run
on is left, such as a size only that member can hold, or when the hold limit passes
while the sync is still running. The record's `fallbacks` lists each
member left out (`failed_sync`: `member`, `duration_ms`, `cause`) with
`decided_at`, where the request went instead (`moved_to`) and why (`reason`). The
decision's `reason` ends with the members left out, and `fleet_busy` names them
too.

When no member can start it within its hold limit, the run fails with `fleet_busy`:
the message says why each member could not, `details.members` lists them,
`details.running` lists what runs where, and `details.retry_after_seconds` is the
earliest any member is estimated to be able to start it. A size larger than every
reachable member's capacity is `request_too_large` at once.

`explain` shows every candidate with its state at decision time and an `estimate`:
its speed (and whether it was measured), the copy (`caller`, `warm`,
`cold_with_history`, `cold`), outside utilization, when it could start, and the
wait, sync and run seconds with where each came from (`run_from` names the source,
such as `family_runs` with its `samples` and the label's own `label_samples`, or
`fleet_runs` with how many runs it converted by learned ratios and by measured
speed, and its `level`: `label`, `converted` (the label's runs elsewhere), `family`
(on the member or converted) or `prior`), then the expected completion
and whether it was eligible. The decision's `reason` says why the winner won, or why
none could take the work. Each candidate's `inputs` hold the rest of what it was
scored with (speed, utilization, the work holding and queued ahead of its capacity,
its copy, what history said as `learned`, and the thresholds of its own gate as
`admission`). Each piece of running work in `holding` has its `size`, the
`remaining_ms` its member expected, how long it had run (`elapsed_ms`) and, when its
member expected nothing, `learned_run`: the whole run history expected, as `ms`
with its source (`from`, `samples`, …), and `cold_from_warm` when a cold copy took
its warm estimate times `cold_run_factor`. A wait behind work that is not its member's expectation says so
in the candidate's `why`: "wait 322 s behind running work: 9 cores 644 s in, 322 s
more (no estimate)". `scoring` holds the size, the hold
limit left and the scoring's own winner, `parameters` the values used, and
`arrival` what the first scoring expected of each candidate, so a decision can be
re-scored exactly. `arrival` also holds that scoring's size and hold limit left,
and each candidate's `state` and `inputs`, so `elsecall replay` can re-score the
scoring that decided where the request went first. `elsecall runs` lists each run's `placement`,
`calling_member`, `member`, `placed` (side, members, warmth, sync time) and `reason`.

## Fleet reports

`elsecall report [--since 7d] [--member NAME] [--label PATTERN] [--bucket DURATION]`
answers, as one JSON document, how the fleet did over a period and how accurate
scheduling's estimates were, with a short list of the most notable problems. Run
it on any member. It reads this member's requests and load history and asks every
other member for theirs over the authenticated member channel, all at once. A
member that cannot answer is named with `records_error` or `load_error` and an
`incomplete` finding; the rest of the report stands. One that cannot say which
outside processes held its gate closed is named with `outside_error` only: the
report then names no processes for it.

- `--since`: a duration back from now (`7d`, `12h`) or an RFC 3339 time, at most 30
  days ago, as long as records and load history are kept. Default `7d`.
- `--member NAME`: only requests the member made or ran, and only its load. An
  unknown name is `invalid_request`, which lists the members.
- `--label PATTERN`: only requests whose label matches the glob (`*`, `?`), as label
  rules match. Load is not per label, so every member's load stays.
- `--bucket DURATION`: the width of each `series` point, from `1m`. Default a 24th of
  the period. Points are whole fine buckets (below).

The report uses `definitions: 7`, which adds members' `outside_warnings` and their
findings. The number changes when a metric definition changes. Version 2 judged
`speed_conversion` against learned ratios and added `run.by_level` and `backtest`.
Version 3 added `leftovers` and `heavy_leftovers`. Version 4 excludes synthetic
rehearsal requests from request, leftover and accuracy metrics while retaining
actual machine load. Version 5 added measured run phases. Version 6 added members'
`blocked`, `blocked_queues` and `outside_error` and their findings.

- **Which requests.** Those made in the period. Each counts once, on the member it
  was made on; that member reports where it ran, so the member side of a placed
  run is not counted again. Requests recorded under a machine's host name before
  it joined the fleet count under its fleet name. **Nested** requests ran inside
  another grant without admission; they count only in `requests`.
- **Times.** A wait runs from request to start; for a placed run, to the member
  admitting it, before the sync. The existing work and accuracy metrics use
  grant-held time less recorded workspace sync. Measured execution is available
  in `run_phases`. Percentiles use nearest rank and means are arithmetic.
- **Fine buckets.** Members' load is compared in buckets of whole minutes,
  aligned to the epoch on every member, as narrow as 1,000 of them over the period
  allow: 1 minute for an hour, 2 for a day, 11 for a week (`period.fine_seconds`).

`period`: `since`, `until`, `fine_seconds`, `series_seconds`. `filters`: `member`,
`label`.

`fleet`, over the requests the filters keep (`this_member`, `master`):
- `requests`: `total`, `runs`, `leases`, `nested`, `started`, `unfinished`, and
  `failed` by failure code (`cancelled` included), before or after starting; past 20
  codes, the least frequent are summed as `other`.
- `fleet_busy`: `count`, `of` (interactive requests that were decided, nested ones
  aside) and `rate`.
- `placement`: `pinned` and `required` requests, and of placeable ones that
  started, `placeable.started`, `on_calling_member`, `elsewhere`, `by_member` and
  `run_prior`, those whose run estimate where they ran was the prior.
- `waits`: `interactive` and `deferrable` (which waits for idle capacity by design),
  each with `count`, `mean_seconds`, `p50_seconds`, `p90_seconds` and `max_seconds`.
- `run_phases` separates `local` and `remote` runs. Each distribution counts
  only runs with a measurement for that phase. `remote_failures` counts
  attributed failures and output-return warnings by phase.
- `balance`, always over every member, compared fine bucket by fine bucket where
  at least two members sampled (`periods`): `cpu_spread_mean`, the mean gap
  between the busiest and the idlest member's CPU; `imbalanced_share`, the share of
  periods with one member at 80% CPU or more and another at 30% or less;
  `fastest_member`, by mean measured speed; and
  `fastest_idle_while_queued_seconds`, sampled time in which the fastest member
  held no Elsecall work and its gate was open while another member had
  interactive requests queued (deferrable work such as housekeeping waits by
  design; history from before this version counts every queued request).
- `fleet_error`: why this member could not list the fleet's members, when it is
  enrolled; the report then covers this member alone.

`members`, each with `master`, `this_member` and `records`: `requests` read,
`truncated` past 50,000, and `dropped`, requests another member sent that named a
machine outside the fleet as where they ran:
- `load`: `sampled_seconds`; `cpu_mean` and `cpu_p95` (of the fine buckets);
  `outside_mean`; `elsecall_share`, the share of weighted cores Elsecall's grants
  held; `queued_max`; `busy_seconds` with Elsecall work; `saturated_seconds` and
  `saturated_share`, of the gate closed by outside load; **work while saturated**,
  `saturated_busy_seconds` and its share of `busy_seconds`,
  `saturated_busy_share`; `memory_pressure_seconds`; `worst_memory_pressure` and
  `worst_thermal_state`. Means weigh each sampled second alike.
- `speed`: the measured speed in the first and last fine buckets (`first`,
  `last`, each the midpoint of its bucket's range), its `min` and `max`, and
  `drift`, (max − min) / max.
- `blocked`: the longest stretch of consecutive fine buckets in which outside
  pressure closed the member's gate for 90% or more of the sampled seconds, with
  at least half of each bucket's time in the period sampled, so the member
  admitted nothing: `since` and `until` (within the period), `seconds`, `cause`
  (`memory_pressure` or `cpu_saturation`, whichever closed the gate longer),
  `memory_pressure_seconds`, `saturated_seconds`, `outside_mean` and
  `worst_memory_pressure`. The gate's CPU saturation is outside load by
  definition; memory pressure counts as outside only in buckets where Elsecall
  work held cores for 10% or less of the sampled seconds, since Elsecall's own
  work may have caused it. A bucket missing from the history ends a stretch.
  `outside` lists the outside processes the member named for the blocked episode
  of the same cause that overlaps the stretch the longest (see "Outside
  pressure"), leading first, as last sampled: `name`, `pid`, `owner`,
  `resident_bytes`, `footprint_bytes` where readable, `cpu_cores` and
  `uptime_seconds`; empty when it named none.
- `blocked_queues`: per label, longest first, requests that waited for admission
  on this member (pinned there, or required there and started; placeable requests
  wait for any member) for an hour or more within the period, while outside
  pressure, as for `blocked`, closed its gate for 90% or more of the sampled time
  they waited, with at least half of that time sampled (each bucket weighted by
  its overlap): `label`, `requests`, `longest_wait_seconds`, and of the longest,
  `requested_at`, how it `ended` (`{"state": "started"}`, `{"state": "queued"}`
  when it still waited at the end of the period, or `{"state": "failed", "code":
  …}` with the failure code or `cancelled`), the `cause`, and `outside`, as for
  `blocked`, over that wait. Elsecall's own internal work and nested requests are
  left out.
- `outside_error`: why the member could not say which outside processes held its
  gate closed or warn about any, such as a member on an earlier version.
- `outside_warnings`: the early warnings about outside processes (see "Fleet",
  "Early warnings") that held at some time in the period, the longest-held first,
  each as `elsecall fleet` shows it, with `until`, when it last held (absent when it
  still held at the period's end), and `hours`, how long it had held from `since` to
  then. At most 200 per member.
- `work`: `runs` and `leases` that ran there, `core_seconds` (size × run time of
  completed work) and its `share` of every member's, of the requests the label
  filter keeps.
- `waits`: as the fleet's, of requests that started there.
- `run_phases`: as the fleet's, of runs admitted there.
- `series`: per point, `at`, `sampled_seconds`, `cpu_mean`, `outside_mean`,
  `elsecall_share`, `queued_max`, `busy_seconds`, `saturated_seconds`,
  `speed_min` and `speed_max`.

### Run phase boundaries

`elsecall explain RUN_ID` shows admission wait in `waited_ms` and a completed
workspace sync in `placed.sync.duration_ms` (`placed.sync_ms` in `elsecall runs`).
`run_phases` adds these caller observations:

- `execution_ms` spans local launch to the observed exit. For a remote run it
  spans sending the start request to receiving the valid exit and draining
  stdout and stderr. Data the command streams on stdout is part of execution.
- `result_return_ms` spans the end of remote execution to completing the
  declared-output exchange and applying the returned files. Runs without
  declared outputs and incomplete returns have no sample for this phase.
- `request_to_result_ms` measures the caller's elapsed time from asking for
  admission to the result. It includes work between the measured phases, such
  as attaching to the member. The report uses the measured elapsed time
  directly. A run without declared outputs reaches its result at the observed
  exit and stream drain. An incomplete return of declared outputs leaves the
  total unknown.
- `remote_failure_phase` uses the last confirmed boundary. Before confirmation
  of workspace sync, failures are attributed to `workspace_sync`, including
  attachment failures. After sync and before the caller confirms the exit and
  stream drain, they are attributed to `execution`. Output-return problems are
  attributed to `result_return` while the command's exit and warnings stand.

Local runs have no workspace-sync or result-return sample. Older records
contribute the admission wait and workspace sync measurements they have.
The existing `run_ms` in run records still measures how long the grant was held.

`accuracy` compares what was predicted for the member a request ran on with what
happened there. `wait`, `sync` and `run` each have `by_member` (every label) and
`by_label` (label and member) groups, sync groups also by the copy's `warmth`, and
`run` also `by_level`: placement's run estimates per member and the `level` of
history they came from (`label`, `converted`, `family`, `prior`).
`by_label` lists the 200 groups with the most samples; `by_label_omitted` counts
the rest.
Each group has its `estimator`, `samples`, `priors` (predictions that were the
prior), `predicted_median_seconds`, `actual_median_seconds`, `ratio_median`
(median of actual ÷ predicted over predictions above zero; above 1, estimates were
too low), `bias_seconds` (mean of actual − predicted) and
`mean_absolute_error_seconds`.
- **Estimators.** `placement` for placeable requests: the wait expected when the
  request arrived (the recorded decision's own wait, just before the start, is
  about zero), and the sync and run with their cold factor and slowdown. Records
  from before arrival estimates have no wait. `label_median` for other runs: the
  label's median run time behind the default hold limit.
- `run` counts completed runs; `wait` started requests; `sync` placed runs.
- `speed_conversion`: for each label with at least 3 completed runs on each of
  two members, other than runs into cold copies: `samples` and `median_seconds` on
  the faster and the slower member by measured speed, `observed_ratio` (median run
  on the slower ÷ on the faster), `measured_ratio` (their mean measured speeds,
  faster ÷ slower), `learned` (the ratio the master's runs of other labels teach,
  as placement learns it, with its `labels` and `scope`, `family` or `fleet`;
  absent when they teach none), `converted_by` and `converted_ratio` (the learned
  ratio, else the measured: what placement would convert this label's runs by had
  it not run on one of them), `error` (observed ÷ converted) and `measured_error`
  (observed ÷ measured). An error far from 1 misleads placement.
- `backtest`: today's run estimate for each completed run of the period (the
  latest 500; `limited` past them), made again from the runs requested before it
  on the master (`history_member`), whose records placement learns from, within
  the period and `[estimates]` `recency`, at most the latest 2,000 (so early runs
  of a period have less history than placement had): label, converted or family
  history, else the run prior at the member's mean measured speed, cold runs as
  placement estimates them, without contention. `baseline` is the same estimate
  from the label's own runs alone, converted by measured speed: placement before
  families and learned ratios. `all` and `by_level` (by the level today's estimate
  came from) each have `samples` and, for `estimate` and `baseline`, `priors`,
  `ratio_median` (actual ÷ predicted), `error_factor_median` (how many times off,
  either way; 1 is exact) and `mean_absolute_error_seconds`. The parameters are the
  reporting member's; on the master, placement's.

`leftovers` sums the processes placed commands left running after they exited
(see "Running on another member"), as the members that ran them report them, of
placed runs requested in the period whose leftovers have ended, the label and
member filters applied (`--member` keeps those it made or ran): `runs` that left
any, `capped`, those the cap stopped, and `cpu_seconds` they used after the
exits; and `by_label`, heaviest first, each with `label`, `runs`, `capped`,
`cpu_seconds` and `max_seconds`, the longest any of them ran. That work was not
admitted, so it loaded members outside admission's view.

`findings` lists at most 10 problems, most severe first, each with `kind`,
`severity` (`high`, `medium`, `low`), a `message`, the `member` or `label` it is
about, and the `metric` behind it with its `value` and `threshold`:

| Kind | Severity | When |
| --- | --- | --- |
| `incomplete` | high | A member's requests or load could not be read, more than 50,000 requests were made there, some it sent named a machine outside the fleet, or the fleet's members could not be listed. Whatever `--member` says. |
| `master_saturated` | high | The master was saturated by outside load 10% or more of the time. |
| `master_memory_pressure` | high | The master was under memory pressure at all. |
| `fastest_idle_while_queued` | high | The fastest member idled 300 s or more while others queued. |
| `outside_pressure_blocked` | medium, high from 4 h | A member's `blocked` stretch lasted an hour or more. It names the leading outside process and what to do about it, from "Outside pressure". |
| `queued_on_closed_gate` | medium, high from 4 h | A member has `blocked_queues`: one finding per member, about the label that waited longest, counting the others. |
| `fleet_busy` | medium, high from 10% | 5% or more of decided interactive requests failed with `fleet_busy`. |
| `wait_estimate`, `sync_estimate`, `run_estimate` | medium, high from 2× | A group of at least 3 is off by 1.5× or more (median ratio; any amount when every prediction was zero) and its medians differ by 2 s or more. Wait and sync per member, run per label. |
| `work_while_saturated` | medium | 10% or more of a member's Elsecall work time ran while outside load saturated it. |
| `imbalance` | medium | 20% or more of periods were imbalanced. |
| `outside_process_spinning` | medium, high from 24 h | An outside process spun on a member (`members.outside_warnings`, `hours` against the 6 h it takes to warn). The message names it, its owner and who can restart it. |
| `outside_process_growing` | medium, high from 24 h or when oversized | An outside process's memory grew steadily for 6 h, or it held a quarter of the member's memory for an hour (threshold 1 h). |
| `heavy_leftovers` | medium, high when both | A label's leftovers used 300 s of CPU time or more after their commands exited, or the cap stopped at least 2 runs' leftovers and 10% or more of them. |
| `speed_drift` | low | A member's measured speed drifted 15% or more. |
| `speed_conversion` | low | The ratio placement would convert a label's run times between two members by (learned, else measured) is off by 1.2× or more, and that misses the slower member's median by 2 s or more. |

A report reads at most 50,000 requests per member. Members answer reports only to
enrolled members, as they answer status; answers carry labels, member names,
times, outcomes, estimates, leftovers' times and the names and usage of the
outside processes that held a member's gate closed or that it warned about,
never paths, the processes of requests, arguments or environment values. Another
member's answer is checked like any untrusted input: run IDs, failure codes and
names must be well formed, only this member's own requests may name a machine
outside the fleet, and a member reports only leftovers it ran itself. A member
asked about a period that ends after its own clock's now answers up to its now.
Members on earlier versions answer neither report route. A member answers `POST
/peer/v1/report/outside` with at most the 200 longest blocked episodes of the
period, each naming at most 5 processes by names of at most 64 characters, and
the 200 latest early warnings; the member that reports holds another member's
answer to the same bounds, and keeps only warnings with such names, finite
measures and times in order that end by the period's end, give or take a
minute of clock difference.

## Running on another member

`elsecall run --member NAME [options] -- COMMAND` runs a command on another member,
in that member's **copy** of the git working tree containing your working directory,
starting at the same relative directory. Callers keep their pipes and redirects:
stdin, separate stdout and stderr (in order when both go to one place), the exit
code, forwarded signals, the 125 envelope and the status file all behave as for a
local run.

- **Who places.** Only the master places work. On any other member, and for a
  request made outside a git working tree or from inside a local lease or run, the
  command runs on the calling member and its record says why. An unknown member is
  `member_unavailable`.
- **Waiting.** The request waits on the master, which offers it to the member until
  the member admits it or the hold limit passes. A member admits placed work at once
  or refuses it: without room, while its own requests wait, or while another run
  uses the same copy. The hold limit covers everything up to the start, workspace
  sync included; when it passes, the run fails with `fleet_busy` and the member's
  busy report. A size the member can never admit is `request_too_large`.
- **The copy.** Every run first makes the copy match your working tree: the same
  HEAD, branch and upstream, the same staged state, your modified, deleted and
  untracked files, and commits that exist only here. No commit or push is needed.
  Ignored files are never copied, and the copy's ignored build state is never
  touched, so builds stay incremental. History moves as git packs, once per
  repository; file content moves by digest, and content the member already has
  (renamed files, committed files) never moves. A warm, unchanged tree sends a few
  bytes. What a run changes in the copy is put back by the next run; nothing comes
  back except declared outputs. Copies are owner-only (mode 0700). Submodules are
  not copied: their directories are empty in the copy.
- **Refs in the copy.** Besides HEAD and its branch, the copy carries, under your
  names and at your commits as of the sync:
  - the upstream of HEAD, such as `origin/feature`;
  - each remote's default branch: `origin/HEAD` and the branch it points to, such
    as `origin/main`, as `git remote set-head` or a clone set them;
  - refs `.elsecall.toml` declares under `[workspace] refs`, such as release
    branches or tags.

  So `git merge-base HEAD origin/main`, `rev-parse origin/HEAD`,
  `rev-list --count HEAD..origin/main` and `log` give the same answers as here, and
  they follow your tracking refs on every run. Other remote branches, tags and
  stashes are not carried: each carried ref brings its history once. A remote's
  URL never reaches the copy, so `git fetch` and `git push` there fail; fetch here
  and run again. A remote without `refs/remotes/<remote>/HEAD` here has none in the
  copy either.
- **Where copies live.** Each member's `[workspace] location` decides: `mapped`
  (default) puts copies under the member's workspace root, at a place derived from
  the calling member and the caller's path; `same-path` puts them at the caller's
  own path, for members whose account can own it. A same-path location that holds
  anything but this workspace's copy is refused, untouched.
- **Environment.** The command gets the variables `.elsecall.toml` forwards, the
  member's own identity variables (`HOME`, `USER`, `LOGNAME`, `PATH`, `TMPDIR`,
  `SHELL`, `PWD` and the like, even when forwarded), and `ELSECALL_RUN_ID`,
  `ELSECALL_MEMBER`, `ELSECALL_ADMISSION` (`granted`), `ELSECALL_CALLER_MEMBER`,
  `ELSECALL_CALLER_WORKSPACE` and `ELSECALL_WORKSPACE_COPY`. Callers' `ELSECALL_*`
  variables are never forwarded. Absolute paths it prints name the copy; use the last two
  to translate. Values are never logged or stored.
- **Outputs.** Paths declared in `.elsecall.toml` come back after any exit code, and
  only if the run changed them: each changed output replaces yours whole, a removed
  one is removed. Links are neither followed nor returned on either side.
  - What comes back is a snapshot taken the moment the command exits (APFS clones,
    or copies), so a background process the command left behind cannot delete or
    change an output before it is sent.
  - Return is best effort: a path that vanished while the snapshot was taken, is
    unreadable, lies behind a link, is too large or fails in transfer is left out,
    and everything else still comes back. A directory still replaces yours whole,
    without that path; a single file leaves yours as it was.
  - Each path left out is named with its cause as a warning in the status file and
    in both members' records (`elsecall explain <run_id>`), never in the command's
    output. The exit code is the command's.
- **Processes left running.** Processes the command leaves in its process group
  when it exits, such as a detached telemetry upload after tests, keep running on
  the member, as they would after a local run:
  - You get the exit code and outputs at once, as before; the run's capacity is
    released when it ends. Their output, if they keep the command's stdout or
    stderr open, is read and dropped once the command's own output has drained
    (one quiet second), so they never fail writing it.
  - They are not admitted work: the member's load sampling sees them as outside
    load, which holds back new work while it saturates the member.
  - They are stopped (SIGTERM, then SIGKILL 5 s later) at the cap, 10 minutes
    after the exit by default (`[placed_runs] leftovers_cap`), whether or not you
    are still connected.
  - The next run of the same copy waits up to 10 s for them to end, then stops
    them, before it syncs: a run always starts in a copy that matches yours, with
    nothing still writing in it. Housekeeping leaves the copy alone while they run.
  - The member's record keeps them as `placed.leftovers`: `state` (`running`, then
    `ended`), `since` (the exit), `cap_ms`, `duration_ms` (exit to the last of them
    ending), `cpu_ms` (CPU time they used after the exit, sampled every half
    second) and `ending.by`: `finished`, `capped`, `next_run` (with its `run_id`) or
    `service_restarted`. The next run's record has `placed.leftovers_wait_ms`.
    `elsecall report` names labels that leave heavy work (`heavy_leftovers`).
  - A service that stops does not stop them; its next start does, as it stops
    placed commands. A process that starts its own session or process group
    escapes all of this, as it would locally.
- **Lost connection.** Both sides send a heartbeat every second; ten seconds of
  silence, or the connection closing, fails the run with `connection_lost`, and the
  member stops the command and everything in its process group at once. A member
  whose service restarts stops the commands it was running for others, and the
  processes earlier commands left running. Nothing comes back after a lost
  connection. A connection lost after the command exited keeps its exit code, with
  a warning that outputs were not returned; what the command left running keeps
  running up to the cap. Ctrl-Z stops only the caller, which then goes silent.
- **Synced configuration first.** Before the run starts there, the master brings the
  member up to date with every synced path (see "Configuration sync"): it checks the
  generation the member holds and pushes only if it differs, so a current member
  costs one request. The time counts toward the hold limit. If the sync fails on a
  member you required with `--member`, the run fails with `config_sync_failed` and
  the command does not run; placeable work is placed again without that member (see
  "Placement"). A member that cannot be reached is `member_unavailable`, as
  before. Runs on the calling member sync nothing. A slow sync to one member never
  holds up syncs to, or runs placed on, another.
- **Consistency warnings.** Toolchains and configuration locations that
  `.elsecall.toml` declares are fingerprinted on both members under a salt chosen for
  the run; the member returns its fingerprints and the caller compares. A mismatch is
  a warning, the run proceeds. Warnings appear in the status file (`data.warnings`),
  in both members' records and in `elsecall fleet` (`latest_placed_run`), never in the
  command's output. A `~/` location that is a synced path or lies inside one is not
  compared: the member was just brought up to date with it, so it is current by
  construction, and any difference is the member's own override. A location reached
  through a link, a special file or a `.git` directory inside the synced path,
  which sync does not carry, or one that contains a synced path, is still compared.
- **Records.** Both members record the run under the same run ID; `explain` shows
  `placed` with how far it got, the sync's time and size, and the copy.
  `placed.sync.refs` lists the refs the copy carries; on the caller,
  `refs_only_commits` counts the commits sent only because a carried ref needs
  them, when there were any. On the master, `config_sync` shows the configuration
  sync before the run: `member`, `duration_ms`, `outcome` (`current`, `pushed`,
  `nothing_synced` or `failed` with its `cause`), the `generation` and synced
  `paths`, and for a push, per path, the number of `files` it changed and their
  `bytes`; never values. `runs` lists its time as `placed.config_sync_ms`.
  `fallbacks` lists syncs that failed on members placement chose before (see
  "Placement").

Signals go to the command's process group on the member, as a terminal's Ctrl-C
does; the command sees pipes, not a terminal.

## Consumer configuration

A repository may hold `.elsecall.toml` at the root of its working tree. It is the only
place consumer-specific knowledge enters Elsecall, and it is validated as untrusted
input: unknown keys and invalid values make every run and lease there fail with
`invalid_request`, whose message names the line or the rule and whose hint says
what to change. Without it, defaults apply. The file is found from the caller's
working directory: the nearest directory upwards with a `.git` entry.

```toml
[outputs]
# Produced paths a placed run brings back; relative, inside the working tree.
paths = ["build/reports", "build/logs/test.log"]

[workspace]
# Refs a placed run's copy carries besides the upstream and remotes' default
# branches: full names under refs/remotes/ or refs/tags/; * matches within one
# name component, and a name without * also matches every ref under it.
refs = ["refs/remotes/origin/release/*", "refs/tags/v*"]

[environment]
# Variables placed work receives from the caller, by name and by prefix.
names = ["CI", "BUILD_NUMBER"]
prefixes = ["APP_"]

[consistency]
# Configuration files or directories compared across members: absolute or ~/.
configuration = ["~/.gradle/gradle.properties"]
[consistency.toolchains]
# Commands whose output identifies a toolchain, compared across members.
xcode = ["xcodebuild", "-version"]

[admission]
# When the local service cannot be reached: "fail" (default) or "run" unmanaged.
when_unavailable = "run"

# Label rules, in order: the first whose match fits the request's --label applies.
[[labels]]
match = "test *"          # glob: * any text, ? one character
size = 4                  # weighted cores
hold_limit = "5m"         # interactive (implied by hold_limit, or urgency = "interactive")
placement = "placeable"   # or "pinned" (the default)
[[labels]]
match = "warm *"
size = 4
urgency = "deferrable"    # needs deadline and at_deadline
deadline = "30m"          # a duration after the request, at most 7 days
at_deadline = "start"     # or "skip"
```

**Label rules.** A rule may set `size`, `urgency`, `hold_limit` (interactive only),
`deadline` and `at_deadline` (deferrable only, both required) and `placement`.
A deferrable rule cannot be placeable. Rules apply to `run` and `lease acquire`;
a lease is always pinned, so `placement` does not apply to it. A request without
`--label` matches no rule. `elsecall explain RUN_ID` shows the applied rule and
where each property came from, as `settings`:

```json
"settings":{"rule":{"number":1,"match":"test *"},"size":"rule","urgency":"rule","hold_limit":"environment","placement":"cli"}
```

Each source is `cli`, `environment`, `rule` or `default`. At most 64 rules;
`pattern` is accepted as the earlier name of `match`.

**Workspace refs.** At most 32 patterns, matched as `git for-each-ref` matches
them. A copy carries at most 256 refs, defaults included; a run whose patterns
match more fails with `invalid_request` before it starts, saying to narrow them.
Declare only refs your scripts compare against: each needs its history on the
member, sent once.

A rule also names a **family**: the labels it matches, by its `match` pattern.
Placement estimates a label with too few runs of its own from its family's (see
"Placement"), so one rule per kind of work, such as `acme test *` and
`acme build *`, gives every new label a sensible estimate from its first
run. Families are keyed by the pattern, as labels are by their text, so the
same pattern in two repositories is one family.

Keep secret files ignored by git: ignored files are never copied. Do not forward
secrets you do not want on the member, and do not pass machine-local identifiers
such as simulator IDs to placed work unless you mean to bind it to that member.

## Integrating a consumer

An existing repository adopts Elsecall with one config file, a two-line guard, and
one line per script or one prefix per heavy command. A complete minimal
integration:

```toml
# .elsecall.toml at the repository root
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
  waits, but never to `--member` work, which is bound to that member.
- **Formatting failures.** On exit 125 the stderr line before the envelope already
  tells the agent what to do (see "Failure lines"); pass it on unchanged.
- **Re-exec retries.** A script that re-execs itself and acquires again gets its
  own lease back instead of queueing (see "Acquiring again").
- **Sizes and deadlines per operation.** They live in the label rules.
- **Private re-exec guards.** Every command `run` starts, in every mode, sees
  `ELSECALL_ADMISSION` (see "Wrapping a whole script").

### Wrapping a whole script

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

### Failure lines

Elsecall's own failures of `run` and `lease acquire` write one plain line on stderr
before the JSON envelope: for `run`, the envelope is the last stderr line after it;
for `lease acquire`, the envelope is the JSON document on stdout. The line has the
form `elsecall: <code>: <what happened> <what to do>`, contains no control
characters, and its `<code>` is the envelope's. For `fleet_busy`:

```text
elsecall: fleet_busy: No capacity within the 90 s hold limit: 0 of 12 cores are free and 2 other requests wait. Wait about 40 s, then rerun the same command once. `elsecall runs` shows what is running and queued; `elsecall explain r-3f9a0c41d2b7` shows why.
```

The seconds are the envelope's `details.retry_after_seconds`. Other codes end with
their fix, such as which rule of `.elsecall.toml` to change for `invalid_request`,
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

### Environment controls

| Variable | Values | Effect |
| --- | --- | --- |
| `ELSECALL` | `off`, `on` | `off`: `run` executes the command directly with exact pass-through (no notice; only `ELSECALL_ADMISSION=off` added to its environment) and `lease acquire`/`release` succeed with `state` `off`. Options and `.elsecall.toml` are still checked. |
| `ELSECALL_HOLD_LIMIT` | a duration such as `90s` | The hold limit of interactive requests without `--hold-limit`. Deferrable requests ignore it. |
| `ELSECALL_WHEN_UNAVAILABLE` | `run`, `fail` | What to do when the service cannot be reached. |

An empty value counts as unset; any other value fails with `invalid_request`.

### Precedence

Each request property comes from the first layer that sets it: **command-line
flags, then the environment, then the matching label rule of `.elsecall.toml`,
then defaults.** `--interactive`/`--deferrable` and `--pinned`/`--placeable`/
`--member` override a rule's urgency and placement; `--hold-limit`, `--deadline`
and `--at-deadline` only set values, so `--hold-limit` on a label whose rule is
deferrable fails with `invalid_request` and asks for `--interactive`.

### Run IDs for your telemetry

`--run-id-file PATH` (on `run` and `lease acquire`) empties the file when the
request starts and writes the run ID and a newline once admission decides,
including for failures with a record such as `fleet_busy`. It stays empty when
there is no record (unmanaged or `ELSECALL=off` work, or failures before
admission). Commands that `run` admits also see `ELSECALL_RUN_ID`. Correlate with
`elsecall explain RUN_ID`.

## Service configuration

To tune the fleet from one place, opt into [fleet service configuration](docs/service-config-sync-slice.md):
edit the master's `fleet-config.toml`, review `elsecall sync service`, then confirm
its plan. Shared defaults and roster-name sections replace hand-edited settings;
existing `config.toml` files stay intact. Later `sync push` delivers validated
generations, and each service restarts automatically between work, including
supervised leftover processes. `fleet` and `doctor` show receipt, active generation,
current state and rejections. Bootstrap identity and trust remain local.

The following local-file instructions apply before opting in. Managed documents
use these same settings under `defaults` or `members.<roster-name>`; capacity,
workspace and placed executable paths must be per member.

The operator may create `config.toml` in the state directory
(`~/Library/Application Support/Elsecall/`). Every setting is optional; a missing
file means defaults. Unknown keys and invalid values stop the service with
`invalid_configuration` in its log, so a typo never silently drops a setting.
Run `elsecall service restart` after changing it.

Logs and backups have local limits:

```toml
[logs]
# 10 MiB per file by default. Sizes accept B, KB, MB, GB (decimal).
# Range: 1024 B through 1 GiB. The full current line is always preserved.
max_size = "10485760B"
# Archives in addition to service.log; range 1–100, default 5.
keep = 5

[retention]
# Oldest sync backups expire across all filenames, including removed paths.
# Range 1–10000, default 1000; the existing cap of 20 per file also applies.
sync_backups = 1000
```

Five 10 MiB archives plus the current file use about 60 MiB: enough lifecycle
and failure context for diagnosis without accumulating build output. Elsecall
owns log writes, reopens the current pathname under a shared lock, and rotates
before each complete record; the upgrade helper uses the same writer. launchd
stdout/stderr go to `/dev/null`, so inherited descriptors never pin an archive.
An individual record larger than the limit stays intact in one file and rotates
on the next record. See [State retention](docs/state-retention.md) for the full
storage audit and warning thresholds.

```toml
[capacity]
# Weighted cores kept free of Elsecall work so this machine stays responsive.
# Default 0. Decimals are allowed, up to thousandths of a core.
reserved_cores = 2
```

Further settings for work other members place here:

```toml
[workspace]
# mapped (default): copies under root; same-path: copies at the caller's own path.
location = "mapped"
# Where mapped copies and each caller repository's history live.
# Default ~/.elsecall/workspaces.
root = "/Users/devbox/.elsecall/workspaces"
# Repositories on this machine that may already hold callers' history, such as
# this account's own clones of the same projects. Default none. A first sync then
# fetches history from them instead of receiving it, and the caller sends only
# what they lack: local-only commits and the working tree's changes.
object_sources = ["/Users/devbox/Developer/acme-ios"]

[placed_runs]
# PATH for placed commands; by default the service's own (launchd's minimal PATH).
path = "/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin"
# How long processes a placed command leaves running may run after it exits,
# from "0s" (stopped at once) to "24h". Default "10m".
leftovers_cap = "10m"
```

Copies are kept between runs, and housekeeping removes them (see "Housekeeping of
copies"); their sizes are also in each run's record (`placed.sync.copy_content_bytes`).

```toml
[housekeeping]
# How often a pass runs, as deferrable work; a pass that cannot start within this
# is skipped ("skip") or started ahead of interactive work ("start").
interval = "1h"
at_deadline = "skip"
# Copies not used for this long are removed.
idle_after = "14d"
# Remove copies whose workspace the calling member deleted.
remove_deleted_workspaces = true
# Free space below the larger of these is disk pressure.
min_free_percent = 10
min_free = "50GB"
# Roll up an object store's small packs once it has this many; 0 never does.
compact_store_packs = 20
```

Object sources are only read, and any repository of the project will do, however
stale. Elsecall asks one for a commit the caller names: HEAD, its upstream or one
of HEAD's ancestors. It packs that commit's history from the source, which fails
unless the whole history is there. A source is trusted like the member's own object
store, so its objects are not hashed again. One that is missing, shallow, partial or
unrelated is passed over. Each record says
whether history came from a source (`placed.sync.history_from_source`) and how long
each phase of the sync took (`placed.sync.phases_ms`).

Placement, the load gate and learned estimates use these values; each defaults to
the value shown. On a fleet, the **master's** `[placement]` and `[estimates]`
govern placement; each member's own `[admission]` gates its own admission, and its
own `[estimates]` learn the sizes of work requested there. Every decision records
the values it used (`decision.parameters` in `explain`). "Tuning scheduling" says how
to choose them.

```toml
[placement]
# Another member must complete earlier than the calling member by this share of the
# caller's expected completion, and by at least the floor, to take its work. Waiting
# work moves by the same margin.
keep_local_margin = 0.1             # 0 to 1
keep_local_margin_floor = "2s"      # up to 10m
# The run time of a label without history, as work at speed 1.
run_prior = "60s"                   # 1s to 24h
# Sync priors by the member's copy: warm, first with the repository's history
# there, first without it.
sync_prior_warm = "3s"              # each up to 24h
sync_prior_cold_with_history = "30s"
sync_prior_cold = "120s"
# How much longer a run into a cold copy takes when no cold run was learned.
cold_run_factor = 3.0               # 1 to 100
# How long queued work without an expected time holds its cores, and running work
# without one at first.
unknown_remaining = "60s"           # 1s to 24h
# Running work without an expected time is assumed to run this share of the time
# it has already run, once that is longer than unknown_remaining. 0 keeps
# unknown_remaining fixed.
unknown_remaining_per_elapsed = 0.5 # 0 to 10
# The most a run is assumed to slow down when contended.
maximum_slowdown = 4.0              # 1 to 100
# How often waiting placeable requests are quoted and scored again.
reconsider_every = "1s"             # 250ms to 1m
# A member's lost runs count against at least this much placed work there.
minimum_loss_exposure = "1h"        # 1m to 7d

[admission]
# Outside CPU utilization, averaged over 3 s, at which nothing new starts.
saturated_utilization = 0.9         # above 0, up to 1
# The memory pressure from which nothing new starts: "warning" or "critical".
memory_pressure = "warning"

[estimates]
# Only runs and syncs requested this recently count, at most the latest `window`
# of each kind.
recency = "14d"                     # 1h to 30d
window = 20                         # 1 to 100
# Samples needed before a run time or size, a warm sync of the same workspace, a
# warm sync of any workspace to the member, or a first sync of the repository is
# learned.
minimum_runs = 3                    # each 1 to window
minimum_workspace_syncs = 2
minimum_member_syncs = 3
minimum_repository_syncs = 1
# Runs of a label's family needed before they stand in for the label.
minimum_family_runs = 3             # 1 to window
# Labels two members must both have run (minimum_runs each) before the ratio of
# their run times converts runs between them instead of their measured speeds.
minimum_ratio_labels = 1            # 1 to 100
# Runs shorter than this are mostly start-up: families and speed ratios leave
# them out.
short_run = "1s"                    # up to 10m
# How far back placed runs that lost their connection, and the placed work beside
# them, count against their member. "0s" counts none.
loss_window = "24h"                 # up to 30d
```

A member's **capacity** is its logical CPUs weighted by core type (a performance
core counts 1, an efficiency core 0.5) minus `reserved_cores`. A Mac with 10
performance and 4 efficiency cores has 12 weighted cores, so a reserve of 2 leaves
a capacity of 10.

## Tuning scheduling

The tuning loop takes minutes: rehearse to certify the simulator, search offline,
A/B rehearse the proposal, let the operator apply it, then monitor recorded-day
drift automatically. Choose a representative arrival window that fits the bound: at
compression six, one hour becomes ten minutes. Broader retained traces can still
be searched offline.

```sh
elsecall trace export --since 1h --output workload.json
elsecall tune preflight --trace workload.json --placement current --rules /path/to/consumer/.elsecall.toml
elsecall rehearse --trace workload.json --placement current --rules /path/to/consumer/.elsecall.toml --compress 6 --limit 15m --seed 42
elsecall tune --trace workload.json --placement current --rules /path/to/consumer/.elsecall.toml --budget 64 --seed 42 > proposal.json
elsecall rehearse --trace workload.json --placement current --rules /path/to/consumer/.elsecall.toml --compare proposal.json --seed 42
# The operator reviews and applies winning service/consumer settings.
elsecall report --since 7d
```

For tuning workloads, `--placement current --rules PATH` reads the placement
component of the consumer's current label rules and applies it to replayed labels,
so older pinned builds/tests can exercise remote placement. It never changes the
consumer file, measured work, sizes or urgency. Leases and deferrable work remain
pinned. All four commands support this mode; `historical` remains the default for
fidelity against history. Mode, rules digest and placement counts are reported.
Current-mode counterfactuals cannot certify historical fidelity.

Internal drift and housekeeping work defer during rehearsals, then resume with
their deadline clocks extended by the pause duration. Internal records are marked
by service ownership and never trigger the production-arrival safety guard.

A complete rehearsal passing the existing fidelity tolerances issues a simulator
certificate, valid for seven days and the same build. Stopped/incomplete rehearsals
issue none. A complete recorded day plus held-out evaluation remains optional
stronger evidence. Missing constraint evidence stays explicit even when the
simulator is trusted.

`tune` prints baseline/best metrics, uncertainty envelopes, alternatives,
sensitivity and proposed service/consumer changes. It never applies them. The
default budget is 64 unique simulations; `--space space.json` supplies a bounded
custom grid. `--compare` runs the same frozen trace and seed twice, temporarily
leases proposed scheduling settings through fleet sync for B, and restores the
originals on every exit path. Proposed label sizes affect synthetic requests only.
Consumer-only proposals apply no settings lease and leave the sync generation
unchanged. A service-settings lease preserves the exact original document bytes;
removing it restores the same `service.fingerprint`. Sync generations may advance
on application and removal and are not restoration identities.
`sync status` and `fleet` also expose each member's
`service.effective_fingerprint`, covering its active parsed scheduling settings,
weighted cores, capacity and reserve. Compare each member with its own pre-run
fingerprints: the document fingerprint checks byte identity; the effective
fingerprint checks active behavior. `--compare` reports per-member
`effective_fingerprints.before` and `.after`, requires equality, and fails with
`settings_restored=false` and an explicit error if restoration cannot be verified.
After controller loss, wait for lease expiry and idle service activation, then
check the effective fingerprint, `pending=false` and `rejection=null`. The expired
overlay may remain in the document until the next ordinary sync scan.
Its observed metrics are side by side; a measured objective improvement must exceed
the fixed 5% noise band without new refusals or known hard violations. One pair
does not estimate statistical confidence. The operator applies winning settings.

Every member checks newly completed recorded days as deferrable work and retains
results in `report.tuning_evidence`. Incomplete inputs are `not_checkable`. Failed
fidelity creates a high-severity `simulator_drift` finding and revokes trust until
a new rehearsal passes. See [the harness contract](docs/tuning-harness.md).


### Fleet simulation

`elsecall simulate --trace workload.json --seed 0` runs the real admission,
placement, estimates and learning services against virtual machines, processes,
member communication and time. It does not contact or change an installed service.
The JSON response contains the charged time-to-result objective, admitted latency
distribution, run/lease counts, reserve and declared-capacity violations,
`fleet_busy` numerator/denominator, other refusals, slowdown, strongest-eligible
member usage, per-member minute timelines and per-request outcomes. Lower and
upper demand/background/contention envelopes accompany the point result. Result
return is modelled as zero. A fixed trace, candidate and seed produce identical
output; the current model uses deterministic envelopes rather than random draws.

```sh
elsecall trace export --since 7d --output workload.json
elsecall simulate --trace workload.json --seed 0
elsecall simulate --trace workload.json --set keep_local_margin=0.2 \
  --set 'label.acme test *.size=1.5' --set member.devbox.reserved_cores=2
elsecall simulate --trace workload.json --fidelity
elsecall simulate --trace workload.json --fidelity --day 2026-11-03
```

`--set` accepts the service's placement, admission and estimate parameters,
`member.NAME.reserved_cores`, `label.PATTERN.size` (first matching rule wins), and
`hold_limit` for interactive requests. A reserve candidate requires recorded
settings and may not reduce their approved floor. Repeated keys are rejected.
Execution work and physical fallback priors remain frozen when estimate settings
change. `--day` evaluates one UTC day while earlier trace arrivals rebuild causal
history. Without a day, `--fidelity` chooses the busiest finished UTC day before
checking coverage, and rejects `--set`.

Fidelity publishes each fixed contract comparison, counts, outliers and queue
timelines. Missing evidence produces `blocked_by_missing_evidence`; a computed
metric can still fail underneath that status. Older installed services without
the trace routes must be upgraded in a separate release verification session
before a fleet export can be collected. Historical settings, workspace identity,
pre-window estimator state and incomplete load cannot be replaced with current
configuration. Grant-derived outside CPU and observational contention also leave
constraint certification uncertain. Read `metrics.status`, `missing_evidence`,
`metrics.uncertain_constraints` and [the harness contract](docs/tuning-harness.md) before
interpreting a score; tuning remains untrusted until its fidelity gate passes.

The [tuning harness contract](docs/tuning-harness.md) defines H1 trace export, H2 recorded-day fidelity, H3 settings proposals and H4 fleet validation. `elsecall trace export --since 7d --output workload.json` writes a new private trace from member histories, with demand sensitivity bands and observational contention curves. Read `limits` and member errors before using it. `--local-history --member <name>` reads only local SQLite history without contacting or changing a service. `report` also exposes `demand` and an `effective_demand` finding when at least five fitted runs and the full sensitivity band disagree with declared size by the [documented thresholds](docs/tuning-harness.md#effective-demand-and-uncertainty).

### Controlled fleet rehearsal

`elsecall rehearse --trace workload.json --compress 6 --limit 15m --seed 17`
executes a trace as synthetic CPU work on the real fleet. Run it on the master
with compatible services on every member. Six is the default compression: an
hour of arrivals becomes ten minutes, with room for actual sync and queues under
the default 15-minute workload bound. Normal admission and placement retain size,
urgency, required member and the resolved hold limit; deadlines are rebased and
compressed. Scratch git repositories create real cold/warm copies and syncs.
The command requires H1 effective-demand evidence; it never substitutes declared
sizes for unknown demand. Only `--compare` temporarily leases scheduling settings;
rehearsal installs nothing.

Startup refuses production work anywhere unless `--force`. New production arrivals
stop submissions even with `--force`; submitted work finishes up to the bound.
Expiry, Ctrl-C and failures stop synthetic processes and clean scratch trees,
member copies/stores and sync caches. Member watchdogs and recovery journals also
handle a lost controller or restarted service. Cleanup gets at most 15 additional
seconds before being reported pending; export/fidelity then follow. Pending cleanup
or an orchestration error exits unsuccessfully. No production process is stopped.

Labels use reserved `rehearsal:SESSION:ORIGINAL_LABEL`, records carry
`synthetic: true`, and synthetic data never trains production estimates, sizes,
speed ratios or sync priors. Reports exclude its request metrics; actual CPU load
stays visible. The new private trace (use `--output FILE` to name it) and command
JSON retain original label/family/kind mappings and intended-versus-recorded arrival
errors. Root leases become run grants, and fully contained nested work is included
in its root demand. Missing inputs, active carry-in and uncovered nested exposure
block explicitly; see [H4a's input and safety rules](docs/tuning-harness.md#h4a-controlled-synthetic-fleet-rehearsal).

Every completed live command exports its own session and runs `simulate --fidelity`
on that export, reporting observed metrics alongside the full comparisons.
Tolerances remain unchanged in actual compressed milliseconds. A complete passing
rehearsal certifies tuning; a bounded recorder proves load coverage within its own
window. Certificates retain the trace/settings/build and expire after seven days
or any build change. A killed controller cannot finish export, but members clean
up and temporary settings expire back to their originals even if disconnected.
Automated tests use temporary loopback services; installed-system execution belongs
to release verification.

### Reports and single-decision replay

Scheduling is doing well when load stays low and balanced, the strongest machine is
used, the master stays responsive, and few runs fail with `fleet_busy`. An agent
tunes the values above against real history, from the CLI alone and in JSON:

1. **Report** how the fleet did over a period and how accurate the estimates were
   (`elsecall report`, its own item; until it lands, `elsecall runs`, `explain` and
   each member's `status.load_history` in `elsecall fleet`).
2. **Replay** the recorded decisions under a change, on the master:
   `elsecall replay --since 7d --set keep_local_margin=0.2`. It shows which decisions
   would flip and the estimated effect, before anything changes.
3. **Change** the master's managed `fleet-config.toml`: shared `[defaults.placement]`
   or `[defaults.admission]`, with `[members.<name>.<section>]` for differences.
   Before opting in, change the relevant member's local `config.toml` instead.
4. **Apply** with `elsecall sync push` and watch `fleet` for every member to become
   current; services restart automatically at idle. Unmanaged local files still
   require `elsecall service restart`.
5. **Compare**: report again over a period after application. Every decision records
   the values it used, so replay `--since` the restart replays only decisions made
   under the new values.

`elsecall replay [--since DURATION|TIME] [--set KEY=VALUE ...]`:

- **`--since`** takes a duration back from now (`24h`, `7d`) or an RFC 3339 time.
  The default is every retained record (30 days). At most the latest 5000 placeable
  requests are read; `limited` says when that bound applied.
- **`--set`** takes `config.toml`'s keys as `key=value` or `section.key=value`
  (`keep_local_margin=0.2`, `admission.saturated_utilization=0.95`), validated as the
  service validates them, and may be repeated. Each value applies over each
  decision's own recorded parameters. `[placement]` and `[admission]` keys replay;
  `reconsider_every` (when requests are scored again, not how) and `[estimates]`
  (records hold what history said, already resolved) are refused with
  `invalid_request`. Without `--set`, replay checks that every decision reproduces.
- **How.** Each decision is first scored again under the parameters it recorded and
  must reproduce its recorded winner and every candidate's estimate exactly; one that
  does not is counted as `not_reproduced` and never replayed. It is then scored under
  the change. Replay uses the request's first scoring, on arrival (`arrival`).
  Records from before arrival inputs were kept use their last scoring
  (`last_scoring`), whose winner a waiting request moved to only by the moving
  margin. Records from before any inputs were kept replay only `keep_local_margin`
  and `keep_local_margin_floor`, which choose between their recorded completions
  (`recorded_estimates`); one that started elsewhere than those choose, as a
  waiting request can, is `started_elsewhere_before_inputs`. Running work recorded
  without `elapsed_ms` (before it was recorded) keeps `unknown_remaining` fixed, as
  it was scored then, under any `unknown_remaining_per_elapsed`.
- **Gates.** An `[admission]` change derives every member's gate again from its
  recorded load, as if every member had set the value.
- **Not simulated.** A flipped decision would have changed the queues, waits, load
  and copies later decisions saw. Replay re-scores each decision against what it
  actually saw, and says so in `limits`.

Its JSON names whose records it read (`member`, `since`, `until`, `records_read`,
`limited`) and holds:

- **`decisions`:** `placement` (placement decisions read), `replayed`, `flipped`,
  `replayed_from` (`arrival`, `last_scoring`, `recorded_estimates`), and
  `not_replayable`, grouped by `why` (`recorded_before_inputs`,
  `started_elsewhere_before_inputs`, `not_reproduced`, `gate_not_reproduced`,
  `invalid_parameters`) with an `explanation` and, except for the first, the latest
  1000 `run_ids`.
- **`effect`:** decisions each member won (`winners`, recorded and replayed),
  `no_winner`, `kept_on_caller`, `fleet_busy` (`recorded`; `had_a_winner`, those
  whose scoring had a place but that failed later; and `would_place`, those without
  one that have one under the change), and `completion_difference_seconds` over the
  flips (`total`, `mean`, `earlier`, `later`, `unknown`).
- **`flips`:** the latest 1000 decisions whose winner changes (`flips_truncated`
  when more did; the counts and effects include them all): `run_id`, `label`,
  `requested_at`, `verdict`, `ran_on`, `replayed_from`, `scored_at`, `recorded` and
  `replayed` (`winner`, its `completion_seconds`, the `run_level` its run estimate
  came from, `reason`), the recorded winner's
  completion under the change (`replayed.recorded_winner_completion_seconds`),
  `completion_difference_seconds` (the replayed winner's against it, both under the
  change; positive is later), and `actual` (what the recorded run took from the
  scoring: `started_after_seconds`, `run_seconds`, `completion_seconds`).

Placement decisions are made on the master, so replay there. Decisions made while
another member was master are in that member's records.

## Housekeeping of copies

Copies keep their ignored build state, so they grow, and callers create and delete
many worktrees. Each member removes the copies it holds in a **housekeeping pass**:

- **Deleted on the caller** (`caller_deleted`): the pass asks each calling member,
  over the authenticated member channel, whether the workspaces of its copies here
  still exist. Only a copy's own calling member is asked, and it answers only about
  workspaces it pushed to this member. A copy used after the question is kept.
- **Idle** (`idle`): not used for `idle_after`. A copy is used when a run is
  admitted for it and when that run ends.
- **Disk pressure** (`disk_pressure`): while the copies' volume has less free space
  than the larger of `min_free_percent` and `min_free`, copies not in use go, least
  recently used first, until it `recovered`. The outcome is `pending` if what went
  should have been enough but the volume does not show it yet. If even every copy
  not in use could not make up the shortfall, none goes and the pass reports
  `unrecoverable`.
- **Missing** (`missing`): the copy's directory was already gone; only Elsecall's
  record of it is removed.
- **Object stores** (`unreferenced`) that no copy uses go too. Stores with many packs
  have their small packs rolled up, and no object is ever dropped.

A copy is **never removed while a run uses it**, or while an admitted run is about to
attach, and removing a copy never races a sync to it: a run holds its copy from
admission to its end, and housekeeping holds a copy, or a store, exclusively while it
removes it. Offers for it meanwhile are refused as busy. A directory is removed only
if it carries the copy's mark, and it is moved aside first.

Every removal is recorded with its reason. The calling member is told which of its
copies went, and drops its sync state for them. Each member also cleans up its own
caller-side state of deleted workspaces.

Passes run every `interval`, the first within five minutes of the service starting.
They run as this machine's own **deferrable, pinned** work, so they wait behind
interactive work. With `at_deadline = "start"`, a pass that reaches its deadline goes
ahead of interactive work, still within capacity. Each has a record labelled `elsecall housekeeping`
in `elsecall runs`. A member refuses placed work only while its own *interactive*
requests wait, so a queued pass never keeps placed runs out.

- `elsecall copies` lists, quickly and as last measured:
  - each copy's calling member, caller path, location, state (`in_use` or `idle`),
    last use, when it becomes idle long enough to remove, and size;
  - each object store with its size;
  - the volume's free space against its threshold, the settings, the last pass, and
    the latest 50 removals.
- `elsecall copies prune` runs a pass now: what it removed or spared, the calling
  members asked, disk pressure and caller state. `--dry-run` reports the same and
  removes nothing; it still records the sizes it measures and the copies it finds.
  A pass with problems exits 1. A manual pass runs at once, outside admission.
- `elsecall fleet` shows each member's `status.copies`:
  - copies and stores with their bytes, as last measured, and `unmeasured_copies`;
  - the workspace volume's free space against its threshold (`disk`);
  - the last pass since the service started.

  `status.copies_error` says why they could not be read. Members on earlier versions
  leave both out. Only the workspace root's volume is shown; passes judge every
  volume that holds copies.

## Fleet

Every member runs the Elsecall service and listens on **its tailnet addresses only**
(TCP port 47821), discovered through the Tailscale CLI. Tailscale encrypts the
transport; it never establishes identity. Each member has its own Ed25519 key,
created on first start in `member.key` (owner-only, never logged or printed). Every
tailnet request must be signed by an enrolled member's key; anything else receives an
empty `401`, whatever the route. Responses are signed by the responding member, so the
CLI never reports state it could not verify. Requests outside a 60-second clock window
or with a reused nonce are refused, so member clocks must be roughly in sync.

Peer failures retain their kind (connect, authentication, send, receive, timeout,
protocol or verified remote refusal), phase and duration. A verified refusal has
phase `request`, since every I/O phase completed, and keeps the member's stable code
in `refusal`, such as `member_upgrading`, but never its message. `fleet` shows recent
outgoing failures from each member, `explain` shows interval context and failed
quotes, and `report.peer_io` collects both members' retained history with coverage
and truncation. Numeric tailnet endpoints do not use DNS or TLS; identity is
verified by signatures. Status and quotes retry transient I/O failures once with
100 ms backoff within a six-second total bound; each endpoint's connect is bounded
at one second. Offers and mutations never retry automatically. A failed quote
leaves that member out while placement continues with eligible responding members.
Sync and result streams keep their longer exchange/inactivity bounds. See the
[harness peer-I/O contract](docs/tuning-harness.md#peer-io-diagnosis-and-bounds).

Enrolment takes a step on each machine. Member codes contain only public keys and
endpoints; carry them between machines yourself (for example over ssh):

```sh
# On the machine joining the fleet:
elsecall fleet identity            # prints data.code
# On any enrolled member (the first machine is a fleet of one, and its master):
elsecall fleet enroll <code>       # prints data.next_step: the join command
# Back on the joining machine:
elsecall fleet join <code from next_step>
elsecall fleet
```

Removal also takes a step on each machine: `elsecall fleet remove <name>` on another
member, then `elsecall fleet leave` on the removed machine. A removed member's
requests are refused immediately. The master cannot be removed, and a member cannot
remove itself.

Exactly one member is **master**. Every member records which member that is; the
first machine starts as master. A member accepts placement-authority requests only from
its recorded master's key: other enrolled members are authenticated but receive
`not_master`. Placement itself arrives in later steps; `elsecall fleet check-authority
<name>` sends the minimal placement-authority request and shows whether that member
would accept placement from this one. `elsecall fleet move-master <name>`, run on any
member, moves the role. There is no automatic election, and the master cannot be
removed or `leave` without `--force`.

`enroll`, `remove` and `move-master` update every member and refuse while any member is
unreachable (for `enroll`, other than the member being enrolled, which may not have
joined yet: it shows as `awaiting_join`, and enrolling it again prints the join step
again); `--force` changes the reachable members and reports the others as
`failed`. A forced move leaves an unreachable member trusting the old master until it
is updated. A change that some members did not record exits 1 with
`data.complete: false`; run `elsecall fleet sync` once they are back. Sync also
resolves members that changed the fleet independently (a `roster_conflict`): the
member you run it on wins, and its roster is re-issued above every epoch seen. Sync
refuses with `stale_roster`, naming the up-to-date member, when another member records
a newer roster; run it there instead, or pass `--override` to make this member's
roster win anyway. Any enrolled member can change the roster and move the master role;
membership is an operator-level trust relationship between the operator's machines,
and placement authority is what only the master holds.

`elsecall fleet` reports each member as
`reachable` (with its own `status`, including its machine inspection, and whether it
is `in_sync` with this member's roster) or `unreachable` (with an `error` such as
`member_unavailable`, `member_rejected` or `member_unverified`). Machine state is
passed through as each member reports it, so members on newer versions may add fields.

Each member also reports its **load history** as `status.load_history`, with
`last_hour` and `last_day` up to the current second (`status.load_history_error`
says why it could not be read). Every member keeps one row per minute in
`load.sqlite`, summarised from the load samples admission takes every second
anyway, whether or not Elsecall work runs, for 30 days. A row holds machine state
only, no processes, arguments, environment values or paths:

- `cpu_mean` and `cpu_max`: whole-machine CPU utilization, 0 to 1;
- `outside_mean`: utilization beyond the share Elsecall's grants hold (outside load);
- `in_use_mean` and `in_use_max`: cores Elsecall's grants held, of `capacity` and
  `weighted_cores`;
- `queued_max`: requests waiting in the member's own queue, and
  `interactive_queued_max` of them interactive ones;
- `busy_seconds`: seconds Elsecall work held cores;
- `saturated_seconds`, `saturated_busy_seconds` (while Elsecall work held cores) and
  `memory_pressure_seconds`: seconds the load gate was closed;
- `worst_memory_pressure`, `worst_thermal_state`, and the measured `speed_min` and
  `speed_max`;
- `seconds`: how many seconds were sampled.

**Outside pressure.** When something Elsecall did not start holds a member's
admission gate closed, the member says so. Each member in `elsecall fleet` has
`blocked` (also `status.blocked`), null while it admits:

```json
"blocked": {
  "reason": "memory_pressure", "level": "warning",
  "since": "2026-11-10T04:48:11Z", "attributed_at": "2026-11-10T13:09:11Z",
  "outside": [
    {"name": "fseventsd", "pid": 412, "owner": "root", "resident_bytes": 28991029248,
     "cpu_cores": 1.0, "uptime_seconds": 5367600},
    {"name": "Simulator", "pid": 8123, "owner": "this_user", "resident_bytes": 2147483648,
     "footprint_bytes": 3221225472, "cpu_cores": 0.1, "uptime_seconds": 7200}
  ]
}
```

- `reason` is `memory_pressure` (with the kernel's `level`) or `outside_saturation`
  (with `outside_utilization`, the share of the CPUs Elsecall's grants do not
  explain).
- `since` is when the gate closed. A member counts as blocked only once the gate has
  stayed closed for **5 minutes**, without staying open for 10 seconds in between.
  That is more than three 90-second hold limits, so link steps, simulator boots and
  indexing bursts never show. It stops being blocked once the gate has stayed open
  for a minute, so a moment's relief does not flap it. A service restart during a
  block, such as a fleet upgrade, keeps its `since`.
- `outside` lists up to five processes outside Elsecall's work, leading first: by
  resident memory under memory pressure, by CPU under saturation. Elsecall's own work
  is the service and the holders of active grants, with all their descendants, plus,
  while a grant is active, the service user's processes started since the earliest
  active grant did (simulators and build daemons a run starts are adopted by
  launchd), so during a long grant this user's newly started apps also count as
  Elsecall's. If outside processes use no more than Elsecall's own work (all of them
  for CPU; the five leading ones for memory, since resident sizes count shared pages
  in every process), the closure is not attributed outside and nothing is reported. The kernel and launchd
  are never named. Processes are named only: never their arguments, environment or
  paths. `resident_bytes` is readable for every process; `footprint_bytes`, which
  macOS weighs for memory pressure and which counts compressed pages, is present only
  where macOS lets the service read it (its own user's processes, not root daemons).
  `cpu_cores` is the average over the last minute (1.0 is one whole core).
  `attributed_at` is when they were last sampled, about once a minute.

`elsecall doctor` lists blocked members, the longest-blocked first, as
`outside_pressure`, and its `next_step` names the leading process and who can restart
it: for a root daemon, `sudo kill 412` on that member once `ps -c -o comm= -p 412`
still names it (launchd restarts the system daemons it manages); for a process whose
restart ends the session, such as WindowServer, closing apps or rebooting.
**Elsecall never stops an outside process itself.** On a member outside a
fleet, `doctor` reports the member alone, as does `GET /v1/outside-pressure` on the
local socket. Every member keeps its blocked episodes (onset, end, and the processes
last attributed) in `outside-pressure.sqlite` for 30 days after they end.

**Early warnings.** Before an outside process closes the gate, a member warns about
it. Each member in `elsecall fleet` has `outside_warnings` (also
`status.outside_warnings`), empty while nothing warrants one:

```json
"outside_warnings": [
  {"concern": "spinning", "cpu_cores": 1.0, "process": "fseventsd", "pid": 412,
   "started": "2026-09-09T08:00:00.000Z", "owner": "root",
   "since": "2026-11-08T21:00:00.000Z", "observed_at": "2026-11-09T03:05:00.000Z"}
]
```

A warning is not a block: the member keeps admitting work. Every 5 minutes, the
member samples its processes (one `ps` listing, about 20 ms) and keeps the outside
ones using at least a quarter core or 256 MiB. Elsecall's own work is told apart as
for `blocked`, so a long granted build never warns. A process that was ever
Elsecall's own work stays so until it exits, so a build server or VM one grant
started and later grants reuse never warns either; the cost is that this user's
apps started while a grant was active are never warned about. Each process is followed by its
PID and start time, never by name: builds and tests are many short-lived processes,
while a daemon that spins or leaks keeps one identity for hours.

- `spinning`: over the last 6 hours, it kept 0.8 cores or more busy on average, and at
  least half a core in 90% of the 5-minute samples. A spinner squeezed by heavy work
  still counts; Spotlight indexing in bursts of about half a core does not.
  `cpu_cores` is the 6-hour average.
- `growing`: over the last 6 hours, its memory rose by at least 1 GiB and 25%, steadily
  (each quarter of the window above the one before, which an app loading a project
  once does not do), to at least 2 GiB or 5% of physical memory, whichever is more.
  `memory_bytes` and `growth_bytes_per_hour`.
- `oversized`: it held a quarter of physical memory or more in every sample of the
  last hour. `memory_bytes` and `share_of_memory`. It replaces `growing` for the same
  process.

Memory is the footprint where macOS lets the service read it, and the resident size
otherwise (root daemons). A window is judged only once samples cover all of it (at
least 80% of its samples taken, reaching back to its start) and the process lived
through it; a Mac asleep for more than about an hour of the window is not judged
until the window has passed. Samples are kept for 12 hours in
`outside-processes.sqlite`, so a service restart does not restart a window, and a
restarted service shows the warnings it had until its second sample confirms or ends
them. `since` is as far back as the samples show the concern holding, and keeps its
first value while the concern holds. A warning ends when a sample no longer shows
it, for example once the process was restarted; one that holds again within an hour
continues. Warnings are kept for 30 days after they end, for reports.

In the 2026-11-10 incident, a hung `fseventsd` held a core in every idle minute from
Nov 8 21:00Z; `spinning` would have shown from about Nov 9 03:00Z, 26 hours before
memory pressure closed the gate.

`elsecall doctor` lists them, the longest-standing first, as `outside_warnings`.
When nothing more urgent needs doing (unhealthy service, version skew, a stale
LaunchAgent, a blocked member, service configuration), its `next_step` names the
process and who can restart it, as for a blocked member. **Elsecall never stops an
outside process itself.** `GET /v1/outside-pressure` on the local socket also
returns this member's `warnings`.

**Firewall.** When the macOS Application Firewall is on, it drops tailnet connections
to Elsecall's executable unless the firewall has a rule allowing it; loopback is never
filtered. Elsecall has no Developer ID signature, so the rule is not added
automatically, and adding one needs administrator rights. `install` and `doctor`
report `member_listener_firewall`: `unfiltered`, `allowed`, `unknown`, or `blocked`
with the commands to run. On an enrolled member, `doctor` is unhealthy while it is
`blocked` or while fleet state is unavailable (`member_identity_error`). The rule belongs
to the executable at `~/Library/Application Support/Elsecall/elsecall`; check `doctor`
after each update, including `fleet upgrade`.

If the member key or fleet record is unreadable, the service still starts: `status` and
`machine` keep working, the member listener stays closed, and every `fleet` command
returns the problem (such as `invalid_fleet_state`) with its repair. Only the service
reads `member.key`; the CLI asks the service to sign on its behalf.

## Upgrading the fleet

Build and `install` on the master, then distribute that build from it:

```sh
mise run build && ./target/release/elsecall install   # on the master
elsecall fleet upgrade                               # every other member
elsecall fleet upgrade --member macbook-pro         # one member
elsecall fleet upgrade --member macbook-pro --executable ./target/release/elsecall
```

The master sends its installed executable, or the build `--executable` names (to try
a build on one member first), over the authenticated member channel. Only the
recorded master may: a member refuses anyone else with `not_master`, since installing
a build is running code on it. Each member, at the same time as the others:

1. **Verifies.** The master signs a manifest with its member key: the content's
   SHA-256 digest and size, the member it is for, the platform and architectures the
   executable runs on, and the wait bound. The member checks the signature against
   its recorded master's key, receives the content, and refuses it with
   `digest_mismatch` if it does not match, `wrong_platform` if its Mach-O header is
   not a macOS executable for the member's architecture, or `manifest_unverified` /
   `manifest_expired`. Nothing changes on a refusal.
2. **Waits for its work.** From then on the member turns away runs other members
   would place on it (it answers placement with `member_upgrading`, so placement uses
   the others) and waits until no run or lease runs or queues there, for at most
   `--wait` (default 5m, at most 1h). If work
   remains, the member is `deferred`: nothing is installed, its work keeps running,
   and the report lists the work it waited for. Running work is never stopped. A
   request made in the second between the member falling idle and its restart is
   treated as at any service restart: a granted lease or run keeps its capacity, a
   queued request ends with `service_unavailable`.

   Elsecall's own work, such as housekeeping and its fidelity drift check, is
   marked `internal: true` in the report. While the member waits, its queued
   internal requests are held, so none starts only to be cut short by the restart,
   and they are not waited for: the restart ends them and the new service asks
   again on its own schedule. A deferred or interrupted upgrade releases them.
   Waiting for them could last forever, because a member whose gate stays closed
   (for example under sustained memory pressure) admits nothing, and housekeeping
   asks again every interval. A running internal pass is waited for like any other
   work. It was admitted, so it ends by itself, and stopping it partway through a
   removal or a compaction would leave work for the next pass.
3. **Installs and confirms.** A helper process installs the build through the same
   owned, atomic installation `elsecall install` uses (same executable path, same
   session, same LaunchAgent), restarts the service, and waits up to 20 seconds for
   the new service to answer with the new build ID, reach its fleet identity, and
   stay up. If `install` refuses before stopping anything (for example another
   install is running), the member is `interrupted` and keeps its service as it was.
   The helper is the build being replaced, so it writes that build's LaunchAgent; a
   change to the LaunchAgent takes effect at the member's next install, which
   `elsecall doctor` there flags as `launch_agent_current: false`.
4. **Rolls back.** If the new build does not become ready, the helper installs the
   previous executable the same way and confirms it, so the member keeps a working
   service; the report says `rolled_back` with the build's error.

The command waits for every member and exits 0 only if each now runs the build.
`data.distributed` names the build (`build`, `digest`, `size`, `architectures`,
`source`), and each member in `data.members` has an `outcome`:

| Outcome | Meaning |
| --- | --- |
| `upgraded` | Runs the build and is ready (`from`, `running`) |
| `already_current` | Already ran it; nothing was sent |
| `rolled_back` | The build did not become ready (`error`, such as `startup_failed`); the previous build runs again (`running`) |
| `rollback_failed` | Neither build became ready; `error.hint` says how to restore the previous build by hand |
| `deferred` | Work still ran when the wait ended (`in_flight`); run the command again later or with a longer `--wait` |
| `refused` | The member refused the build (`error.code`); nothing changed |
| `interrupted` | The attempt ended before anything was installed, such as a service restart while it waited |
| `unreachable` | The member could not be reached; nothing was sent |
| `unknown` | The member did not report how it ended in time (`last_state`); check `elsecall fleet` |

The master itself is not upgraded over the fleet: `install` updates it. A member's
latest upgrade is recorded in `upgrade/state.json` in its state directory, which the
restarted service reports; logs record upgrade events, IDs, build IDs and codes.

**Versions.** `elsecall fleet` shows each member's `build` (`version` and `build`
ID) and `version_skew` (whether it differs from the master's), and `data.version_skew`
lists the members on another build with the repair. `doctor` shows the same as
`fleet_builds` and `version_skew`; skew does not change doctor's verdict about this
machine, but its `next_step` points at the repair. Until members run one build:

- **Refused with `version_skew`:** work between members on different builds:
  placement (quotes, placed runs and their channels), configuration sync, workspace
  checks during housekeeping and gathering reports. A placeable run therefore stays
  off a skewed member, `run --member` on one fails, and `report` lists it with
  `records_error.code` `version_skew`. Each refusal's hint is to run `fleet upgrade`.
- **Always open:** member status (so `fleet` shows the skew), the roster (enrolment,
  removal, moving the master, `fleet sync`), `fleet check-authority`, and the upgrade
  itself, whose wire format is frozen, so a member on any build that has upgrades
  can always be brought forward.

Builds from before upgrades name no build and are not refused; install a build with
upgrades on such a member once by hand (copy it over and run its `install`), and
later builds can be upgraded over the fleet.

**Firewall.** The upgraded executable keeps its path, so the Application Firewall
rule for it (see "Fleet") keeps applying. Check `member_listener_firewall` in
`doctor` on upgraded members.

## Configuration sync

The master mirrors declared configuration and secret files, such as a repository's
git-ignored `~/.config/<app>` credentials, to every member, so a run placed on
another member reads the same configuration as a local one. Only files and
directories the operator declares and enables are synced: never environment
variables, tool logins or keychain items. Elsecall's own service settings have a
separate typed attachment on the same channel; opt in with `sync service` as
[documented here](docs/service-config-sync-slice.md). Ordinary mirrored paths still
cannot target `config.toml` or Elsecall's state.

**Declare** paths in the master's `config.toml`, relative to home; each maps to the
same path below each member's own home. `sync` commands read the declaration when
they run, so no restart is needed:

```toml
[sync]
paths = ["~/.config/acme-ios"]
```

A path is refused (`invalid_configuration`, and again by every member on arrival)
when it is home itself, not below home, not printable ASCII (APFS folds other
letters onto ASCII ones), contains `.` or `..`, overlaps another declared path, or
is, lies inside or contains `~/.ssh`, `~/.gnupg`, `~/Library/Keychains`,
`~/Library/LaunchAgents`, `~/Library/Application Support/Elsecall`, `~/.elsecall` or
`~/.local/bin`, compared case-insensitively. Each member also refuses a path that
overlaps its workspace root, and a path that really resolves (through links above
it, or as the file system names it) somewhere refused or outside home; that check
comes before any directory is created. At most 16 paths, 4,000 files and
directories, 16 MiB per file and 64 MiB in all are synced.

The master syncs only enabled paths its own `[sync] paths` declares. An enabled path
it no longer declares, for example on a member that just became master, is
**paused**: members keep it as last synced, and `sync status` lists it under the
master's `paused`, until it is declared there again or disabled.

**Plan, then enable.** On the master:

```sh
elsecall sync plan                                   # every declared or enabled path
elsecall sync enable ~/.config/acme-ios         # shows the plan and its plan_id
elsecall sync enable ~/.config/acme-ios --confirm <plan_id>
```

The plan lists, per member and per file, what syncing would do: `added`,
`changed`, `removed`, `mode_changed`, `locally_edited` or `overridden`, with
`backup: true` where the member's own version is kept before it is replaced. Files
are named and shown by short fingerprints, never by content. `--confirm` enables
exactly the plan with that ID; if anything changed since, it refuses with
`plan_changed` and shows the new plan. A path missing on the master is refused, since
enabling it would remove it everywhere. `elsecall sync disable <path>` works the same
way and stops syncing: every member keeps its files as they are, overrides and
backups included.

**Status and pushing**, from any member:

```sh
elsecall sync status          # every member: in_sync, pending, locally_edited, source
elsecall sync push            # asks the master to push its current files now
elsecall sync push --member <name>   # only that member, and only if it differs
elsecall sync history         # this member's records: paths, sizes, fingerprints, outcomes
```

`elsecall fleet` shows each member's `config_sync` state, and its
`status.config_sync` holds the details. `sync status` lists each member's overridden
files, files that differ (local edits not yet backed up, overrides not yet applied)
and backups with their location.

**How it syncs.**

- The master's files are the truth. Nothing runs in the background: sync happens
  when you enable or disable a path, run `sync push`, or move the master role. The
  master then checks the enabled paths (reading again only files whose size, times
  or inode changed), issues a new generation if they changed, and pushes it. After
  editing a synced file on the master, or enrolling a member, run `sync push` to
  update every member at once. A placed run also brings its member up to date
  before it starts, pushing only if the member differs, so placed runs always see
  the master's current files; if that fails, a placeable run goes to another member
  or the caller, and a run required on that member fails with `config_sync_failed`
  (see "Running on another member").
- Each member is synced independently: pushes to the same member happen one at a
  time, while a slow or unreachable member never delays syncs to the others. The
  master scans and issues one generation at a time, and every push sends a whole
  generation: one issued meanwhile goes out with the next sync.
- Content, deletions and permissions are mirrored. Members hold synced paths
  owner-only: directories 0700, files the master's owner permission bits (0600, or
  0700 for an executable), never group or other access. Links, special files and
  `.git` directories inside a synced path are not synced; plans list them as
  `skipped`. A member whose copy holds such entries keeps its whole previous tree
  as a backup when sync replaces it; the master's own are never touched.
- Content moves only over the authenticated member channel, sealed with keys both
  members sign, as placed runs are. A member accepts a push only from its recorded
  master's key. It receives and verifies all content before anything changes, then
  replaces each path in one atomic step (the new tree is built beside it and swapped
  in), so a lost connection or a failed push leaves the previous state in place.
- Every member keeps an exact replica of the synced set, without its overrides, in
  Elsecall's state (`config-sync/`, owner-only), so any member can become master
  without loss.

**Overrides.** A member that needs its own value for a synced file puts it in
`~/.elsecall/sync-overrides/` at the same home-relative path, for example
`~/.elsecall/sync-overrides/.config/acme-ios/.env` (the directory is created
0700). Override files win at the declared path, are applied at each sync (`sync push`
applies a changed one), and are never touched by sync; an override that is a single file
replaces the whole path. There is no templating. On the master, overrides are
written in place, file by file, and overridden files keep the values last issued to
the fleet; removing an override there restores the fleet's value, which is never
pushed from the override.

**Local edits.** A member's file that was edited since the last sync is reported as
`locally_edited`. At the next sync the master still wins: the member first backs the
local version up, owner-only, in `config-sync/backups/` in Elsecall's state, and lists
it in `sync status` (once per distinct content; at most 20 backups per file, older
ones removed and counted). Content a member had before it first synced a path is
backed up the same way, and so is anything a former master changed but never pushed.
Backup fingerprints use a key only that member holds.

**Moving the master.** `elsecall fleet move-master <name>` first has the old master
push its final state, then checks that every member holds it; only then does the
role move, and its report includes `config_sync`. It refuses
(`config_sync_handover_failed`, `config_sync_incomplete`) unless `--force`. Forced
with the old master unreachable, the new master's replica becomes the truth, and the
report warns that changes the old master never pushed are lost to the fleet; when the
old master returns as a member, its differing files are backed up before they are
replaced. A member that becomes master first restores its replica, backing up any
local edits, so its own drift never becomes the fleet's truth. After a handover's
final push the old master issues nothing for 30 seconds while the role moves. The
synced set travels with the role; paths the new master does not declare are paused.

**Removing a member** stops pushes to it. Its files stay as they were last applied.

Values never appear in logs, records, status or command output: only file names,
sizes and fingerprints, keyed with a random fleet key that stays in replicas and the
sealed channel, so a fingerprint cannot be checked against guessed values.

## Diagnosis and removal

```sh
elsecall doctor
elsecall service restart
# The doctor output also reports this log path:
tail -n 50 "$HOME/Library/Application Support/Elsecall/service.log"
```

`service_unavailable` means no connection could be made. Use `doctor` to distinguish
missing installation, unloaded service and unsafe state permissions. `install`
repairs missing owned files and starts an unloaded service. On a
`service_manager_error`, inspect the registration in the installation's session:

```sh
launchctl print "gui/$(id -u)/dev.elsecall.agent"   # desktop session
launchctl print "user/$(id -u)/dev.elsecall.agent"  # background session
```

For foreground diagnosis, first unload the LaunchAgent to prevent competing
instances (use the `user/` domain for a background installation), then run `serve`:

```sh
launchctl bootout "gui/$(id -u)/dev.elsecall.agent"
elsecall serve
# After stopping the foreground process with Ctrl-C:
elsecall service restart
```

Do not delete lock files while a process is running. A crashed process releases its
OS lock automatically; the next service safely removes the stale socket. Logs
contain lifecycle events and errors, never environment dumps, request payloads
or secret values. `service.log.1` is the newest archive; all log files are mode
0600. `doctor` reports logical and allocated state bytes, per-entry sizes and
warnings for unusually large logs, databases or other state. Size warnings are
advisory and include retention guidance; they do not change API health. See
[State retention](docs/state-retention.md).

```sh
elsecall uninstall
```

Uninstall removes the owned CLI and LaunchAgent links, executable, configuration,
request records, receipt, socket, locks, current and rotated logs, member key, fleet roster and upgrade
record. While the
machine is enrolled in a fleet, uninstall refuses with `fleet_member` unless given
`--force`, because a reinstalled machine has a new key and must be enrolled again.
Shared parent directories and unrecognized files are preserved. If an installation
link was replaced by something else, removal
stops with a conflict instead of deleting it. Repeating uninstall after a clean
removal succeeds. Build outputs in this repository are separate from the installed
application.

## Development and verification

```sh
mise run check
mise run build
# Timing budgets, measured serially against same-run baselines:
mise run perf

# Direct verification without consumer admission:
cargo fmt --all -- --check
cargo clippy --workspace --locked --all-targets -- -D warnings
python3 scripts/test-environment.py cargo test --workspace --locked
```

The mise tasks use the consumer entry pattern in "Integrating a consumer".
The root `.elsecall.toml` sizes `elsecall check` and `elsecall build` from measured
CPU use and chooses `when_unavailable = "run"`. They run directly when Elsecall is
missing; `ELSECALL=off mise run check` uses the usual consumer escape hatch.
Tests require Python 3 to remove inherited `ELSECALL_*` controls (including
`ELSECALL_ADMISSION`) and `ELSECALL` before launching the test suite.
`ELSECALL_UI_SKIP_GOLDENS` and `ELSECALL_UI_BLESS` are kept; they select desktop
golden tests, not admission. Fixture subprocesses independently clear every
`ELSECALL_*` control before adding their own settings.

Normal checks exercise the performance fixtures and assert their functional
results. `mise run perf` also enforces timing budgets, one test at a time; run it
without synthetic CPU hogs or competing benchmarks. See
[test-reliability-slice.md](docs/test-reliability-slice.md) for the causes, baseline
rules, sizing measurements and repeated loaded-machine validation.

The test suite uses temporary homes and never registers a real LaunchAgent. It
runs real CLI/service processes and local sockets, and fakes only external machine
observation and service-manager boundaries where needed. Fleet tests run several real
services in separate temporary homes; `ELSECALL_TAILNET_NAME`,
`ELSECALL_TAILNET_ADDRESSES` (tailnet or loopback addresses only) and
`ELSECALL_FLEET_PORT` replace Tailscale discovery so loopback stands in for the
tailnet. For services that tests start, `ELSECALL_MACHINE_LOAD=idle` reports the
machine as idle, since its real load comes from unrelated work.
`ELSECALL_MACHINE_SPEED=1` fixes the speed observation for fixtures so their
startup does not launch competing CPU calibrations; the dedicated calibration
test still measures the real provider. Tests of placement override that speed.

`ELSECALL_SAME_PATH_BASE` puts same-path copies under a directory, since a member on
the caller's own machine would otherwise copy onto the caller's working tree.
`ELSECALL_HOUSEKEEPING_CLOCK` names a file holding the time housekeeping judges
idleness by, and `ELSECALL_SIMULATED_VOLUME` a file describing a volume
(`{"total_bytes", "used_elsewhere_bytes"}`) whose free space shrinks with the workspace
root. A service with either set logs `test_seam_in_use`. Upgrade tests install
real services with `ELSECALL_SERVICE_MANAGER=process`, which runs the installed
service as a plain background process instead of a LaunchAgent (logged as
`test_seam_in_use` too; debug builds only). Socket tests require an
execution environment that permits Unix sockets. macOS CI runs the same checks.
See [local-slice.md](docs/local-slice.md) for decisions, installed-system verification
and limitations, [admission-slice.md](docs/admission-slice.md) for admission, leases,
records and `run`, [fleet-membership.md](docs/fleet-membership.md) for fleet
membership, [remote-run-slice.md](docs/remote-run-slice.md) for runs on another
member, [consumer-kit-slice.md](docs/consumer-kit-slice.md) for label rules,
consumer controls and failure lines,
[housekeeping-slice.md](docs/housekeeping-slice.md) for housekeeping of copies, and
[config-sync-slice.md](docs/config-sync-slice.md) for configuration sync, and
[fleet-upgrade-slice.md](docs/fleet-upgrade-slice.md) for upgrading the fleet. Review is
submitted through the operator's Mergegate hub.

Optional typed workload declarations and grant-bound terminal observations are described in [the workload metadata contract](docs/workload-metadata-contract.md). They preserve existing estimation and placement behavior.
