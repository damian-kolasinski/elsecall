# Test reliability and Elsecall as its own consumer

This is one deliverable: reliable verification on a busy development machine,
with the same consumer admission used by other repositories. No installation or
configuration on either production member changes.

## Causes and fixes

- **Startup and observation budgets.** Starting many debug services used a short
  readiness allowance while each service reads its executable and starts its
  runtime. Fixtures now poll the readiness API for up to 120 seconds, reporting
  early process exit immediately. Other state/file/exit waits use that same
  scheduling allowance. Operations themselves run once; there are no assertion
  retries. The existing free-port retry is still limited to a bind collision.
- **Calibration contention.** Test members now fix speed at the existing external
  observation boundary, with explicit placement-test overrides still taking
  precedence. Previously every member ran the real high-priority CPU benchmark
  during startup; concurrent calibrations competed with two-second peer I/O.
  A full CPU-hog run reproduced this as a fleet-join `member_unavailable` in
  `a_forced_move_makes_the_new_masters_replica_the_truth`. Calibration itself keeps
  its dedicated real-provider unit test. Installed-copy fixtures use the same
  fixed observation, and the poisoned-environment regression proves it applies.
- **Runtime oversubscription.** Each concurrent async test and each fixture
  service previously created a worker per logical CPU. Async integration tests
  and unit tests, plus fixture subprocesses, use two workers, while the test
  harness still runs tests concurrently. This limits test-created contention without serializing
  business tests or changing production runtime settings.
- **Housekeeping.** Command exit can reach the caller before the member releases
  its workspace claim. The replaced-directory test waits for the copy to become
  idle before replacing it. The in-flight-sync test now gates a real transport
  after receiving the member's reply, proving the copy is open and checked before
  housekeeping runs; a 500 ms sleep did not establish that condition.
- **Output loss in `many_skipped_links`.** The member inferred quiet stdout from
  elapsed wall time before its output reader necessarily ran. A delayed reader
  could discard buffered output, including the printed run ID. Output draining
  now belongs to the pipe reader. Exit wakes it; EOF or an actual empty pipe
  after the quiet interval ends forwarding. A final nonblocking read also covers
  delayed I/O readiness delivery. Flow control remains active, and leftovers
  retain open, drained pipes. A paused-time real-pipe regression covers both
  streams, readers first scheduled after exit, and readers descheduled past an
  already-started quiet timer. The real fleet warning/exit test keeps its
  immediate assertions; it does not poll until a lost output looks successful.
  The output-snapshot test also replaces a 500 ms deletion race: bounded stdout
  backpressure keeps the snapshot alive, the test observes the cloned file,
  releases a real leftover to delete the original, then drains stdout and proves
  the cloned output still returns.
- **Disconnect and restart races.** The dropped-client test first proves its
  request queued, then drops the connected future. The restart fixture waits
  until the old service lock is actually released: a concurrent fork may retain
  the close-on-exec descriptor briefly after the service task is cancelled.
  It never retries a failed service startup.
- **Load sampling.** Admission's own-load test waits for three observations from
  the machine-inspection fake, covering the averaging window rather than assuming
  three scheduled ticks fit in 3.5 seconds.
- **Sync contention.** Pausing a member before the authenticated handshake raced
  the handshake timeout. A transparent loopback TCP gate now forwards the full
  signed upgrade and waits for real encrypted response bytes before holding one
  sync exchange. All other connections flow normally. This proves another member
  progresses while the exchange stays held. Serialization is proved by a second
  request reaching its hold limit without another config-sync exchange reaching
  the proxy; after release, a run sees the current generation and exactly one
  push was applied. No ciphertext is decoded or logged by the gate.
- **Copy exclusivity.** The old test measured elapsed time after spawning both
  runs, so descheduling could consume part of its assumed two seconds. It now
  holds the first command explicitly, proves a competing request is refused for
  the busy copy, releases the holder, then proves the next command succeeds.
  Readiness and release waits are bounded and clean up their children on failure.
- **History and CPU assumptions.** Family-estimate tests hold real commands on
  stdin and compare learning with the measured training median, handling a run
  that outlives that median during descheduling. Other family/ratio tests compare
  estimates with recorded command medians and their learned conversion, rather
  than ceilings guessed from sleeps. A scripted quarter-core destination makes
  the learned-size clamp unavoidable even when contention teaches the minimum. Size-learning runs use two
  waited children with measured 250 ms CPU budgets, followed by the minimum
  sample duration, so descheduling cannot invalidate their CPU-accounting floor.
  A trailing shell builtin preserves the waited children's accounting instead
  of replacing the shell with the final sleep. Process-accounting tests gate
  startup/finish and observe a real CPU budget; a wall-time loop under contention
  did not guarantee 200 ms of CPU. The async member-lock test polls the held
  future directly and uses a paused clock rather than sleep-based evidence.

- **Leftover lifecycle and probe cleanup.** Background test work waits on explicit
  marker-present file gates, removed even during unwinding. Absence remains
  observable after temporary-home deletion, so teardown cannot strand a child;
  a real-process regression proves explicit release, guard drop and home removal.
  The same gate covers service-configuration fixtures added by the parallel item.
  TERM-ignore traps acknowledge
  readiness before a cap starts; tests observe actual TERM handling and final
  state instead of assuming a still-live process or a short completion ceiling.
  A nonsecret `waiting_for_leftovers` lifecycle event identifies when a placed run
  reaches that phase, letting the integration test release its predecessor only
  then and require a positive recorded wait. Process-group holders block on owned
  input rather than expiring during observations. Fingerprint tests use a blocked
  descendant and verify that the probe kills its group, with a generous overall
  bound, replacing the two-second scheduling margin around the ten-second limit.

## Performance policy

`mise run check` still executes the full 20,000-file warm/cold sync, 300-commit
object-source fixture, 5,000-file carried-ref fixture, and peer latency scenarios.
Contents, writes, transferred bytes, pack reduction, authentication, placement,
exit results and timeout lower bounds remain asserted in normal checks.

`mise run perf` additionally enforces the existing timing budgets, with one test
at a time. It uses the same fixtures and same-run cold/unsourced/no-ref baselines;
no performance assertion is deleted or loosened. This separates scheduler latency
from functional failures and prevents concurrently running benchmark fixtures
from becoming one another's changing baseline. The latency scenarios retain their
upper bounds there too. Run this task without synthetic hogs or another benchmark
on the machine. Compare its emitted `PERF` samples on the same machine, toolchain
and build profile; changes to a budget require new paired measurements and review.
A contended performance run is not a functional check failure. Release runs retain
the original absolute sub-second warm-sync budget as well.

This policy is preferable to widening a timing ratio until every contended run
passes: wall scheduling delays can differ even between adjacent measurements.
The ordinary check continues to catch extra writes, unwanted packs, missing
history and refs, bad outputs, and incorrect state transitions.

## Consumer entry and environment isolation

The root `.elsecall.toml` declares `elsecall check` (2 weighted cores),
`elsecall build` (4.5), a 15-minute interactive queue allowance for both,
and `when_unavailable = "run"`. The executable mise
entry scripts use the documented consumer guard and re-exec pattern. The entire
check or build is one request; a check nested in a run stays in its outer grant.
Without an installed executable the scripts run directly. `ELSECALL=off` and an
unavailable service follow the consumer kit's existing semantics.

Sizing was measured on this 14-logical-core Mac with `/usr/bin/time -l`:

| Work | Wall | User + system CPU | Average busy cores | Peak RSS |
| --- | ---: | ---: | ---: | ---: |
| Original baseline check, including cold compilation | 336.82 s | 1109.50 s | 3.29 | 2.12 GB |
| Final check, fresh temporary Cargo target | 251.56 s | 382.50 s | 1.52 | 2.14 GB |
| Clean release build | 53.01 s | 222.08 s | 4.19 | 2.38 GB |

A verification run also observed a full-capacity competing request outlasting
the default 323-second hold limit; Elsecall correctly returned `fleet_busy`
before Cargo started. The explicit 15-minute hold limit permits queueing behind
long verification batches without changing test budgets or bypassing admission.
That refusal is recorded separately and is not counted as a passing check.

The final cold-check measurement runs inside admission, so its clock excludes
queue time. Its result selects 2 cores for checks; the measured release build
selects 4.5. The original check measurement is retained as failure evidence, not
as the final sizing input. Sizes round measured average CPU use up to the next
half core. They are admission
estimates, not thread limits. Re-measure when the workload or toolchain changes.

Before running Cargo's tests, `scripts/test-environment.py` removes `ELSECALL`
and every `ELSECALL_*` key, including unknown future controls and
`ELSECALL_ADMISSION`. This isolates in-process services too, without mutating a
multithreaded process environment. Rust fixture constructors independently strip
the same controls before adding explicit fixture settings, preserving toolchain
and unrelated OS environment. Fixture CLI working directories default to their
temporary homes, so they cannot accidentally discover this repository's consumer
policy. Tests deliberately needing a consumer tree set their own directory.

Subprocess regressions inject poisoned outer admission, fleet, hold-limit and
unknown controls, proving both the test entry boundary and the independent Rust
constructors. A real member admits a fresh run; an in-process fixture and its
scripts see isolated controls. The actual development scripts are exercised
against a temporary real service and a fake external Cargo executable, proving
exactly one admission, fail-fast exit propagation, policy lookup and off/unmanaged
modes. Existing entry-pattern tests cover a missing executable and signal handling.

## Measurement and validation

Baseline `mergegate/main` failed `warm_syncs_carrying_refs_cost_what_they_did_without`:
carrying refs had median 312 ms versus 223 ms without, during concurrent fixtures.
The original allowance was 25% plus 15 ms. An exploratory check with 14 CPU hogs
then exposed `a_slow_push_to_one_member_does_not_delay_a_run_on_another`: its
paused member timed out before the supposedly concurrent command completed.
These failures were investigated and fixed; the acceptance sequence stops on any
failure, with no automatic rerun of a failed test or check.

Independent review covered architecture, behavior and security. Findings about
live CPU accounting, bounded gate cleanup, inherited-environment artifacts and
rebase integration were corrected, and the final review approved the change.

Five consecutive full checks passed on revision `1595c06` (including the
log-retention item on `4ba597e`). Each passed all 478 tests; the one ignored
entry is an existing subprocess helper that is executed by its parent test.
There were no test retries or ignored flaky tests.

| Run | Wall time, including admission (s) | CPU hogs | Observed peak 1-minute load |
| --- | ---: | ---: | ---: |
| 1 | 268.83 | 14 | 164.53 |
| 2 | 209.81 | 0 | 198.06 |
| 3 | 609.76 | 0 | 160.71 |
| 4 | 410.14 | 0 | 104.04 |
| 5 | 227.45 | 0 | 54.10 |

The synthetic run started one `yes` process per logical core after admission,
kept them throughout `mise run check`, then terminated and reaped all 14.
Runs 3 and 4 included capacity waits; run 3's admission record separates
401.763 s waiting from 207.845 s executing. The queue allowance changed only the
consumer policy, never a test's expected behavior.

Serial `mise run perf` passed in 120.62 s after those checks and before rebasing. It measured
warm 20,000-file syncs at 625–669 ms against 10,087 ms cold; source-assisted
history at 1,099 ms against 2,855 ms without; carried-ref median 236 ms and
default-ref median 242 ms against 220 ms without refs.

A parallel service-settings item then landed on `4110ed5`. After rebasing,
the complete combined check passed in 269.98 s: 495 passed, zero failed and the
same one helper ignored. This includes the newly merged service-configuration
flows and the real-process gate cleanup regression. The existing five-run
measurement is recorded separately above rather than attributed to the newer base.
The rebased `mise run build` also passed through consumer admission in 83.17 s
(including its queue wait), with release compilation taking 26.56 s.
The rebased serial `mise run perf` passed all 11 timing tests in 111.76 s:
20,000-file warm syncs took 716–913 ms against 9,483 ms cold; source-assisted
history took 1,538 ms against 3,426 ms without; carried-ref median was 256 ms
and default-ref median 226 ms against 222 ms without refs. All original
performance budgets passed.

Installed-system verification: left to the release verification session.
That session should run the check/build entry scripts through the installed CLI,
inspect one admission record and the label-rule sizes, verify unavailable/off and
nested behavior, and repeat a remote command that prints its run ID while returning
many skipped-link warnings under load. Confirm stdout/exit/records are preserved
and a command leaving stdout open still returns after genuine pipe quiet.

## Forced-controller-loss comparison

The controller-loss comparison test had a three-second `--limit` covering baseline
preparation and execution, not only the proposal trial. On `f336e4c`, an isolated
run exported an incomplete baseline with a worker exiting 124 at that deadline.
Neither member activated the proposal's 0.2 margin. The comparison correctly
returned before applying the proposal, while the test waited 120 seconds for a
state that could no longer occur. This was a test setup deadline, with no evidence
of a production restoration defect.

The test now allows 30 seconds for each rehearsal arm and observes proposal
activation with the existing 120-second scheduling allowance. It fails promptly
if the controller exits, with file-backed output for diagnostics. At the first
member's activation it suspends the controller, confirms the stopped state without
reaping it, then observes both members at 0.2 before SIGKILL. This prevents normal
controller cleanup from satisfying the restoration assertion. The test still
requires both members to return to 0.1 and the original source bytes to remain
unchanged. The current 55-second settings lease fits within the unchanged
120-second restoration wait. Failed observations kill and reap the controller.

All measurements for this follow-up use `nice -n 10`, normal machine load, and no
synthetic load. Full checks set `ELSECALL_ADMISSION=isolated-test-verification` to
short-circuit the installed CLI wrapper; the test entry removes that variable
before starting temporary-home fixtures. No real-machine services are contacted.

Unchanged baseline isolated runs failed in 123.28 seconds (149.24 seconds including
compilation) and passed in 35.37 seconds (35.64 including command setup). Two
unchanged full checks failed in 369.48 and 315.19 seconds. Both reproduced the
controller-loss timeout and also failed these neighboring tests, outside this item:

- `automatically_monitored_failing_day_revokes_trust_and_incomplete_day_is_not_checkable`
- `compare_restores_after_second_run_hits_its_bound`
- `completed_rehearsal_certifies_tune_and_build_changes_expire_it`

Twenty consecutive isolated runs of the reviewed fix passed, without retries.
Wall times below include command setup and the first run's compilation. The
median was 62.81 seconds; the total was 1,261.37 seconds. End-of-run one-minute
load samples ranged from 18.2 to 114.4.

| Isolated run | Wall seconds |
| --- | ---: |
| 1 | 65.84 |
| 2 | 64.52 |
| 3 | 63.32 |
| 4 | 62.60 |
| 5 | 62.52 |
| 6 | 62.64 |
| 7 | 63.21 |
| 8 | 64.15 |
| 9 | 62.95 |
| 10 | 62.05 |
| 11 | 62.79 |
| 12 | 62.83 |
| 13 | 62.12 |
| 14 | 61.97 |
| 15 | 64.02 |
| 16 | 63.56 |
| 17 | 62.96 |
| 18 | 62.36 |
| 19 | 62.52 |
| 20 | 62.46 |

The first full-check attempt also exposed two more neighboring failures. Running
the original `mergegate/main` rehearsal test file reproduced both, alongside the
original controller-loss timeout, in 126.03 seconds (123.94 seconds in the test
binary):

- `rehearses_timing_members_outcomes_real_cold_warm_copies_and_own_fidelity`
- `compare_reports_live_metrics_uses_synthetic_sizes_and_restores_synced_settings`

The reviewed test file was then restored byte-for-byte before continuing the full
checks. These additional baseline failures are recorded under the same scope
exception; no neighboring assertion is skipped or retried.

Three full `mise run check` attempts followed the isolated sequence. Formatting
and Clippy passed in all three, and the fixed controller-loss test passed each
time. All three checks exited 101 on neighboring failures reproduced on the
unchanged base; Cargo stopped at the failing rehearsal test binary. Therefore
three green full checks were not achieved. Per this item's scope exception, the
neighboring failures remain separate work, with all assertions enabled.

| Full check | Wall seconds | Controller-loss test | Baseline neighboring failures |
| --- | ---: | --- | ---: |
| 1 | 234.79 | passed | 5 |
| 2 | 238.28 | passed | 3 |
| 3 | 234.43 | passed | 5 |

Check 2 failed the bounded-second-run, live-metrics comparison, and timing/fidelity
tests listed above. Checks 1 and 3 failed all five neighboring tests listed above.
Independent review covered behavior, architecture, and security; the stopped-child
observer leaves reaping to its owned child handle, including on assertion failure.


## Rehearsal integration follow-up

This follow-up covers the five neighboring failures above. All commands run with
`nice -n 10`, under normal operator load, with no synthetic load generator and no
installed-system calls. Full checks set
`ELSECALL_ADMISSION=isolated-test-verification`; the test entry removes that
control before launching temporary-home fixtures.

### Causes and preserved coverage

- `completed_rehearsal_certifies_tune_and_build_changes_expire_it` and
  `automatically_monitored_failing_day_revokes_trust_and_incomplete_day_is_not_checkable`
  expected a short real CPU trial to certify under arbitrary host contention.
  In the unchanged full check, the planned 2,500 ms took 4,278 and 4,419 ms.
  Both recordings were complete, with no fidelity blockers, but correctly failed
  the unchanged 5% charged-time tolerance. The shared certification fixture now
  paces a small positive CPU budget over 30 seconds, rather than turning trust
  lifecycle coverage into a CPU benchmark. It uses less CPU than the original
  trial. Both tests still require real completed rehearsal, full observations,
  a non-null certificate, and the original tuning/build-expiry or automatic
  drift-revocation behavior. There are no certification or assertion retries.
- `compare_restores_after_second_run_hits_its_bound` used the same four-second
  setup-and-execution limit for an overlapping baseline and serialized proposal.
  Baseline preparation or host stalls could consume that limit before B began.
  The fixture now uses a 30-second arm limit and a proposal admission threshold
  below the existing scripted 5% load. The baseline completes; proposal requests
  actually queue and remain unexecuted until the limit. Their 120-second holds
  cannot expire first. The test requires both submissions and terminal exported
  records, an observed queue, the limit lower bound, no certificate or comparison
  acceptance, complete cleanup, and restoration of synced settings. No extra CPU
  load or process suspension is needed to hold this condition.
- `rehearses_timing_members_outcomes_real_cold_warm_copies_and_own_fidelity`
  assumed timestamp order identified source requests and required subsecond
  wall-time accuracy under host contention. The baseline full check failed on a
  1,095 ms execution against the 400 ms +/- 500 ms budget. The test now uses the
  explicit original-ID/run-ID mapping, verifies all six unique exported records,
  and checks cold/warm copies in execution order. Normal checks retain exact
  compressed arrival/duration metadata, no early arrivals, positive fixed work,
  minimum execution duration, outcomes, placement, sizes, uncompressed holds,
  exported-trace fidelity identity, production-history isolation and cleanup.
  A 120-second scheduling allowance covers preparation and queued copy access.
  The original adjacent-arrival and run-duration upper budgets remain unchanged
  in `mise run perf`, which now selects this test through its `perf_` prefix.
- `compare_reports_live_metrics_uses_synthetic_sizes_and_restores_synced_settings`
  incorrectly required a null verdict without an A/A noise file. A known hard
  violation correctly rejects a proposal even without noise; the baseline full
  check reproduced that assertion failure. The test now checks the shared
  acceptance rule and forbids acceptance without noise. Live metrics, both arms'
  synthetic sizes, source/seed identity, restored settings and unchanged source
  bytes remain asserted. Successful trials use the 120-second scheduling bound.

No product defect was found in these five failures. Production deadlines,
fidelity thresholds, and acceptance semantics are unchanged.

### Validation

On unchanged `8dd4040` (`mergegate/main`), the rehearsal suite passed all 23 tests
in 64.05 seconds. The full check then failed in 289.52 seconds on the four
reproduced failures described above; the bounded-second-arm test passed in that
run. This contrast demonstrates why an isolated pass did not establish reliability.

The first revised rehearsal-suite check took 62.80 seconds in the test binary,
passed all five target tests, and exposed an additional failure in
`limit_stops_in_flight_local_and_remote_work_and_exports_it`: its explicit
six-second execution cap was followed by the 15-second cleanup deadline, with
`cleanup_complete=false` and
`cleanup_deadline: member watchdogs/recovery retain responsibility` (21.277 seconds
reported by the controller). This test is outside the five-test item; it is not
counted as green or hidden by a retry.

All five fixed tests passed **ten consecutive isolated runs each**, with one test
per process and no concurrent verification jobs or retries. The table gives wall
seconds including command setup; columns are drift revocation, bounded proposal,
certificate/build expiry, timing/copies, and live metrics, respectively.

| Run | Drift | Bound | Certificate | Timing/copies | Metrics |
| --- | ---: | ---: | ---: | ---: | ---: |
| 1 | 39.28 | 38.18 | 34.13 | 9.19 | 12.83 |
| 2 | 41.40 | 38.13 | 33.41 | 8.49 | 10.66 |
| 3 | 38.05 | 37.91 | 33.94 | 9.45 | 11.74 |
| 4 | 37.89 | 38.86 | 35.37 | 9.65 | 12.06 |
| 5 | 37.29 | 37.88 | 34.03 | 9.75 | 11.24 |
| 6 | 38.58 | 39.41 | 34.38 | 10.12 | 10.87 |
| 7 | 39.71 | 39.12 | 35.45 | 11.15 | 11.51 |
| 8 | 38.04 | 38.64 | 34.92 | 10.02 | 13.17 |
| 9 | 38.67 | 37.98 | 34.54 | 9.70 | 13.75 |
| 10 | 41.45 | 41.07 | 34.81 | 9.10 | 12.85 |

The 50 runs took 1,339.82 seconds in total. End-of-run
one-minute load samples ranged from 14.44 to 57.39.

Three consecutive full `mise run check` runs then passed on the same code, without
retries. Formatting and Clippy passed each time. Each check passed all 726 tests,
with zero failures and the one existing ignored subprocess helper. The earlier
unrelated in-flight cleanup timeout did not recur in these checks; its failed
exploratory run remains recorded above rather than being counted as green.

| Full check | Wall seconds | Passed | Failed | Ignored | Ending 1-minute load |
| --- | ---: | ---: | ---: | ---: | ---: |
| 1 | 356.93 | 726 | 0 | 1 | 163.34 |
| 2 | 358.75 | 726 | 0 | 1 | 133.87 |
| 3 | 384.57 | 726 | 0 | 1 | 72.33 |

Independent review covered architecture, behavior and security. Its findings
restored explicit exported-record cardinality/uniqueness checks and strengthened
minimum execution and actual queued-expiry evidence. No findings remain open.

A final isolated run with `EC_TEST_PERF=1` passed the original rehearsal timing
ceilings on the accepted code in 10.46 seconds including command setup (9.71
seconds in the test binary).

Installed-system verification: left to the release verification session.
That session should run a normal rehearsal to completion and inspect its exported
mapping/observations/fidelity and certificate; run tune against that certificate;
run a bounded comparison with a deliberately gated proposal and verify the
incomplete verdict, worker/copy cleanup, restoration of effective settings on both
members, and unchanged synced source bytes; then check build-expiry and automatic
drift revocation in disposable release-verification state.

## In-flight limit cleanup idempotence

`limit_stops_in_flight_local_and_remote_work_and_exports_it` exposed a product
race, rather than merely an observation budget that needed widening. On unchanged
`779171e` (`mergegate/main`), the full offline check reproduced the failure:
controller elapsed time was 21,333 ms, with `stop_reason=limit`,
`cleanup_complete=false`, and the cleanup-deadline diagnostic. Both submitted
requests were recorded with exit 124. An exploratory sequence with unchanged
product code also failed on isolated run 9 after eight passes (21,282 ms reported
by the controller), so full-suite contention was not necessary.

Temporary phase instrumentation showed the remote member's watchdog completing
copy/store cleanup first. The controller then repeated cleanup roughly every
100 ms for the entire 15-second allowance. Caller and process-group settlement
completed promptly. Durable removal history still supplied the now-deleted store
path, so housekeeping repeatedly tried `git for-each-ref` with a nonexistent
working directory. More cleanup time could not resolve that state. The temporary
instrumentation was removed; no installation or live-service changes were made.

Rehearsal housekeeping now treats a confirmed absent store as already clean.
`symlink_metadata` skips only `NotFound`; other filesystem errors still fail, and
existing stores retain their identity, reference and reservation checks. Removal
history remains durable. This also permits cleanup to resume after removal has
already succeeded, without relying on an in-memory session flag.

A deterministic integration regression completes a real two-member rehearsal,
requires evidence of a started cold remote copy and complete cleanup, then calls
cleanup again on each member through the real authorized API. It failed against
unchanged product code with `workspace_sync_failed` / “git could not be started:
No such file or directory” in 3.71 seconds, and passed with the fix in 3.74 seconds
(test-binary times). No internal modules are stubbed.

The in-flight limit test keeps its six-second execution cap, longer ten-second
compressed workloads, and third arrival after the cap. Controller exit is observed
with the existing 120-second scheduling allowance and file-backed output; a guard
kills and reaps a controller on assertion failure. The old fifteen-second total
wall ceiling is removed. Successful CLI exit, `stop_reason=limit`, exactly two
submitted and terminal exported requests, started execution on both members,
no certificate, fidelity output, and immediate full cleanup remain required.
Both exported outcomes must additionally be unsuccessful, and execution cannot
return before the six-second cap. No export assertion is retried, no watchdog
fallback failure is accepted as success, and production deadlines are unchanged.

### Validation

All verification uses `nice -n 10`, offline Cargo, temporary-home services and
loopback transport. Full checks set
`ELSECALL_ADMISSION=isolated-test-verification` to bypass the installed consumer
wrapper; the test entry strips that variable before starting fixtures. No
synthetic load is added. Parallel operator work can still contend for the host.

Ten consecutive isolated runs passed without retries on the fixed code. Wall
seconds include command setup and the first run's compilation; total time was
90.13 seconds and the median was 8.47 seconds. End-of-run one-minute load samples
ranged from 8.54 to 10.78.

| Isolated run | Wall seconds |
| --- | ---: |
| 1 | 9.81 |
| 2 | 11.47 |
| 3 | 9.93 |
| 4 | 8.28 |
| 5 | 8.27 |
| 6 | 8.89 |
| 7 | 8.28 |
| 8 | 8.39 |
| 9 | 8.26 |
| 10 | 8.55 |

Three consecutive full `mise run check` runs then passed with no retries or
unrelated failures. Formatting and Clippy passed in all three. Each check passed
732 tests with zero failures and the one existing ignored subprocess helper.
Both the in-flight limit test and the deterministic repeated-cleanup regression
passed in every check.

| Full check | Wall seconds | Controller milliseconds | Ending 1-minute load |
| --- | ---: | ---: | ---: |
| 1 | 332.92 | 6349 | 7.80 |
| 2 | 294.71 | 6357 | 7.29 |
| 3 | 295.99 | 6327 | 7.94 |

These checks and the isolated sequence built against `779171e`. The third check
had already compiled when the parallel settings-restoration item merged; this
change was then rebased cleanly onto `676d2b3`, with an identical patch.

The additional full offline check on the combined base passed in 335.00 seconds:
737 passed, zero failed, and the same one ignored helper. Formatting and Clippy
passed; both regression tests passed, with the in-flight controller reporting
6,335 ms and complete cleanup. The ending one-minute load was 7.46. No unrelated
failures occurred in any of the four full checks.

Independent review found no architectural, behavioral or security issues,
including interactions with the merged settings-restoration changes.

Installed-system verification: left to the release verification session.
That session should run a bounded rehearsal with simultaneous long local and
remote work and a later unsubmitted arrival; require successful command exit,
`cleanup_complete=true`, terminal unsuccessful records for both members, retained
export/fidelity and no certificate; inspect both members for ended rehearsal runs
and absent copies, stores, journals and scratch directories. Repeat under normal
load to exercise controller/watchdog cleanup ordering, then verify that an
abandoned controller's watchdog cleanup leaves the same resources absent.
