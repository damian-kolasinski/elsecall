# S1: Admission on one machine

Delivers the S1 row of [Fleet scheduling](fleet-scheduling.md): capacity and
reserve, live local load sampling, leases and nested requests, interactive and
deferrable requests, local `elsecall run` with full I/O fidelity, failure codes and
the status file, request records, `runs` and `explain`.

## Decisions subsequent slices can build on

- **Ports.** `MachineInspector` gains `sample_load` (CPU utilization, memory
  pressure, thermal state, cores by type). A new `ProcessTable` port identifies
  processes by PID and start time, walks their ancestry and reports exits; the
  macOS adapter uses libproc and kqueue, so a SIGKILLed holder is noticed at once.
  `run` executes through an `Executor` port with an asynchronous `start` that
  also binds whatever runs the command to its grant. Caller-facing behaviour
  stays in `passthrough` and does not depend on the executor: streams, exit
  status, the 125 envelope and the status file. Admission always goes through
  the local service, and the grant names the member. `LocalExecutor` runs the
  command as a child and binds the child's PID. For a grant on another member,
  S3 adds an executor that copies the workspace and forwards streams and signals
  over its connection. Its `RunContext` is where the workspace paths of placed
  runs will go. `preflight` only checks commands that run locally.
- **Modules.**
  - `scheduling/` is the domain:
    - `admission` holds the pure queue and capacity ledger. `capacity` covers
      weighted cores and reserve; `load` the outside-load gate; `estimates` hold
      limits and retry delays.
    - `binding` maps processes to requests; `recovery` plans restart survival;
      `busy` builds the `fleet_busy` report.
    - `record`/`records` define the SQLite records. `service` orchestrates.
  - Driving adapters sit outside it: `admission_api` (HTTP routes),
    `admission_cli` (lease, runs, explain) and `passthrough` (run).
- **Capacity.** A performance core counts 1, an efficiency core 0.5, minus
  `[capacity] reserved_cores` from `config.toml` in the state directory. Sizes are
  held in thousandths of a core so accounting is exact. This Mac mini (10 P + 4 E)
  has 12.
- **Admission.**
  - The queue is strictly first come, first served within a class: a head that
    does not fit blocks those behind it, so large requests never starve.
  - Interactive work goes before deferrable work. Deferrable work promoted at its
    deadline goes ahead of interactive work.
  - Nothing new starts while outside load reaches 90% or memory pressure is
    `warning` or `critical`. Outside load is CPU utilization averaged over 3
    one-second samples, minus the share of weighted cores Elsecall's grants hold.
  - Nested requests skip the gate and the queue.
- **Leases and runs belong to processes.**
  - Leases:
    - A lease belongs to its holder: by default the parent of `elsecall`, or
      `--holder PID`, which must be an ancestor.
    - The service takes the requester's PID from the socket's peer credentials,
      never from the request body.
  - Runs:
    - A run belongs to its `elsecall` process and, once started, also to the
      command's process. So a SIGKILLed `elsecall` does not free capacity while
      its orphaned command still runs.
  - Waiting requests:
    - A waiting request is bound to its requester and holder, so either one
      exiting withdraws it.
    - So does a dropped connection.
    - A grant sent to a caller that disconnected is ended.
- **Nesting** is by process ancestry: any descendant of an active grant's holder
  reuses that grant at once, and the grant's capacity stays in use until every
  nested grant ends.
- **Clocks.** Hold limits and deadlines are timers, re-checked against the wall
  clock every second, because macOS timers stop while the machine sleeps.
- **Restart.**
  - On restart, grants whose processes still run are re-adopted, including nested
    families whose root ended, so capacity is never handed out twice.
  - On shutdown, queued requests fail with `service_unavailable` at once rather
    than holding connections open, which would block `service restart` and
    `install`.
- **Records.**
  - Records live in `records.sqlite` (WAL, mode 0600). Each is one JSON document
    per request, with indexed columns for listing, unfinished requests and
    run-time history.
  - They hold the request, the decision with every input (capacity, use, queue,
    load sample, gate, expected run time), timings, outcome, warnings and the
    holder processes.
  - The CLI never sends the service the command, its arguments or the
    environment, so records cannot contain them.
  - Ended records are pruned after 30 days.
- **Hold-limit estimate.** The default hold limit is ten minutes minus the median
  run time of the label's last 20 completed requests, at least 30 s. Without
  history it is ten minutes.
- **`run` failures** are checked before admission where possible: invalid options
  and a command that cannot be found fail at once, without waiting for capacity.
  Invalid options still write `--status-file`, so a stale file is never mistaken
  for this run's outcome. A status file that cannot be written after the command
  ran leaves the command's exit status, plus a warning line on stderr, rather
  than exit 125, so the command is never re-run because of it.
- **Signals.**
  - Signals that arrive while the command starts are kept as a set and delivered
    exactly once, by whichever of the handler and the spawning thread sees the
    command first.
  - The handler preserves `errno`.
  - Once the command is reaped, the default dispositions return, so a late
    signal never reaches a reused PID.

## Automated evidence

`mise run check` passes: formatting, Clippy with warnings denied, and the locked
test suite.

- Unit tests:
  - the admission engine: ordering, head blocking, promotion, the load gate,
    nesting capacity;
  - capacity parsing and weights, the outside-load gate, hold limits, the median
    and retry estimates;
  - request validation and CLI option parsing;
  - process bindings with a fake process table, including a reused PID and a late
    exit report;
  - the restart plan, records storage and pruning, the `fleet_busy` report;
  - the kqueue adapter, which sees a SIGKILLed child exit.
- `tests/admission.rs`: a real service on a real socket with the real process
  watcher, driven by the real CLI, with only machine load faked. It covers:
  - no overcommit under six concurrent requests;
  - arrival order, and interactive work before deferrable work;
  - `fleet_busy` after the hold limit, with retry hint and running work;
  - `request_too_large` and `invalid_request`;
  - saturation and memory pressure blocking new work;
  - deferrable skip, and start-ahead at the deadline without overcommitting;
  - lease release by command, holder exit and SIGKILL;
  - a caller killed while waiting;
  - nested requests admitted ahead of a queued outsider;
  - grants and nested families surviving a restart;
  - WAL and SHM file modes;
  - a caller that only disconnects;
  - the run endpoints' caller and child checks.
- `tests/shutdown.rs`: the service stops within 5 s while a request is queued,
  and the waiter gets `service_unavailable`. The test fails without the fix.
- `tests/run.rs`:
  - a 300 KB binary stdin-to-stdout round trip with separate, byte-exact binary
    stderr and no extra output;
  - `2>&1` keeping order;
  - exit codes;
  - the command's own 125 told apart from Elsecall's `fleet_busy` 125 by the
    status file;
  - invalid options and unknown commands exiting 125 without running;
  - the service down: 125 with `service_unavailable`, `run_id` null, and the
    command not run;
  - SIGINT and SIGTERM reaching the command's traps, and Elsecall dying by the
    command's signal;
  - `ELSECALL_RUN_ID` and `ELSECALL_MEMBER`;
  - a nested run inside a capacity-1 run;
  - the placeable note;
  - a SIGKILLed caller whose command keeps its capacity until it exits;
  - records containing no arguments and no environment values;
  - the status file's `running` state;
  - a stale status file replaced on a usage error;
  - SIGINT ignored by a background job staying ignored in the command;
  - exit 139 for a command killed by SIGSEGV;
  - the envelope's key order.
- `tests/admission.rs` also checks that Elsecall's own grants do not count as
  outside load through the running service.

## Real-machine evidence

On 2026-11-02 this Mac mini (macOS 26.4, M4 Pro, 10 performance and 4 efficiency
cores) ran the release build. While this was done, the parallel S2 session had
its own build installed as the login service. Replacing it would have broken that
session, and a second home cannot share the LaunchAgent label. So the release
build's `elsecall serve` ran in the foreground under an isolated home, on the real
machine with real load sampling. `elsecall install` of this build is left for the
operator (see "Limits").

Four concurrent `elsecall run --size 5 --label "demo build" -- sh -c '…; sleep 3'`
calls on a capacity of 12. Two started, and two queued until the first pair ended:

```text
== member while queued
in_use 10.0 of 12.0 queued {'interactive': 2, 'deferrable': 0} gate open
build 1 started as r-95d3eaf37ebb on mac-mini at 16:14:27
build 2 started as r-8697f23e597e on mac-mini at 16:14:27
build 3 started as r-29b3a911d296 on mac-mini at 16:14:30
build 4 started as r-c66af908796d on mac-mini at 16:14:30
(each elsecall exited 0; wall time 6.4 s for four 3 s builds)
{"schema_version":1,"data":{"state":"exited","run_id":"r-c66af908796d","member":"mac-mini","exit_code":0}}
```

`elsecall explain r-c66af908796d`, abridged:
- It waited 2,624 ms and ran 3,119 ms.
- Its default hold limit was 596.9 s, learned from an earlier run of the same
  label (`expected_run_seconds: 3.065`).
- The decision recorded `in_use 5.0, free 7.0`, one request still queued,
  utilization 0.35, a 3-sample window average of 0.25, memory pressure `normal`,
  thermal state `nominal`, and gate `open`.

With another script holding all 12 cores, a run with `--hold-limit 1s` exited 125.
Its last stderr line was:

```json
{"error":{"code":"fleet_busy","details":{"retry_after_seconds":5,"running":[{"expected_remaining_seconds":0,"kind":"lease","label":"hold all","run_id":"r-6de8b5a855d9","running_seconds":1,"size":12.0}]},"hint":"Retry in about 5 s, request a smaller --size, or run it as --deferrable with a --deadline.","message":"No capacity within the 1 s hold limit: 0 of 12 cores are free and 1 requests wait.","run_id":"r-bc13b465e2d7"},"schema_version":1}
```

The message's request count included the failing request itself. It now counts
only the other requests waiting.

## Limits

- Not covered by tests:
  - a signal arriving between spawn and forwarding (the pending set);
  - the wall-clock re-check of hold limits after machine sleep.
- Leases still wait without a client-side bound when deferrable.
- macOS only. The login service of this build was not installed on the operator's
  Mac (see above). To install it: `mise run build && ./target/release/elsecall install`.
- Signals:
  - A terminal Ctrl-C or a signal sent to the whole process group reaches the
    command directly and again through Elsecall. Identical standard signals that
    arrive together are delivered once, but this is not guaranteed. On macOS,
    `si_code` is 0 for both kill(2) and terminal keys, and a probe under a pty
    saw a terminal Ctrl-C with a nonzero `si_pid`, so the two cannot be told
    apart. Running the command in its own process group would make forwarding
    exact, but then a harness that SIGKILLs the caller's process group would
    leave the command running.
  - Rust resets SIGPIPE to its default and clears the signal mask in the child.
    Signals the caller ignores stay ignored.
- If the service restarts while requests wait, those requests fail with
  `service_unavailable`; their callers must request again. Running grants survive.
- Record writes happen under the admission lock; a stalled disk stalls admission.
- The member name is the host name up to its first dot until S2 defines member
  names. `--member` compares it case-insensitively.
- Outside load is estimated, not measured per process: Elsecall assumes its own
  work uses what it was granted.

## Contract questions

The most literal reading was implemented in each case.

1. **Core-type weights** are not specified. Chosen: performance 1, efficiency 0.5.
2. **Outside load.** The contract asks for load "Elsecall did not start". That
   cannot be measured without inspecting processes, so it is estimated as
   utilization minus Elsecall's granted share. The thresholds are also chosen:
   90% over 3 s, and memory pressure `warning` or worse.
3. **Strict first come, first served** leaves capacity idle behind a large head.
   Backfilling without delaying the head needs run-time estimates (S4). Should S4
   relax this?
4. **Nested requests larger than their outer grant** run within it and carry a
   warning. Any request from a lease holder itself is nested too, so a long-lived
   shell holding a lease (`--holder $$`) bypasses admission for everything it
   starts. Should nested requests be capped, queued or charged for the
   difference?
5. **Deferrable work promoted at its deadline** has no hold limit afterwards; it
   waits until it fits.
6. **`--at-deadline`** has no default; "needs a deadline and what to do at it" was
   read as both required.
7. **Default placement of `run`** is not specified. Chosen: pinned, so existing
   callers do not move to other members once S4 lands. Agents opt in with
   `--placeable`.
8. **A command that cannot be found** is "a command that did not run". It exits
   125 with `invalid_request`, before admission. There is no dedicated code.
9. **Cancellation codes.** A request whose caller or holder exits while waiting
   fails with `cancelled`, which the failure-code table lacks. A request queued
   when the service stops gets `service_unavailable`.
10. **Error envelope.** `fleet_busy` adds `details`
    (`retry_after_seconds`, `running`) beside `code`, `message`, `hint` and
    `run_id`. `run_id` is always present in `run`'s envelope, null without a
    record.
11. **Signal deaths.** "The command's exit code is Elsecall's exit code". For a
    command killed by a signal, Elsecall dies of the same signal, so the caller's
    shell sees the same status. Core-dumping signals instead exit with 128 + the
    signal number, to avoid crash reports for Elsecall.
12. **The status file schema** is not specified. Chosen: `data.state` is
    `waiting`, `running`, `exited` (with `exit_code`) or `signaled` (with
    `signal`). Elsecall's own failures write the error envelope instead.
13. **Hold-limit floor.** The 30 s floor applies to the default only; an explicit
    `--hold-limit` may be shorter, including `0s`.
14. **Shared identifiers.** Leases and runs share one ID space and one record list;
    a lease's grant reports its ID as `run_id`.
