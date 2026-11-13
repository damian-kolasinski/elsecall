# S4: Placement

Step S4 of [Fleet scheduling](fleet-scheduling.md): earliest-completion placement,
learned run times and sizes, warmth, and a fleet-wide `fleet_busy`. It came in
stacked items:

1. **elsecall#16** (merged), placement inputs: measured core speed, CPU time per run, learned run times and
   sizes, and the copy's warmth in records.
2. **elsecall#19**, placement: quotes, scoring, automatic placement and reconsideration, the
   fleet-wide `fleet_busy`, `explain` and `runs`.
3. **This item:** installed-system evidence on both members.

## Decisions

### Speed is measured per core, on the thread's own clock
- Sizes mean the same number of cores on every member, so what differs between
  members is the work one core does. Each member measures that with a short
  compile-like workload: sorting, a hash table and a byte scan. It exercises
  branches, caches and memory the way builds do, not one instruction that one
  unit executes.
- Each trial is timed by `CLOCK_THREAD_CPUTIME_ID`. Time the scheduler gives to
  other processes does not count. So the devbox, at load average 50 to 330, still
  measures its core and not its load. Load is a separate input (see below).
- The thread asks for user-interactive QoS, which macOS places on a performance
  core when one is free. A trial that lands on an efficiency core, or shares caches,
  only ever comes out slower. The measurement is therefore the best of five
  trials, and a member's speed is the best of its last six measurements, taken
  every ten minutes.
- The unit makes an M4 Pro performance core about 1. Only ratios matter.
- Rejected alternatives:
  - **Wall-clock benchmarks** measure load as much as speed.
  - **Speed derived only from records** needs the same label to have run on both
    members first. Records refine run times per member anyway (see "Run times").

### Run time and size come from records
- **Trust.** Only completed runs of the last 14 days count, at most the latest 20
  of each kind, and an estimate needs at least 3 of them (`learning.rs`).
- **Run time on a member:**
  - its own median when trusted;
  - otherwise the fleet's, with every member's runs converted to work (seconds ×
    measured speed) and divided by this member's speed.

  The estimate families item (below) put the label's family between these and
  the prior, and converts by learned speed ratios before measured speeds.
- **Warm and cold.** Runs into a copy that was cold when admitted are learned
  apart from warm ones.
- **What a run's time covers.** The command's own time: for a placed run, the
  sync is taken out.
- **CPU time.** A run's command is noticed exiting with `waitid(WNOWAIT)`, and its
  CPU time is read from the unreaped process with `proc_pid_rusage`. That time
  includes every descendant it waited for. The command is reaped afterwards. On
  members this happens on SIGCHLD and on a 200 ms tick, never on a parked thread,
  so a service still stops at once.
- **Size.** The upper quartile of recent runs' busy cores (CPU time over run time,
  for runs of at least one second), rounded up to half a core:
  - It is used when a run declares no size.
  - It never exceeds the calling member's capacity.
  - A member offered a learned size it cannot hold shrinks it instead of
    refusing.
- **Syncs** (`sync_history.rs`):
  - **Warm:** this workspace's warm syncs to the member (from 2 samples), else that
    member's warm syncs (from 3).
  - **Cold:** first syncs of the same repository in the same state (from 1 sample,
    since a first sync happens once per copy and depends on the repository far more
    than on the member).

### Expected completion is wait + sync + run (`placement.rs`)
- **Wait.** A simulation of the member's capacity:
  - running work ends at its expected remaining time (60 s when unknown; see
    "Running work's remaining time" below for what a later item changed);
  - requests ahead of this one start in order: the member's own queue, then what
    the master already offers it.
- **Sync.** Learned, else a deliberately pessimistic prior:
  - 3 s warm;
  - 30 s for a first sync with the repository's history already there;
  - 120 s for a first sync without it.

  Measured first syncs were 5.8 s and 43 s for a 12,857-file worktree (S3), so
  the priors are several times worse than real large repositories.
- **Run.** Learned, else 60 s of work at speed 1 divided by the member's speed.
  - A cold copy runs a cold build: learned cold runs, else three times the warm
    estimate.
  - **Outside load** slows the run when outside load and Elsecall's running work
    leave fewer idle weighted cores than the request's size. The slowdown is
    proportional, at most 4×.
- **Eligibility.** A candidate is eligible when wait + sync fits what is left of the
  hold limit. It is never eligible:
  - while saturated or under memory pressure ("never sent somewhere already
    saturated");
  - while another run uses its copy;
  - when the request is larger than its capacity. The reserve is already outside
    capacity, so the master's reserve holds through capacity.
- **Winner.** The eligible candidate with the earliest completion.
  - The caller keeps the work unless another member is earlier by
    `max(2 s, 10%)`.
  - Among other members, warm copies win ties.
  - With the cold priors, a new workspace stays on the caller unless the caller
    cannot start it in time or is clearly worse off.

### A request is always somewhere, and is reconsidered every second (`service/auto.rs`)
- **Where it waits.** In the caller's own queue, in the queue of requests the
  master offers to one member (shared with `--member` requests, in arrival order),
  or undecided while no member can take it in time.
- **Quotes.** On arrival and then every second, the master asks each other member
  for a quote (`POST /peer/v1/placements/quote`, behind placement authority). A
  quote carries:
  - its admission state, speed and windowed utilization;
  - its running and waiting work with expected times;
  - its copy of each asked workspace: warm, cold with history, or cold, and busy.

  The master bounds what it believes: utilization in 0..1, plausible speeds, at
  most 256 work items, and labels validated.
- **Answers merge.** The master keeps each member's state and each copy it
  reported apart (`service/quotes.rs`). Answers about different workspaces, asked
  at different times, merge, and an answer older than the one kept is ignored.
- **Slow and unreachable members.** A new request waits at most 750 ms for quotes.
  A member that answers later is scored on its earlier answer. A member that
  cannot be asked (3 s in the background) is left out and named in the decision,
  in the master's own words: a member's error text is never quoted. For new
  requests it is not asked again for 10 s. A workspace a member cannot host, such
  as a same-path copy that would overlap its own directories, makes that member
  ineligible for that workspace only.
- **Nested work first.** A placeable request from inside a lease or run nests at
  once, before any member is asked.
- **Moving.** Waiting requests are scored again oldest first. A request moves
  when:
  - its place can no longer start it in time; or
  - another member can start it now and completes earlier by the same margin.
- **Offers in flight.** An offer is built just before it is sent, for a request
  still queued for that member, and the request does not move until the answer is
  in. So a member never admits a request that has already left.
- **Learning from replies.** Offer replies update that member's state at once, and
  an admitted request is added to its running work, so concurrent requests do not
  all pick the same member.
- **Deferrable work.** On the caller, only interactive work counts as ahead of a
  placeable request, as admission starts interactive work first.
- **Hold limit.** When it passes, the request fails with a `fleet_busy` that covers
  every member:
  - why each one could not take it;
  - what runs where (`details.running` with `member`);
  - the earliest estimated start across members as `retry_after_seconds`.

### Observability
- **Candidates.** Every candidate in a decision carries `estimate`:
  - speed, and whether it was measured;
  - copy state and whether the copy is busy;
  - outside utilization;
  - when it could start;
  - wait, sync and run seconds, each with its source (`member_runs`, `fleet_runs`,
    `workspace_syncs`, `member_syncs`, `repository_syncs` or `prior`);
  - the cold factor and slowdown;
  - completion and eligibility.
- **Reasons.** A one-line `why` per candidate. The decision's `reason` says why
  the winner won, or why no member could take the work. `unreachable` lists the
  members that could not be asked.
- **`runs`.** Lists `placement`, `calling_member`, `placed` (side, members, warmth,
  reached, sync time) and `reason`.

## Automated evidence

`mise run check` passes. Unit tests cover:
- the benchmark;
- CPU time of an unreaped child against `wait4`'s rusage;
- learning and sync history rules;
- the wait simulation, slowdown and scoring;
- believed quotes and the fleet-wide `fleet_busy`.

`tests/placement.rs` runs real services in temporary homes on loopback with
scripted core counts, load and speed:

| Test | Shows |
| --- | --- |
| `a_new_workspace_stays_on_the_caller_and_member_overrides_it` | A twice-as-fast cold member loses to the idle caller, with the priors in `explain`. `--member` overrides. A non-master runs placeable work itself. |
| `a_warm_faster_member_wins_and_explain_shows_why` | A warm member at speed 2 wins (33 s against 60 s). The candidates' components, the reason and `runs`. |
| `a_saturated_caller_sends_work_away_and_a_saturated_member_is_avoided` | Remote when local is saturated, even cold. A saturated warm member is never chosen. |
| `the_callers_reserve_holds_through_capacity` | A size above the caller's capacity after its reserve goes elsewhere. Within capacity it stays. Larger than every member is `request_too_large`. |
| `a_warm_copy_is_preferred_when_the_caller_is_busy` | With the caller full, the warm member beats the cold one. |
| `no_member_in_time_is_fleet_busy_with_a_retry_hint_and_what_runs_where` | The fleet-wide `fleet_busy` with members, running work per member and a retry hint. |
| `a_request_waiting_on_a_busy_caller_moves_to_a_member_that_frees` | Reconsideration moves a waiting request when another member frees. |
| `learned_run_times_change_the_decision_as_records_accumulate` | The decision flips once trusted history shows the label is faster on the member. |
| `an_unreachable_member_is_left_out_and_named` | A stopped member does not block placement. |

`tests/run.rs` learns a size through the real CLI. `tests/remote_run.rs` checks CPU
time and warmth on both records of a placed run, a learned size shrinking on a
smaller required member, and a member service stopping within 5 s while a placed
command runs.

## Installed-system evidence

On 2026-11-03 the release build of the placement item (commit `ec4ca26`, sha256
`808e7f2a47ab63b5…`) was installed on both members:

| Member | Role | Cores | Capacity | Measured speed |
| --- | --- | --- | --- | --- |
| `mac-mini` | master, desktop session | 10 P + 4 E = 12 weighted | 9 (`reserved_cores = 3`) | 1.00 |
| `macbook-pro` | member, account `devbox`, background session, mapped copies | 8 P + 2 E = 9 weighted | 9 | 0.70 and 0.76 |

The devbox measured its speed twice, after two installs, at load averages of
about 21 to 32. Each time it was measured on the thread's own CPU clock. Hashing
2,000 MiB took 3.0 s on the mini and 3.4 s on the devbox.

Every command ran on the mini, in a fresh scratch repository with one commit and an
untracked file. The command was a synthetic CPU-bound job with fixed work: `--size
N` parallel `dd | md5` hashes of 3,000 or 4,000 MiB each, for N from 2 to 4, with
the label `synthetic hash`. A sampler recorded both members' `elsecall runs` state
every second (45 samples each). Nothing else ran on the devbox. The mini's own load
varied with the agents running on it: its CPU utilization was 94 to 100% during
the first wave. The full output is summarised below from `elsecall explain`.

**Warm-up.** One `--member macbook-pro` run set the devbox's copy up.

**Wave 1: one at a time, without history for the label. The saturated mini was
avoided.** Other work on the mini held it at 94 to 99% CPU, so its gate was
`saturated`, and the first two runs went to the devbox:

```text
r-e53be69de038 size 2 -> macbook-pro
reason: macbook-pro completes earliest, in 79 s; the calling member cannot start it in time.
  mac-mini: free 9.0 of 9.0, gate saturated — Saturated by outside load (94% CPU).
* macbook-pro: free 9.0 of 9.0, gate open, speed 0.76, copy warm
    Starts now, completes in 79 s: warm sync 0.3 s (learned from 3 syncs there) + run 79 s (prior).
```

When the third run arrived the gate had just reopened, but the mini's idle cores
were still fewer than the request's size:

```text
r-7e3181ae3967 size 4 -> macbook-pro
reason: macbook-pro completes earliest, in 79 s, against 168 s on the calling member mac-mini.
  mac-mini: Starts now, completes in 168 s: run 168 s (prior, 2.8× slower under load).
* macbook-pro: Starts now, completes in 79 s: warm sync 0.3 s (learned from 2 syncs of this workspace) + run 79 s (prior).
```

**Wave 2: 16 cores at once against the mini's 9.** Five requests (4, 4, 3, 2 and 3
cores) went out within 1.5 s. The label now had three runs on the devbox, 5.7 to
7.0 s, which the mini converted by speed.
- The first two stayed on the mini: 5.1 s there against 8.2 s on the devbox.
- The third saw the mini's queue and went to the devbox:

  ```text
  r-6f17a41b32a1 size 3 -> macbook-pro
  reason: macbook-pro completes earliest, in 6.2 s, against 9.9 s on the calling member mac-mini.
    mac-mini: free 1.0 of 9.0 — Completes in 9.9 s: wait 5.4 s + run 4.5 s (learned from 3 runs, converted by speed).
  * macbook-pro: free 9.0 of 9.0 — Starts now, completes in 6.2 s: warm sync 0.3 s (learned from 3 syncs of this workspace) + run 5.9 s (learned from 3 runs there).
  ```

- The last two waited on the mini, 6.9 and 6.6 s. The devbox still had six free
  cores, but its copy of this workspace was in use, and one run at a time uses a
  copy.

**The reserve held.** Across the 45 samples, the mini's `in_use` never exceeded 8
of its capacity of 9, three cores below its 12 weighted cores. While two requests
queued it read `in_use 8.0 of 9.0 queued 2`, because the next request, of size 2,
did not fit the one free core.

**Wave 3: the mini saturated on purpose.** Fourteen niced `yes` processes outside
Elsecall held the mini at 100% CPU. Both requests went to the devbox, now with
estimates learned on it:

```text
r-b1d4b32c3524 size 2 -> macbook-pro
  mac-mini: free 9.0 of 9.0, gate saturated — Saturated by outside load (100% CPU).
* macbook-pro: Starts now, completes in 6.8 s: warm sync 0.3 s (learned from 4 syncs of this workspace) + run 6.5 s (learned from 4 runs there).
```

**Wave 4: idle again.** The run stayed on the caller within the margin:

```text
r-1404c6ec53dd size 2 -> mac-mini
reason: The calling member mac-mini completes in 7.6 s; macbook-pro would take 6.2 s, not earlier by the 2.0 s margin for leaving the caller.
```

**Gates over the series.** The mini's gate was `saturated` in 21 of 45 samples;
the devbox's was `open` in all 45. `elsecall runs` on the mini lists each run's
placement:

```json
{"run_id": "r-afb1a78cf1ef", "placement": "placeable", "calling_member": "mac-mini", "member": "macbook-pro", "placed": {"side": "caller", "calling_member": "mac-mini", "member": "macbook-pro", "warmth": "warm", "reached": "finished", "sync_ms": 302}, "reason": "macbook-pro completes earliest, in 6.2 s; the calling member cannot start it in time."}
```

**What the run showed.**
- **One run per copy limits parallelism.** It decided the last two wave-2
  requests: one workspace can run on the devbox only once at a time, however many
  cores are free there.
- **Speed converts only roughly.** Against fixed work the devbox was 0.88 of the
  mini, while the benchmark measured 0.70 to 0.76. Until the label had runs on
  the devbox, its time there was overestimated. Once three runs were learned, the
  devbox's own times took over (`member_runs`).
- **Utilization lags.** It is averaged over 3 s. For a moment after Elsecall's own
  runs end, their load still counts as outside load, as the 2.8× slowdown in wave
  1 shows.
- **An earlier series** with time-bound jobs (`yes` for 6 or 8 s) gave the same
  placements: overflow went to the devbox, the saturated mini was avoided, and the
  mini's `in_use` peaked at 9 of 9.

## Limits

- **Fairness across members.** Requests are first come, first served per member.
  A later request may start on one member while an earlier one waits for another.
- **Losing a queue position.** A request moved out of the caller's queue to a
  member that then refuses it goes back to the end of the caller's queue.
- **History lives on the master.** Estimates use the master's records only. Runs
  other members make on their own are not seen.
- **What CPU time misses.** It counts descendants the command waited for.
  Daemonized or double-forked processes are missed, and contention lowers busy
  cores, so learned sizes err small under load.
- **Learned sizes are capped** at the calling member's capacity.
- **Quotes are at most a second old.** Requests arriving together can still pick
  the same member; the one it refuses is reconsidered a second later.
- **No early failure.** A request waits its whole hold limit even when estimates
  say no member will free in time ("may fail early" was not taken).
- **The speed seam.** `ELSECALL_MACHINE_SPEED` and `ELSECALL_MACHINE_LOAD=file:`
  exist for services that tests start.

## How to tune

These constants became `config.toml` settings in [the tuning slice](tuning-slice.md),
with the values above as defaults; README "Tuning scheduling" gives the loop.

## Contract questions

The most literal reading was implemented in each case.

1. **Learned sizes apply to all runs.** "Without a declared or learned size, a
   request has size 1" was read as learned sizes replacing the default for pinned
   runs too, not only placed ones. Leases are never sized by learning.
2. **"Too large" with unreachable members.** "Larger than every eligible member's
   capacity" fails at once even if an unreachable member might be larger.
3. **First come, first served across members.** The contract orders work per
   member. Should a placeable request reserve capacity fleet-wide ahead of later
   requests?
4. **A saturated caller.** "Never sent somewhere already saturated" was applied to
   the caller too. Its own new placeable work goes elsewhere, even into a cold copy,
   rather than waiting for its load to drop.
5. **Hold-limit default.** It still uses the label's median run time across all
   members (S1), not the per-member estimate of where the request lands.
6. **Early failure.** "May fail early" was not taken; see "Limits".

## Estimate families and learned speed ratios

A later item, driven by the first real `elsecall report` (2026-11-03). Consumers
label each unit of work (`acme test Formatting`, `acme test FeatureModule`),
so each label has few runs and its estimates fell back to priors; and the master's
measured speed drifted 29% under load, so converting a label's runs between members
by measured speed was off by 0.8 to 3× per label.

### Decisions

#### The order of run estimates (`learning/run_time.rs`)
For a member and warmth, the first that has enough trusted runs:

| Level | `run_from` | From | Needs |
| --- | --- | --- | --- |
| `label` | `member_runs` | the label's runs on the member | `minimum_runs` (3) |
| `converted` | `fleet_runs` | the label's runs on every member, converted | `minimum_runs` |
| `plan` | `plan_runs` | a declared sequential plan: the line of run time against units through the observed plans of the label and its rule, every member, converted ([workload metadata](workload-metadata-contract.md#plan-run-estimates)) | `minimum_family_runs`, in two sizes |
| `family` | `family_runs` | other labels of the same rule, on the member | `minimum_family_runs` (3) |
| `family` | `fleet_family_runs` | other labels of the same rule, every member, converted | `minimum_family_runs` |
| `prior` | `prior` | `run_prior` at the member's speed | |

The level says whose runs an estimate rests on (the label's, its family's, or
none), and `converted` that they are the label's own from other members; the
source says where they ran. `Level` is ordered as the chain is tried.

A cold copy takes a learned cold estimate only when it rests on history at least
as close to the label as the warm estimate does; otherwise the warm estimate times
`cold_run_factor`, as before. So a family's few cold runs never override the
label's own warm runs (a review caught that regression: 3 cold family runs of
5 s would have stood in for a label that takes 300 s warm).

- **The label's own history before its family.** The request listed label, family,
  converted, prior. The label's runs elsewhere, converted, come before the family
  on the member, because the family is the cruder estimate: on the master,
  `acme build *` held labels from 3 ms to 120 s. Replaying the order on the
  real history before building it, the label converted beat the family on the
  member in both runs where both existed (4.1 s against 11.9 s predicted for a
  2.2 s run; 8.2 s against 15.0 s for a 10.3 s one). A
  family is used when the label has too little history *anywhere*, which is how
  "too little history" was read; the family on the member still comes before the
  family converted.
- **A family is a rule's labels.** The record of every request keeps the rule it
  matched (`settings.rule.match`); the family is that pattern's other labels,
  read from records by the pattern. Keyed by pattern as labels are by text, so the
  same pattern in two repositories is one family, like the same label is today. A
  request without a rule has no family.
- **The label's few runs and the family combine as stand-ins.** With `k` of the
  label's runs (converted to the member) and `minimum_runs` needed, the family's
  median fills the other `minimum_runs − k` places, and the estimate is the
  geometric mean of all of them. Run times vary by factors, so the mean is
  geometric. At `k = minimum_runs` the label's own history takes over completely,
  at the same threshold as before.
- **Short runs leave families out.** Family runs shorter than `short_run` (1 s) do
  not count: the real `acme build *` family held no-op labels of 3 ms that
  pulled a 21 s build's family estimate to 0 s. The label's own short runs still
  count for it.

#### Learned speed ratios (`learning/ratios.rs`)
- A label counts for a pair of members when each has `minimum_runs` warm runs of
  it with a median of at least `short_run`. The pair's ratio is the median of
  those labels' ratios, in log space, so that one way is exactly the inverse of
  the other.
- Labels of the request's own family come first: when `minimum_ratio_labels` (1)
  of them count, only they do. Otherwise any labels, from the same minimum.
  Otherwise measured speeds, as before.
- Learned from the latest 2,000 completed runs of every label within `recency`,
  read with decisions stripped in SQLite (`json_set` to null), on a read-only
  connection of its own, as replay reads, and kept for a minute
  (`service/ratio_cache.rs`): a ratio between two members moves far more slowly
  than requests arrive.
- A family's runs are read through an expression index on the rule
  (`requests_by_rule`, completed requests only), with the label's own runs and
  runs shorter than `short_run` left out in SQL, so a rare family is found
  without reading every recent record, and frequent no-ops cannot crowd out the
  read's bound. The index is built when the service first opens records.

#### Defaults
`minimum_family_runs` and the per-member runs a ratio needs reuse today's 3;
`short_run` is the 1 s that learned sizes already ignore below. A default that
kept today's behaviour exactly would turn both features off, so
`minimum_ratio_labels` defaults to 1 rather than off.

#### Recording the level
- `run_from` gains `family_runs` and `fleet_family_runs` (`samples`,
  `label_samples`), and `fleet_runs` counts its `conversions` (`learned`,
  `measured`; absent, so zero, in earlier records). Every candidate's
  `estimate.run_from` is written with its `level`, which reading ignores, since it
  follows from the source. So unchanged parameters replay every record, earlier
  ones included, exactly as before.
- `explain` says it in words: "run 1.5 s (family: 3 runs of other labels there)",
  "learned from 3 runs, converted by learned speed ratios".
- `report`: facts carry the family and the run level; `accuracy.run.by_level`
  groups placement's estimates by level. `replay` names each flip's winning
  `run_level`.

#### The report judges conversion by what placement would use
- `speed_conversion` now compares each label's observed ratio with the ratio the
  master's runs of *other* labels teach, learned as placement learns it: what
  placement would convert by had the label not run on one member. It keeps the
  measured ratio and `measured_error` beside it.
- Its finding also needs the conversion to miss the slower member's median by 2 s,
  as estimate findings do: a 3× error on 15 ms runs misleads nothing.
- `accuracy.backtest` estimates every completed run of the period again, from the
  master's runs requested before it, today's way and the baseline way (the label
  alone, converted by measured speed). A recorded prediction says how the
  estimator of its time did; a backtest asks this version the same question of the
  same runs, so the change can be judged on real history, before and after.
  `definitions` is now 2.

### Automated evidence

`mise run check` passes. Unit tests cover:
- the chain's order and every level, the stand-in combination of a label's few
  runs with its family (and its own history taking over at `minimum_runs`), short
  runs left out of families but not of the label, no family without a rule, and
  a family's cold runs never overriding the label's warm ones
  (`learning/run_time.rs`);
- learned ratios: the log median and its exact inverse, family labels first,
  `minimum_ratio_labels`, `short_run`, warm runs only, the window, and leaving a
  label out (`learning/ratios.rs`); the minute-long cache;
- the three new parameters' defaults, ranges and round trip;
- records: family reads newest first without decisions, the label's own and
  short runs left out, and the query plan using `requests_by_rule`;
- `run_from` written with its level and read back equal, and an earlier
  `fleet_runs` without conversions; replay of every record generation with the
  new sources, and each flip's `run_level`;
- the report: groups by level, conversion against the ratio other labels teach,
  the finding's message and 2 s floor, and the backtest (master's history only,
  baseline, levels, cold factor, the 500-run bound).

`tests/placement.rs`, `sparse_labels_take_their_familys_runs_and_learned_speed_ratios`:
real services in temporary homes, with measured speeds that say the members are
alike while the work runs 5× faster on bravo. After three runs of `test one` on
each member, a new `test two` (rule `test *`) is estimated from its family on both
members (1.5 s and 0.3 s) and goes to bravo, where both priors would have kept it
on the caller; `test three`, run only on alpha, converts to bravo by the ratio
`test one` taught (`conversions: {learned: 3}`, under 0.8 s where measured speeds
say 1.5 s); an unruled `lint` gets the prior. Replay reproduces all three
decisions, and the report groups them by level and backtests the family estimate
against the baseline's prior.

### Installed-system evidence

On 2026-11-03 at 16:01 UTC the release build of this item (commit `f6b4baa`,
sha256 `36ef3cc717c32b96…`) was installed on the Mac mini (`mac-mini`,
master) once it held no Elsecall work: the parallel acme session kept it busy
for most of an hour before. Placement, records and reports are the master's, so
the devbox kept the report item's build; its facts carry no family, and it answered
the report as before. `config.toml` unchanged, so every value is its default.

**Before and after over the same history.** `elsecall report --since
2026-11-02T15:00:00Z` on the report item's build (sha256 `17f5ec33…`, 15:05 UTC)
and on this one (16:01 UTC, a few more acme runs in between):

*Speed conversion*, per label (observed ratio, slower ÷ faster by measured speed,
against what conversion says; 1 is right):

| Label | Observed | Before: measured | Error | After: learned (labels) | Error |
| --- | --- | --- | --- | --- | --- |
| `synthetic hash` | 0.81 | 1.24 | 0.65 | 0.78 (1) | 1.03 |
| `acme build Uploads` | 0.99 | 1.24 | 0.80 | 0.80 (2) | 1.23 |
| `synthetic burn` | 1.04 | 1.24 | 0.84 | 0.82 (1) | 1.28 |
| `elsecall housekeeping` (15–60 ms runs) | 3.07 → 4.20 | 1.24 | 2.47 | 0.80 (2) | 5.25 |

Over the three labels that run for seconds, the mean error factor fell from 1.33×
to 1.18× (mean |ln error| 0.27 to 0.16): one much better, one the same either side
of 1, one slightly worse. `speed_conversion` findings went from 3 to 1; one of the
two that went is the sub-second `elsecall housekeeping`, now below the finding's
2 s floor rather than better converted. The learned ratio rests on one or two
labels: the master's history has few labels run on both members.

*Backtest* (new; `accuracy.backtest`, 132 completed runs of the period estimated
again from the master's earlier runs; error factor: median times off either way):

| Level of today's estimate | Runs | Today: factor / MAE / priors | Baseline (before): factor / MAE / priors |
| --- | --- | --- | --- |
| all | 132 | 2.54× / 70 s / 48 | 3.89× / 80 s / 75 |
| `label` | 37 | 1.34× / 10 s / 0 | the same |
| `converted` | 20 | 1.26× / 11.5 s / 0 | 1.49× / 15.3 s / 0 |
| `family` | 27 | 1.96× / 132 s / 0 | 4.34× / 177 s / 27 |
| `prior` | 48 | the same | the same |

The 27 runs a family now estimates were all priors before; families halve their
error but run low (ratio median 1.8: acme's long screen-suite tests against
their family's median). The 48 left on the prior are first runs of a family and
sub-second labels without one.

**Replay.** `elsecall replay --since 2026-11-02T15:00:00Z` reproduced all 26
placement decisions with no flips, before and after the install (22 S4 records
from their recorded estimates, 4 from arrival inputs).

**A family estimate, recorded.** In a scratch repository whose `.elsecall.toml`
has one rule, `evidence *`, three 1.2 s runs of `evidence a` were followed by a
placeable `evidence b` (`r-e02c9ff829b5`), which stayed on the mini:

```text
reason: The calling member mac-mini completes in 1.2 s; macbook-pro would take 123 s, not earlier by the 2.0 s margin for leaving the caller.
* mac-mini run_from {"from": "family_runs", "samples": 3, "label_samples": 0, "conversions": {"learned": 0, "measured": 0}, "level": "family"}
    Starts now, completes in 1.2 s: run 1.2 s (family: 3 runs of other labels there).
  macbook-pro run_from {"from": "fleet_family_runs", "samples": 3, "label_samples": 0, "conversions": {"learned": 3, "measured": 0}, "level": "family"}
    Starts now, completes in 123 s: first sync 120 s (prior) + cold run 2.9 s (family: 3 runs of other labels, converted by learned speed ratios × 3 cold).
```

It took 1.215 s against 1.21 s predicted; before, the prior was 59 s. Replay
reproduced it from its arrival inputs, and the report's `run.by_level` counts it
as `family` (ratio 1.00).

### Limits

- The hold limit's default still uses the label's median across members (S1),
  without families: the `acme test Formatting` finding (82 s predicted, 12.5 s
  taken) is that estimate, for pinned runs, and its runs went from 70–82 s (cold)
  to 12 s (warm), which neither a family nor a ratio fixes.
- History lives on the master; runs other members made themselves teach neither
  families nor ratios, and the backtest and `speed_conversion` learn from the
  master's requests to match.
- The backtest counts runs requested before each run, even if still going then;
  it estimates without contention, at the members' mean measured speeds over the
  period, and learns only from runs within the report's period, so runs early in
  a period have less history than placement had (for both estimates alike). It
  is computed off the async runtime.
- Learned ratios are up to a minute old.
- Replay still refuses `[estimates]` changes: records hold what history said.
  The backtest is the way to judge them.

### Contract questions

1. **Order.** The label's runs on other members, converted, come before its family
   on the member (see Decisions). Keep, or put the family first as listed?
2. **Family scope.** Families are keyed by rule pattern alone, across
   repositories, as labels are. Should a rule be able to name its family (`family =
   "…"`) or opt out, for a catch-all `*` rule?
3. **Hold limit.** Should the default hold limit also use family estimates?
4. **One shared label is enough.** `minimum_ratio_labels` defaults to 1: a single
   label both members ran three times replaces measured speed between them. An
   I/O-bound label could teach a ratio near 1 across a CPU difference; then again,
   the benchmark misjudged every label on the real fleet. Raise the default to 2?

## Running work's remaining time

A later item, driven by a real request on 2026-11-04. An acme test
(`r-b3832e93b295`, 3 cores, placeable, on the master) **waited 410 s on the Mac
mini while `macbook-pro` was idle**. The mini's 9 cores were held by another
session's run, `09c affected tests` (`r-e97cc3dc78dd`), that had no expected run
time, and a 9-core request without one waited ahead. Each counted the fixed 60 s
of `unknown_remaining`, so the mini's wait was 120 s on every rescore: 190 s
against the devbox's 173 s on arrival, within the 10% keep-local margin. The
holding run had already run 644 s and went on for 2,256 s in all. The request moved
only once its hold limit forced it: "macbook-pro completes earliest, in 173 s;
the calling member cannot start it in time."

### Decisions

#### Running work's remaining time, in order (`placement.rs`, `Held::holds_for`)
1. **Its member's expectation**, as before: the label's median run time (S1), less
   the time it has run. Unchanged, so decisions that had one score as they did.
2. **What history expects of its whole run** (`learned_run`), less the time it has
   run: the chain new work's run estimates take (the label's runs on the member,
   converted from other members, then its family's), on the member it runs on.
   - **Runs and leases.** A lease is estimated from leases of its label and family,
     a run from runs: a lease's time is how long its holder held it.
     `learning::samples_of` reads a kind; `samples` stays the runs'.
   - **Local or placed.** Work running on the master is looked up in its own book.
     Work in a member's quote (`holding`, with its `run_id`, `label` and
     `running_ms`) takes its family and its copy's warmth from the master's record
     only when the master placed it on that member; anything else a quote names
     is that member's own work, with its label only, so no family.
   - **Cold copies.** As for new work, through one shared choice
     (`LearnedRun::for_copy`): a learned cold estimate, else the warm one marked
     `cold_from_warm`, which scoring multiplies by `cold_run_factor`, so `replay
     --set cold_run_factor=…` changes running work as it changes new work.
   - **Learned once per running request on each member**
     (`service/running_estimates.rs`), after each round of quotes and before
     scoring, outside the book (learning reads records). Kept by member and run
     ID, since a member's quote names IDs the member chose and must never stand
     for the master's own work; forgotten once the request no longer holds
     capacity there. Requests that ask the same (kind, label, family, member,
     warmth) share one read, at most 8 such reads a round (the rest are learned
     in the next rounds and meanwhile count as work without an estimate), and
     history that knew nothing is asked again after a minute. A run placed on a
     member whose copy's warmth has not come back yet waits for it.
   - **Elapsed time is the command's.** A member counts a placed run's time from
     when it admitted it, sync included, while learned runs leave the sync out.
     For runs the master placed, it subtracts the sync time the caller reported,
     and counts nothing before the command started.
3. **No estimate at all:** `unknown_remaining`, growing with the time it has run:
   `max(unknown_remaining, unknown_remaining_per_elapsed × elapsed)`.

**Work that outlived its estimate** (1 or 2) is treated as work without one (3),
rather than ending now. Before, such work freed its cores in every scoring, so a
request behind a run that overran its estimate waited for its hold limit, the same
failure as the incident's; and a wrong estimate would have been worse than none
(the incident's run, with a 300 s family estimate, would have counted 0 s at 644 s
instead of 322 s). Its member's expectation has elapsed time taken out already, so
work counts as past it when what is left is zero and elapsed time is recorded;
records without it score as they did.

Queued work without an expected time still counts `unknown_remaining`: it has not
run, so it has no elapsed time to grow from.

`Held` keeps one `Expectation` (its member's, learned, or none), so a record can
never be read as having two; records keep it as `remaining_ms` and `learned_run`,
as before.

#### Why a share of elapsed time, and why half
- **The principle.** For durations with a heavy tail, how long work has run is the
  best predictor of how much longer it runs. Without other information, the time
  already elapsed is a uniformly random share of the whole (Gott's argument), so
  the median remaining time is the elapsed time itself (a share of 1), and work that
  has run long is likely to keep running. A fixed assumption says the opposite: that
  work 37 minutes in ends within a minute as surely as work just started.
- **The real fleet says less than 1.** Over the master's latest 483 completed runs
  and leases (2026-11-03 08:07 to 2026-11-04 08:00), among the work still running
  at each point, the median of remaining ÷ elapsed was 1.88 at 60 s, **0.53 at
  120 s** and **0.37 at 300 s** (and 0.85 for the 4 still running at 600 s). Much of
  that history is acme tests that run 150 to 250 s, so their remaining time
  collapses past 120 s; work without an estimate is, by definition, not that.
- **0.5** is the observed median where the rule starts to matter and below the
  scale-free 1, so it errs towards keeping work local. It also makes the rule
  continuous with today's: half of 120 s is the 60 s floor, so nothing changes for
  work that has run less than two minutes.
- **0** turns growth off, restoring the fixed assumption; the range is 0 to 10.

#### Rescores move work without new rules
The moving rule is unchanged: a waiting request moves when another member can
start it now and completes earlier by the moving margin. With a wait that grows,
each second's rescore sees the local completion grow, and the request moves once
it passes the idle member's completion by the margin, rather than when its place
can no longer start it in time.

#### Records and replay
- Each piece of running work in a candidate's `inputs.holding` gains `elapsed_ms`,
  and `learned_run` (`ms`, its source as `from`, `samples`, …, and
  `cold_from_warm`) when its member expected nothing. Both are optional and left out when absent, so
  earlier records read as before.
- Scoring derives the remaining time from them, so replay re-derives it: `replay
  --set unknown_remaining_per_elapsed=0` shows what the fixed assumption would have
  done to each decision recorded since.
- **Records from before this item** hold no `elapsed_ms`: their work without an
  estimate keeps `unknown_remaining` fixed under any parameters, exactly as it was
  scored, and their recorded parameters lack the new key, which reads as its
  default. Their `why` texts are unchanged too, which replay of the last-scoring
  generation compares word for word.
- `explain` says it in the candidate's `why`: "wait 322 s behind running work:
  9 cores 644 s in, 322 s more (no estimate)", "(family: 3 runs of other labels
  there)" for a learned one, and "(past its 1.0 s estimate)" or "(past its expected
  run time)" for work that outlived one; work within its member's expectation is
  not listed, as before.

### Automated evidence

`mise run check` passes. Unit tests cover:
- the rule: the floor, growth from 120 s at 0.5, monotone, 0 turning it off, and a
  record without elapsed time keeping the fixed value (`parameters.rs`); the new
  parameter's default, range and round trip;
- the order of sources: the member's expectation, then the learned run less elapsed
  (scaled by `cold_run_factor` at scoring for a cold copy), then the grown
  assumption, and work that outlived either estimate taking the grown assumption
  (`placement.rs`); the cold choice shared with new work, and `Held`'s record
  format, old and new; a wait that grows with elapsed time and moves the winner, and
  the `why` text, unchanged for records without elapsed time;
- leases learned from leases and runs from runs (`learning.rs`);
- replay: a record with elapsed time and a family-learned run reproduces, and flips
  under `unknown_remaining_per_elapsed=0`; the same record stripped of elapsed time
  and of the new key scores as the fixed assumption did;
- **the real request**: `r-b3832e93b295`'s recorded arrival scoring reproduces
  as recorded (190 s against 173 s, the mini), and with the 644.4 s the holding run
  had run, the mini's wait is 382.2 s (322.2 s, then 60 s for the request ahead) and
  `macbook-pro` wins on arrival: "macbook-pro completes earliest, in 173 s,
  against 452 s on the calling member mac-mini."

`tests/placement.rs`, real services in temporary homes:

| Test | Shows |
| --- | --- |
| `a_request_behind_long_running_work_without_an_estimate_moves_well_before_its_hold_limit` | With `unknown_remaining = "1s"` and the default growth, a placeable request behind a lease nobody has an estimate for waits on the caller on arrival (about 11 s against 13 s), then moves to the idle member once the lease has run about 10 s, far before its 120 s hold limit. The arrival's `holding` has `elapsed_ms` and neither `remaining_ms` nor `learned_run`; the last scoring's `why` says "(no estimate)". Replay reproduces it. |
| `running_work_with_a_family_estimate_is_scored_from_it` | With `unknown_remaining = "10m"`, a pinned 1 s `hold two` without runs of its own holds the caller; its family (`hold *`, three 2 s runs of `hold one`) gives `learned_run` `family_runs`, so the caller's wait is at most the family's run, not 10 minutes, and the work stays there. Then a `hold three` the master placed on bravo, as bravo's quote lists it, takes `family_runs` from bravo's warm runs of `hold one`, with no elapsed time before its sync was reported. Replay reproduces both. (While writing it, a `hold two` of 2 s outlived its 1 s family estimate and fell back to the 10-minute assumption, as designed, sending the request to bravo.) |

### Replay against real history

The master's 28 placement decisions (2026-11-03 08:35 to 2026-11-04, every one
retained: 6 with arrival inputs, 22 S4 records with estimates only), exported with
`elsecall explain` and replayed by this item's code in a test that read them,
without installing it:

| `--set` | Replayed | Flipped | Not replayable |
| --- | --- | --- | --- |
| none (today's parameters) | 28 | 0 | 0 |
| `unknown_remaining_per_elapsed=0` | 6 | 0 | 22 `recorded_before_inputs` |
| `unknown_remaining_per_elapsed=1` | 6 | 0 | 22 `recorded_before_inputs` |

The installed build (before this item) also replays 28 of 28 with no flips. No
earlier record holds elapsed time, so the new parameter changes none of them,
as intended.

**`r-b3832e93b295` under the new logic**, from its recorded arrival inputs and the
start of the run holding the mini (`r-e97cc3dc78dd`, started 07:24:23.340, so
644.4 s in at the arrival scoring, 07:35:07.753; it ran 2,256 s in all):

| | Recorded | New logic |
| --- | --- | --- |
| Running 9 cores, no estimate | 60 s | 322.2 s (0.5 × 644.4 s) |
| Queued 9 cores ahead, no estimate | 60 s | 60 s |
| Mini's wait + run | 120 s + 70.1 s = 190.1 s | 382.2 s + 70.1 s = 452.3 s |
| `macbook-pro` (cold with history) | 172.9 s | 172.9 s |
| Winner on arrival | the mini (not earlier by the 19 s margin) | `macbook-pro`, by 279 s |

So it would have gone to `macbook-pro` at 07:35:08 instead of 07:41:58, 410 s
sooner. A unit test (`the_real_request_that_waited_410_s_…`) scores exactly this.
The record itself cannot replay it, since it holds no elapsed time.

### Installed-system evidence

Deferred to the batch release verification. Elsecall is in production use on both
machines (the master at a load average near 100), so it had no idle window for an
install, and parallel items would overwrite each other's installs. The checks for
that verification, on the master (`mac-mini`) with this build installed:

1. `elsecall replay`: every decision reproduces (`flipped` 0, nothing
   `not_reproduced`).
2. **The probe.** In a scratch repository (`git init`, one commit), three
   `elsecall run --pinned --label probe-move -- sleep 5` (history on the master).
   Then `elsecall run --label probe-unknown --size 9 -- sleep 600` in the
   background, filling the master. At once,
   `elsecall run --placeable --label probe-move --run-id-file id -- sleep 5`.
   Expected: on arrival the master wins (60 s + 5 s, against `macbook-pro`'s
   first sync of a new repository, 120 s prior, plus about 3 × 6 s cold); as the
   probe runs, the master's wait grows as half its elapsed time, and the request
   moves to `macbook-pro` at about 300 s, against about 540 s (its hold limit's
   last start) before. Check with `elsecall explain $(cat id)`: `arrival.winner`
   is the master, the decision's `reason` is "macbook-pro completes earliest…",
   the master's `inputs.holding[0]` has `elapsed_ms` near 300,000 with no
   `remaining_ms` or `learned_run`, and its `why` says "behind running work:
   9 cores … (no estimate)". Then stop the probe (interrupt its `elsecall run`).
3. `elsecall replay --since <the probe's start>` reproduces the probe's decision,
   and `--set unknown_remaining_per_elapsed=0` keeps its arrival winner (the
   arrival scoring had grown nothing yet).
4. `macbook-pro` runs only the 5 s `sleep`, so the devbox load stays light.

### Limits

- Queued work without an expected time still counts `unknown_remaining`; only
  running work grows. In the incident, the 9-core request ahead was 60 s of the
  mini's 382 s.
- Work a member requested itself has its label but no family: quotes do not carry
  the rule, and the master learns families from its own records.
- A member's quote is up to a second old, so its `running_ms` is too.
- When the root of nested work has ended, `holding` names one of its nested
  requests, chosen arbitrarily (as before); its elapsed time and learned run are
  that request's.
- Outliving an estimate is a step: just before, little is left; just after,
  `max(unknown_remaining, 0.5 × elapsed)`. With a large `unknown_remaining` the step
  is large.

### Contract questions

1. **Outlived member expectations.** Work past its member's S1 expectation is now
   treated as work without one, like work past a learned estimate. Keep it for both,
   or only for learned estimates?
2. **The growth share.** 0.5 rests on a day of history dominated by acme tests.
   Revisit it with `elsecall report` once records with elapsed time accumulate?
3. **Queued work.** Should queued requests without an expected time also take the
   chain's estimate, as running work now does?

## Lost runs weigh against their member

A later item, from Elsecall's own records of 2026-11-06 to 2026-11-09.
`macbook-pro` lost 76 placed runs to `connection_lost` (12.6 run-hours of work),
and hid about 60 more as `holder_exited`: their callers had exited 125 with
`connection_lost`, but their report to the master's stalled service timed out. The
rate was steady at 0.6 to 2 lost runs per hour of placed work there, so loss
grew with length. Of the runs placed there, 4% of single-module test runs were
lost, against 44% of batches of 15 or more modules, which ran for 30 to 115
minutes. The mini lost none of its own 59 such batches. Placement weighed nothing
of this: long batches went to the MacBook on near ties.

### Decisions

- **The loss rate.** `loss_history.rs` counts, from the master's records, the
  placed runs requested within `[estimates] loss_window` (24 h) that ended
  `connection_lost` on each member, over the `run_ms` of every placed run there
  (sync included, lost ones too) and the time so far of those still running. It
  is read with the sync history, from the latest 500 placed records since the
  longer of `recency` and `loss_window`, when a request arrives, and kept while
  the request waits, as its run and sync history are. Synthetic work never
  counts.
- **Restarts on this side are not the member's.** Recovery after a service
  restart closes the placed runs it finds as `connection_lost`, as before, and
  now marks them `placed.interrupted: "service_restart"`. Those count neither
  as losses nor as placed work: an upgrade or settings change of the master
  says nothing about the member. A member's own restart still counts against
  it, as the caller sees only the member going silent; the member's record
  carries the marker.
- **Its weight.** A lost run is run again from the start, so sync and run of `t`
  on a member losing `λ` runs per second of placed work take `(e^(λt) − 1) / λ`
  on average, the expected time of a task that has to run `t` without an
  interruption. The exposure counts at least `[placement] minimum_loss_exposure`
  (1 h), so one early loss does not read as a member that loses all its work.
  The wait is not weighed, and neither is eligibility, which still asks only
  whether the request can start within its hold limit. The result is capped at 30
  days.
- **Records.** Each candidate's `inputs.learned.losses` keeps
  `{"lost", "run_ms"}`, and `estimate.loss_factor` the factor applied. The `why`
  ends with ", sync and run 1.80× for 25 lost of its placed runs in 28.0 h of
  them". `replay` re-scores with the recorded losses, and `--set
  minimum_loss_exposure=…` replays; `loss_window` is an `[estimates]` key and is
  refused, as the others are, and so it only takes effect live (a tuning search
  over it changes nothing it can measure from records).
- **Not done here.** Placement does not resume, re-place or split a lost run;
  that would need the member to keep the command alive and the caller to
  reattach, a protocol change. The cause of these losses is the calling member
  itself: 58 of the 59 lost runs ended in a minute in which the master's own
  service missed five or more of its 60 load samples (7.4% of busy minutes did),
  and such stalls came with memory pressure (20% of its minutes against 4%). A
  stall over the 10 s run-link grace ends the run on the member however healthy
  the link. Weighing losses keeps long work off the member until that is fixed,
  and stops weighing it once the member stops losing runs.

### Backtest

Of the 51 batches labelled with 15 or more modules that were placed on
`macbook-pro` on 2026-11-06 to 2026-11-09, 7 went there because the mini was
under memory pressure. Rescoring the other 44 with a plan estimate of 400 s plus
100 s a module on both members, and the MacBook's losses over the 24 h before each
decision, keeps 29 on the mini: 8 of them were lost to `connection_lost` on the
MacBook and 11 more ended `holder_exited`. The other 15 stay on the MacBook,
because the MacBook had lost nothing yet or the mini was estimated 1.4 to 4 times
slower under contention.
