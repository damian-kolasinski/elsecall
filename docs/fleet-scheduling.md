# Fleet scheduling

Elsecall runs a caller's command on the fleet member best able to run it now, holds
work that cannot run yet, and tells consumers when machine-bound work should start.
This document fixes **how it behaves**. Implementation slices choose how to build it
within these rules. A change to anything here is a product decision and updates this
document first.

## Problem

Agents start heavy builds and tests from many concurrent sessions, and nothing
coordinates them. Measured over three weeks on the operator's two machines (Mac mini
M4 Pro with 14 cores; MacBook Pro M1 Max with 10 cores, the "devbox"):

- One machine ran up to 19 heavy operations at once and reached load average 334.
  Meanwhile the other was often nearly idle.
- Times grew with concurrency. A typical app build took 41 s alone and 148 s with 7
  or more others running; the slowest tenth of those took over 23 minutes.
- Overload caused wrong results, not only slow ones: timeouts, simulator contention
  and timing-sensitive tests failing "under load".
- Most heavy runs (78%) worked on uncommitted changes in per-task git worktrees, and
  they reused warm per-worktree build state between runs.

Success means:
- load stays low and balanced;
- the strongest machine is used;
- the operator's interactive machine stays responsive;
- adopting Elsecall needs no workflow change from its consumers.

## Principles

- **Generic.** Elsecall knows about commands, git working trees, machines and
  capacity. It never knows about a consumer's build system, simulators or test
  layers. Consumer-specific knowledge lives in the consumer's configuration and
  scripts.
- **Transparent.** A command run through Elsecall behaves as if run locally. It
  reads the same input, produces the same output and exit code, and sees the same
  paths wherever the member can host them (see "Workspace"). Elsecall never
  rewrites a command's input or output.
- **Cheap to adopt.** Existing scripts start using Elsecall by adding one line or one
  prefix. Callers keep their pipes, redirects and timeouts.
- **One operation.** From the caller's view, a run is a single blocking call that may
  take longer because it waited. It is attached to its caller, not a durable job;
  durable jobs are a later feature.
- **Explainable.** Every decision is recorded with the inputs that produced it, so
  agents can debug and tune scheduling.

## Roles

- **Member:** a machine running the Elsecall service and enrolled in the fleet.
  Every member admits work onto itself and reports its state.
  - A member's service runs in one account on that machine. That account need not
    be the one with the desktop login, and need not have the same name or home as
    the accounts on other members.
  - The service keeps running without a desktop login, across ssh disconnects, and
    after the account's processes restart.
- **Master:** exactly one member at a time.
  - Only the master places work onto other members.
  - A placeable request made on any other member runs on that member under local
    admission, and its record says so.
- **Recognising the master.** Each member records which enrolled member is master.
  It accepts placed work only from that member's enrolled key.
- **Moving the role.** The master role moves only through an explicit operator
  command; there is no automatic election.
  - The command updates every member.
  - It refuses while any member is unreachable, unless the operator forces it. A
    forced move leaves the unreachable member trusting the old master until it is
    updated.
  - Enrolling and removing members are also explicit operator actions.

## Work requests

Every use of Elsecall is a work request. The caller describes it, and consumer
configuration supplies defaults.

- **Precedence.** Each property comes from the first layer that sets it: the
  caller's command-line flags, then the caller's environment, then the first label
  rule of the consumer configuration that matches the request's label, then
  defaults.
- **Provenance.** The record of every request names the label rule that applied
  and where each property came from.

| Property | Values | Meaning |
| --- | --- | --- |
| Placement | `pinned` | Must run on the calling machine. |
| | `placeable` | May run on any member that can host its workspace. |
| | required member | Hard requirement to run on a named member (a placeable request only). |
| Urgency | `interactive` | A caller is waiting. It waits for capacity up to a hold limit, then fails. |
| | `deferrable` | Should run, but not while the machine is busy. Needs a deadline and what to do at it (start, or skip). Pinned only for now. |
| Size | cores | How much of a member the work occupies while it runs (see "Capacity"). |
| Label | consumer-chosen text | Identifies the kind of work (e.g. `test FeatureModule`) for history, cost learning and diagnostics. |
| Resources | (later) | Named resources the work needs; see "Later". |

Asking for a deferrable placeable request, or a required member on pinned work, is
`invalid_request`.

Examples of how requests map:

| Work | Request |
| --- | --- |
| A test or build an agent is waiting for | placeable, interactive |
| A heavy section inside a script already running on this machine | lease (pinned, interactive) |
| A cache warm started by the machine's own scheduler | pinned, deferrable with a deadline |
| A follow-up step that must reuse state left on one machine | placeable with a required member |

### Entry points

- **Run a command.** The caller prefixes a command: `elsecall run [options] -- <command>`.
  Elsecall admits it, places it if it is placeable, and runs it to completion.
  Its output passes through unchanged (see "Input and output").
- **Hold capacity.** A script that is already running asks for capacity for a section of
  its own work. The grant is a **lease**:
  - It has a size and a hold limit, and it is always pinned.
  - It belongs to the process that asked for it, by default the parent of the
    `elsecall` command invoked. It is released explicitly or when that process
    exits, so a killed script never leaks capacity.
  - A holder that already holds an active lease and asks again, such as a script
    that re-execs itself for a retry, gets that same lease back at once. Nothing
    queues and no new request is recorded. Acquires are not counted: one release
    ends the lease. A section of the same process therefore shares and ends the
    outer lease. A separate nested lease needs a child process.
- **Ask when.** Deferrable work uses the same entry points with a deadline. Elsecall
  decides **when** it may start. Where it runs stays the consumer's choice.

### Consumer controls

- **Off switch.** With `ELSECALL=off` in the caller's environment, Elsecall steps
  aside: a run executes its command directly with exact pass-through, adding only
  the admission marker to its environment, and lease requests and releases
  succeed without doing anything. It serves consumers' tests and emergencies.
  Request properties and consumer configuration are still validated.
- **Running anyway.** A consumer may choose, through its configuration, the
  caller's environment or a flag, that work whose local service is unavailable
  proceeds unmanaged (see "Input and output"). The default is to fail with
  `service_unavailable`.
- **Hold limit default.** The caller's environment may set the hold limit of
  interactive requests that set none.
- **Run IDs.** A caller may ask for the run ID as plain text in a file, to
  correlate its own telemetry with Elsecall's records without parsing JSON.
- **Admission marker.** Every command a run starts sees one variable saying how
  it came to run: with a grant of its own, nested in other work, unmanaged, or
  off. It is set in every mode, on the member a run was placed on too, and
  replaces any inherited value. A script can therefore re-exec itself through
  Elsecall at its entry exactly once, and run cheap checks only before that
  re-exec. Elsecall never reads the marker: nesting follows processes. Leases set
  nothing, since a lease cannot change its caller's environment and covers the
  caller where it already runs.
- **Finding Elsecall.** The CLI is always reachable at `~/.local/bin/elsecall`.
  Consumers that may run where Elsecall is not installed guard with a few lines of
  shell that run the work directly, with the marker saying Elsecall is missing;
  the README gives the guard and the entry line.

### Nested requests

A request made by a process that already runs inside a lease or a run reuses the
outer request's grant:
- it is admitted at once;
- it runs on the same member;
- it does not queue behind its own parent.

This covers scripts that call other scripts, and a placed command whose script asks
for a lease on the member it was placed on.

## Capacity

- **Units.** Capacity and size are counted in cores. A member's capacity is its core
  count, weighted by core type, minus its **reserved capacity**. A size means the
  same number on every member; differences in speed are placement's concern.
- **Reserved capacity.** Each member has its own reserve setting, independent of
  which member is master. The operator's interactive machine keeps a reserve so it
  stays responsive.
- **Default size.** Without a declared or learned size, a request has size 1.
- **Too large.** A request larger than every eligible member's capacity fails at
  once with `request_too_large`.

## Admission

Each member's service owns that member's capacity. Every request that runs on a
member is admitted by that member.

- **No overcommit.** A request starts only if its size fits the free capacity
  left by admitted work. Work admitted to a member keeps the capacity it was
  admitted with for its whole run. Timing-sensitive work therefore runs in the
  conditions it was admitted under, as far as admitted work goes. Processes a
  placed command leaves running after it exits are not admitted work: they count
  as outside load, and are capped (see "Leftovers").
- **Outside load.** Admission accounts for load Elsecall did not start, using live
  samples of the member (see "Machine state"). A member saturated by outside work,
  or under memory pressure, admits nothing new.
- **Order.** Interactive work goes before deferrable work. Within an urgency class,
  work goes first come, first served. A large request is not starved by a stream
  of small ones.
- **Where requests wait.**
  - Local requests wait on their own member.
  - Placed requests wait on the master.
  - A member offered a placed request either starts it at once or refuses it, so it
    can go elsewhere. A member refuses placed work while its own interactive
    requests are waiting. Its own deferrable work, which waits behind interactive
    work, does not keep placed work out.
- **Hold limit.** This is the longest an interactive request may take from request
  to start, including workspace sync.
  - Default: ten minutes minus the label's expected run time, with a floor of 30
    seconds, so a run fits typical agent tool-call limits.
  - A request whose estimated wait already exceeds its limit may fail early.
  - A request that cannot start in time fails with `fleet_busy`. The error includes
    a suggested retry delay estimated from running work, what is running, and what
    the caller can do next.
- **Deferrable work.**
  - It starts only when the member has capacity beyond its reserve and no
    interactive work is waiting.
  - At its deadline, work that chose to start moves to the front of the interactive
    queue. It still never overcommits.
  - Work that chose to skip ends with `deadline_skipped`.
  - The outcome is recorded either way.

## Placement

The master places placeable requests. It picks the member with the **earliest
expected completion**, considering:

- each member's relative speed, measured rather than assumed;
- current free capacity and outside load;
- expected run time for the request's label on that member, learned from history;
- **workspace warmth**: a member that already holds a warm copy of the caller's
  workspace saves setup and cold builds;
- each member's reserved capacity.

A required member overrides the choice. A request that no member can start before
its hold limit fails with `fleet_busy`. It is never sent somewhere already saturated.
A member whose synced configuration cannot be brought up to date before the run is
left out of that request, which is placed again within its hold limit; the calling
member needs no sync, so placeable work is never worse off than running locally.

The command learns where and under which run it executes through `ELSECALL_*`
environment variables: the member name, the run ID, and for placed runs the
caller's workspace path and the copy's path. A consumer can use these to ask for the
same member in a follow-up request.

### Machine-bound values

Some values a caller passes bind the work to one machine, such as a local simulator
or device identifier. Elsecall does not interpret them. Consumers should not pass
machine-local identifiers to placeable work unless they mean to bind it. They state
that binding with a required member. Consumer guidance should say this.

## Workspace

A placeable run's workspace is the git working tree containing the caller's working
directory. Placeable work requested outside a git working tree runs locally, and its
record says so.

- **Location.** The copy keeps the caller's working tree layout. The command starts
  at the same relative position in it as the caller's working directory. Each
  member's **workspace location** setting decides where its copies live:
  - **Same path:** the copy lives at the caller's absolute path. Absolute paths in
    output and in build state match the caller's. A member uses this only when its
    account can own the calling member's paths.
  - **Mapped:** the copy lives under the member's workspace root. Its location
    there is derived deterministically from the calling member and the caller's
    absolute path, so the same workspace always lands at the same place and stays
    warm.
  - Elsecall does not rewrite output for mapped copies. Absolute paths the command
    prints name the member's copy. Relative paths and declared outputs are
    unaffected.
  - The command receives the caller's workspace path and its own copy's path as
    `ELSECALL_*` variables. A consumer that needs caller paths in its output can
    use them.
  - Changing a member's setting, for example to same path once its machine is set
    up for that, needs no other change. Mapped is the default.
  - Placed commands run as the member's Elsecall account, which owns its copies.
- **Same state.**
  - The copy is a git working tree at the same HEAD as the caller's, with the same
    staged state.
  - It carries the caller's uncommitted changes: modified, deleted, and untracked
    files that git does not ignore.
  - Commits that exist only locally are carried too.
  - It carries the caller's remote-tracking refs that scripts compare against, as
    remote-tracking refs with the caller's names:
    - the upstream of HEAD;
    - each remote's default branch: `refs/remotes/<remote>/HEAD` and the branch it
      points to, such as `origin/main`;
    - refs the consumer configuration declares, under `refs/remotes/` or
      `refs/tags/` only.

    They point at the same commits as on the caller at sync time and follow it on
    every sync, with their history. Other remote branches and tags are not
    carried. So `rev-parse`, `merge-base`, `rev-list` and `log` against them give
    the caller's answers.
  - A remote's URL never reaches the copy, so `git fetch` and `git push` in it
    fail. Fetching is the caller's.
  - No commit, push or other git ceremony is required of the caller.
- **The caller is the source of truth.** Every run first makes the copy match the
  caller's working tree. Changes the remote run makes to the copy are not brought
  back, except declared outputs.
- **One run per copy.** Only one run at a time uses a member's copy of a given path.
  Another run for the same path is placed elsewhere or waits for the copy.
  Leftovers (below) do not keep the copy busy for placement, but they may still be
  writing in it: the next run of the copy waits up to 10 seconds for them to end,
  then stops them, before it syncs. So the copy always matches the caller when a
  run starts, and a warm copy is not given up for work nobody waits on. The wait
  counts toward the next run's hold limit and is recorded with it. Housekeeping
  never removes a copy while leftovers run in it.
- **Leftovers.** Processes a placed command leaves running in its process group
  when it exits keep running on the member, as they would after a local run.
  - The caller gets its exit and outputs at once; the run's capacity is released
    when it ends, as for any run.
  - They are not admitted work. The member's load sampling sees them as outside
    load, which holds back new admissions while it saturates the member.
  - They are stopped at a cap, 10 minutes after the exit by default and
    configurable per member, whether or not the caller is still connected: a
    termination signal to the group, then a kill after 5 seconds.
  - A service restart stops them, as it stops placed commands: the service
    that starts again stops what the one that stopped left running.
  - The member's record keeps how long they ran after the exit, the CPU time they
    used then, and how they ended: on their own, at the cap, for the next run of
    the copy, or at a restart. Reports name labels whose leftovers are heavy.
  - Output they write to the command's streams once its own output has drained is
    dropped, so they never fail writing it.
  - A process that leaves the group, by starting its own session or group, is
    not followed, as locally.
- **Persistent and warm.** Copies stay on the member between runs and keep the
  build state that git ignores, so repeated runs stay incremental. Copies are
  readable only by their owner. Housekeeping cleans them up, never mid-run.
- **Ignored files.** Files that git ignores, such as machine-local configuration and
  secrets, are not copied. They belong to the target machine or to the command's
  own setup, just as when a fresh worktree is created locally. Consumers keep
  secret files ignored.
- **Outputs.** Consumer configuration declares which produced paths come back to the
  caller, e.g. logs and result bundles agents read afterwards.
  - Output paths are relative to the workspace. They never resolve outside it,
    including through symbolic links.
  - Declared outputs come back whenever the command exits, whatever its exit code,
    and only if they changed.
  - **Snapshot at exit.** The moment the command exits, the member takes the
    changed outputs as they are into a staging area private to the run: APFS
    clones, or copies where the volume cannot clone. What comes back is that
    snapshot, so what processes the command left running do to the outputs
    afterwards does not reach the caller. The staging area is removed once the
    outputs are returned, or the run ends.
  - **Best effort.** Every declared output in the snapshot comes back. A path
    that cannot come back never keeps the others from coming back. A directory
    still replaces the caller's whole, without that path, so the caller's own
    file there is gone too; a single file that cannot come back leaves the
    caller's version as it was. The command's exit code is unchanged.
  - **Causes recorded.** Each output, or path inside one, that did not come back
    is named with its cause in both members' records (`elsecall explain`) and in
    the status file's warnings, never in the command's output. The causes are:
    vanished while the snapshot was taken, unreadable, outside the workspace (a
    directory on its way is a link), a link or special file, too large for what
    Elsecall returns from one run (16 GiB or 200,000 entries), and a transfer
    error. Many paths of one output with one cause make one warning.
  - They do not come back after a lost connection.

## Input and output

- **Streams.** stdin is forwarded. stdout and stderr come back as separate streams,
  byte for byte. When the caller has pointed both at the same destination, their
  relative order is preserved.
- **Exit code.** The command's exit code is Elsecall's exit code.
- **Signals.** An interrupt or termination of the caller is forwarded, so the
  command stops wherever it runs.
- **No extra output.** Elsecall writes nothing to stdout or stderr on a successful
  pass-through, except the single notice of unmanaged work below.
- **Elsecall's own failures.** These cover invalid arguments, a command that did not
  run, or one whose outcome is unknown.
  - Elsecall exits with code **125**.
  - It writes one plain **failure line** on stderr,
    `elsecall: <code>: <what happened> <what to do>`, so consumers need not format
    their own. For `fleet_busy` it says to wait about the estimated retry delay
    and then rerun the same command once, that `elsecall runs` shows what is
    running and queued, and that `elsecall explain <run_id>` shows why.
  - As the last line on stderr, it writes the standard error envelope:
    `{"schema_version":1,"error":{"code","message","hint","run_id"}}`.
  - Because a command can exit 125 itself, a caller can ask for the outcome to be
    written to a status file, which tells the two cases apart.
  - A lease request that fails writes the same failure line on stderr; its
    envelope is the JSON document on stdout.
- **Unmanaged work.** When the local service is unavailable and the consumer chose
  to run anyway, a run executes its command directly with the same pass-through,
  after exactly one notice line on stderr,
  `elsecall: unmanaged: <reason> The command runs without admission. …`. A lease
  request succeeds in an `unmanaged` state that holds nothing. Unmanaged work has
  no record. Work required on a named member never runs unmanaged, since it is
  bound to that member.
- **Failure codes.**

  | Code | Meaning |
  | --- | --- |
  | `invalid_request` | Invalid arguments or request properties. |
  | `service_unavailable` | The local Elsecall service is down. The command does not run, unless the consumer chose to run anyway (see "Unmanaged work"). |
  | `fleet_busy` | No capacity within the hold limit. |
  | `request_too_large` | Larger than any eligible member. |
  | `deadline_skipped` | Deferrable work skipped at its deadline. |
  | `member_unavailable` | A required or chosen member cannot be reached. |
  | `workspace_sync_failed` | The workspace could not be copied. |
  | `config_sync_failed` | The synced configuration of a required member, or of the only member placeable work could run on, could not be brought up to date before a placed run; the command did not run. Placeable work with another candidate left, the calling member always included, is placed again without that member instead. |
  | `connection_lost` | Contact with the running command was lost. |

- **Lost connection.** Either side treats silence longer than a short grace period
  (seconds) as a lost connection.
  - The caller fails with `connection_lost`.
  - The member stops the command and its whole process group at once.
  - The record states what is known about how far the command got.
  - Once the caller has the command's exit, the connection no longer decides how
    long leftovers run: the cap does.
- **Environment.**
  - Pinned work inherits the caller's full environment.
  - Placed work receives variables matching a consumer allowlist of names and
    prefixes.
  - A fixed list of machine-identity variables, including HOME, USER, PATH and
    TMPDIR, always comes from the target, even when allowlisted.
  - Elsecall sets and replaces its own `ELSECALL_*` variables, and otherwise never
    changes the caller's arguments or variables. Unmanaged and off runs set only
    the admission marker (see "Consumer controls"). A caller's `ELSECALL_*`
    variables are never forwarded to placed work.
  - Variable values are never logged or persisted.
- **Contract exception.** `run` is the one CLI command that does not print a JSON
  document on stdout, because its output is the command's.

## Consistency warnings

Members can drift apart in tool versions (e.g. Xcode) and in machine-level consumer
configuration. Before a placed run, Elsecall compares fingerprints of:
- toolchains the consumer declares;
- configuration locations the consumer declares.

It compares fingerprints, never values. A mismatch is a **warning**, and the run
proceeds. Since E2 ([pre-run-sync-slice.md](pre-run-sync-slice.md)), the master
first brings the member up to date with every synced path, so a declared location
within a synced path is current by construction and no longer compared; other
locations and toolchains still warn. Warnings appear in the run's record, in `elsecall fleet` and in the status
file, never in the command's output. Enforcement comes later, once Elsecall can
align tools and configuration across the fleet.

## Consumer configuration

A consumer repository holds a configuration file at its root, `.elsecall.toml`. It
declares:
- output paths to bring back;
- environment variables to forward;
- toolchains and configuration locations to fingerprint;
- whether work proceeds unmanaged when the local service is unavailable;
- **label rules**: ordered rules, each matching labels with a glob pattern, of
  which the first match applies. A rule may set size, urgency, hold limit,
  deadline, the action at the deadline, and placement (pinned or placeable).
  Rules apply to runs and leases; a lease stays pinned. The caller's flags
  override them.

Invalid label rules fail every request in that working tree with
`invalid_request`, naming the rule and the change that fixes it.

The file is the only place consumer-specific knowledge enters Elsecall. It is
validated as untrusted input. Without a file, defaults apply: pinned commands work,
and placed commands bring back no files and receive no variables beyond the target's
own and Elsecall's.

## Machine state

Each member reports:
- CPU utilization;
- load averages;
- memory pressure;
- thermal state;
- core counts by type;
- capacity in use by Elsecall leases and runs;
- warm workspaces it holds.

Samples are live and include outside work. Admission uses them on every member,
placement uses them on the master, and `elsecall fleet` shows them.

## Observability

Every request produces a durable record on the members involved:
- the request: label, placement, urgency, size and calling member;
- the state of each candidate member at decision time;
- the expected completion per candidate and why the winner won;
- time waited;
- sync time and size;
- run time;
- exit code or failure code;
- consistency warnings;
- for a placed run, on the member: what the command left running after it exited,
  how long it ran, its CPU time and how it ended.

Records never contain command arguments, environment values or secret material.
The label is the consumer's chosen description.

- `elsecall runs` lists recent records.
- `elsecall explain <run>` shows one decision in full, with the parameter values it
  used.
- Records feed the learned run-time estimates, and later a replay that re-scores
  past decisions under changed rules. Agents can then improve scheduling against
  real history. `elsecall replay` does this for placement decisions under changed
  `[placement]` and `[admission]` values ([Tuning](tuning-slice.md), "Replay").
- **Load history.** Each member keeps a per-minute summary of its own machine state
  (utilization, outside load, Elsecall's use, queue, gate, speed), including time
  when no Elsecall work ran, for as long as records. It holds no processes,
  arguments, environment values or paths.
- **Tunable parameters.** Placement's margin and priors, the load gate's thresholds
  and how much history estimates trust are operator settings with documented
  defaults. The master's settings govern placement; each member's gate its own
  admission.

## Security

- **Network.** Members listen only on the tailnet.
- **Identity.** Members identify each other with keys enrolled per machine. Being
  on the tailnet is not enough.
- **Placement authority.** Only the recorded master can place work on a member. The
  local socket keeps its same-user rule.
- **Untrusted input.** Placed requests and their workspace data are validated as
  untrusted input on arrival. Returned outputs are validated as untrusted input on
  the caller.
- **Secrets and source.** Workspace copies contain source code but no ignored
  secrets. Forwarded variable values exist only in transit and in the running
  command.

## Later

- **Named resources.** A request may need named resources with capacities. Some are
  attached to a member, such as a physical device, which then decides placement.
  Others are fleet-wide, such as a shared test account. Collisions on shared
  accounts are assumed absent for now.
- **Same-path hosting setup.** Elsecall can help prepare members so they host
  callers' paths unchanged, for example a shared workspace root owned by each
  machine's agent account. Until then, members whose account cannot own the
  caller's paths use mapped locations.
- **Secrets and environment sync** across members. This is a separate track; it
  removes configuration drift, which consistency warnings only report.
- Deferrable placeable work, durable runs that outlive their caller, moving
  unfinished work, and splitting one request into many.

## Delivery

Each step is one reviewable item, verified before the steps that depend on it begin.

| Step | Delivers | Depends on |
| --- | --- | --- |
| S1 Admission on one machine | Capacity and reserve, live local load sampling, leases and nested requests, interactive and deferrable requests, local `elsecall run` with full I/O fidelity, failure codes and status file, request records, `runs` and `explain` | this document |
| S2 Fleet membership | Tailnet listener, enrolment and identity, master role and moving it, fleet-wide machine state, `elsecall fleet`, a service that runs without a desktop login, installation on the second member | this document |
| S3 Remote run on a required member | Workspace copy with mapped and same-path locations, output return, environment forwarding, consistency warnings, lost-connection handling | S1, S2 |
| S4 Placement | Earliest-completion placement, learned run times and sizes, warmth, fleet-wide `fleet_busy` | S3 |
| E Secrets and environment sync | Decided separately before it starts. E1, mirroring declared configuration and secret files from the master, is [config-sync-slice.md](config-sync-slice.md); E2, syncing a placed run's member before it starts, is [pre-run-sync-slice.md](pre-run-sync-slice.md) | S2 |
| C Consumer integration kit | Label rules with placement, running anyway when unavailable, failure lines, lease reuse for the same holder, the off switch and environment defaults, run IDs for consumers' telemetry, the admission marker and the entry line | S1 |

S1 and S2 can run in parallel. Consumer adoption happens in the consumer's own
repository and follows the step it needs.
