# Tuning: parameters, recorded inputs and load history

Fleet service settings now also travel with E1 generations through an optional typed
attachment; see [service-config-sync-slice.md](service-config-sync-slice.md) for its
schema, migration and automatic application. The original slice decisions below
describe ordinary mirrored paths and pre-migration local configuration.

The first step towards a scheduling tuning loop, after S4
([Placement](placement-slice.md)). It delivers:

- the placement and admission constants as validated `config.toml` settings, with
  S4's values as defaults;
- every input a placement decision scored with in its record, with the parameter
  values it used, so that a later replay can re-score recorded decisions under
  changed parameters ([Fleet scheduling](fleet-scheduling.md), "Observability");
- a per-minute load history on every member, in the fleet view.

`elsecall report` and `elsecall replay` build on it in later items.

## Decisions

### Parameters are settings with today's values as defaults (`parameters.rs`)
- `config.toml` gains `[placement]`, `[admission]` and `[estimates]`. Every key is
  optional, and its default is the constant S4 shipped with:

  | Key | Default | Was |
  | --- | --- | --- |
  | `placement.keep_local_margin`, `keep_local_margin_floor` | 0.1, `2s` | `caller_margin`; `moving_margin` follows it |
  | `placement.run_prior` | `60s` | `DEFAULT_WORK` |
  | `placement.sync_prior_warm`, `_cold_with_history`, `_cold` | `3s`, `30s`, `120s` | `WARM_SYNC`, `COLD_SYNC_WITH_HISTORY`, `COLD_SYNC` |
  | `placement.cold_run_factor` | 3.0 | `COLD_RUN_FACTOR` |
  | `placement.unknown_remaining` | `60s` | `UNKNOWN_REMAINING` |
  | `placement.unknown_remaining_per_elapsed` | 0.5 | added by running work's remaining time ([Placement](placement-slice.md)) |
  | `placement.maximum_slowdown` | 4.0 | `MAXIMUM_SLOWDOWN` |
  | `placement.reconsider_every` | `1s` | `RECONSIDER_EVERY` (the requote interval) |
  | `placement.minimum_loss_exposure` | `1h` | added by loss-weighted placement ([Placement](placement-slice.md)) |
  | `admission.saturated_utilization` | 0.9 | `SATURATED_UTILIZATION` |
  | `admission.memory_pressure` | `warning` | blocking at warning or worse |
  | `estimates.recency`, `window`, `minimum_runs` | `14d`, 20, 3 | `RECENCY`, `WINDOW`, `MINIMUM_SAMPLES` |
  | `estimates.minimum_workspace_syncs`, `_member_syncs`, `_repository_syncs` | 2, 3, 1 | literals in `sync_history.rs` |
  | `estimates.minimum_family_runs`, `minimum_ratio_labels`, `short_run` | 3, 1, `1s` | added by estimate families ([Placement](placement-slice.md)) |
  | `estimates.loss_window` | `24h` | added by loss-weighted placement ([Placement](placement-slice.md)) |

- **Records read back tolerantly.** Parameters recorded in decisions take defaults
  for keys they lack and ignore keys they do not know, so a later version adding
  a key, or a rollback, never makes a record unreadable. Only `config.toml` and
  `--set` are strict.
- **Validated as a whole.** Each value has a documented range, and the sample
  minimums may not exceed the window. An invalid value, or an unknown key, stops
  the service with `invalid_configuration`, as other settings do.
- **Whose values govern.** Placement runs on the master, so the master's
  `[placement]` and `[estimates]` decide. Each member's `[admission]` gates its own
  admission, and the master scores another member by the gate that member reported.
  A member's own `[estimates]` also learn sizes for work requested there.
- **Not settings.** The 3-sample saturation window, the speed measurement and the
  hold-limit estimate stay constants. Replay could not re-derive a window it never
  recorded, and the others are not decisions.
- **One parser for both uses.** `Overrides::parse_settings` reads `section.key=value`
  settings (the section may be left out, since every key is unique) through the
  same validation as `config.toml`, so a later `replay --set` accepts exactly the
  values the service would.

### Decisions record everything they scored with
Before, a decision recorded each candidate's estimate (its components and their
sources), but not all inputs: the running and queued work behind the wait, what
history said before slowdown and cold factors, the hold limit left, or the
scoring's own winner. Replay needs them, so records now hold:

- `decision.parameters`: the values this member decided with, on every decision,
  placed or not;
- `decision.scoring`: when the scoring ran, the size it scored, the hold limit left
  (`hold_left_ms`) and its winner. A request already waiting somewhere moves to a
  new winner only by the moving margin, so the winner can differ from where the
  request ran;
- each candidate's `inputs`: whether it is the caller, its measured speed, window
  utilization, the work holding its capacity (`holding`, with `remaining_ms`) and
  queued ahead (`ahead`, with `run_ms`), its copy, and what history said
  (`learned.warm_run`, `cold_run` and `sync`, each with `ms` and its source), and
  the `admission` thresholds its own gate used, which members now report in
  their quotes;
- `decision.arrival`: what the first scoring expected of each candidate. The
  recorded decision is the last scoring, just before the start, whose expected
  wait is about zero; the wait predicted on arrival is the one to check.

**Exact by construction.** Scoring rounds every duration it is given to whole
milliseconds before it scores, and records exactly those values, in milliseconds.
Floats (utilization, speed) are JSON numbers that read back bit for bit, since
`serde_json` now parses with `float_roundtrip`. History is resolved into
`learned` before scoring, so scoring is a pure function of the record. A unit test
scores, serializes the inputs, reads them back, and scores again to the same
verdict.

Records from before this item have estimates but no inputs, scoring or
parameters (they used the defaults). Of their decisions, only the choice between
recorded completions can be repeated: changing the keep-local margin, from each
candidate's `estimate.completion_seconds`, `eligible` and `copy`, with the caller
being the candidate named like the record's `calling_member`.

### A load history on every member (`load_log/`)
- **What.** One row per minute in `load.sqlite` (owner-only, WAL), folded from the
  per-second samples admission takes anyway: CPU mean and maximum, outside load
  (utilization beyond the share Elsecall's grants hold), Elsecall's cores in use
  (mean and maximum) of capacity and weighted cores, the most requests queued,
  seconds Elsecall work held cores, seconds the gate was closed by outside load (and of those, seconds Elsecall work
  held cores), seconds of memory pressure, the worst memory pressure and thermal
  state, and the measured speed. Machine state only: no processes, arguments,
  environment values or paths.
- **Cheap.** Folding a second is arithmetic under the scheduler's existing sample;
  one insert a minute, written outside the admission lock. 30 days, as long as
  records, is about 43,000 small rows; it is pruned as the service starts and
  daily after.
- **Clocks.** The sampler skips missed ticks rather than bursting, so a minute
  never counts more than its seconds. While the wall clock is behind a minute
  already folded, as after a clock step back, seconds are dropped rather than
  written over stored minutes.
- **Each member keeps its own.** It survives the master being down or moving, and
  reports ask each member for its own (item 2). Buckets start at whole multiples of
  their width since the epoch, so members' buckets line up.
- **The fleet view.** `elsecall fleet` shows each member's `status.load_history`:
  `last_hour` and `last_day`, including the minute in progress, so it is live.

## Automated evidence

`mise run check` passes. Unit tests cover:
- parameter defaults, their round trip through records, `--set` parsing with and
  without sections, and every range check;
- the configuration file's sections and invalid values;
- the gate under changed thresholds, trust thresholds in run and sync learning;
- placement under changed margins and priors, and recorded inputs read back
  scoring to the identical verdict;
- folding seconds into minutes, minutes into aligned buckets, and the store.

`tests/placement.rs`,
`the_masters_margin_decides_and_explain_records_every_input`: real services in
temporary homes. A warm, twice-as-fast member wins by default (33 s against 60 s);
with `keep_local_margin = 0.5` on the master, and 0 on the member, the caller keeps
the work "not earlier by the 30 s margin". `explain` shows the parameters (set and
defaulted), the scoring, every candidate's inputs and the arrival estimate; a
pinned run records its admission parameters; `elsecall fleet` shows each member's
load history.

The full suite also showed an existing flake: under heavy machine load, a test
member occasionally misses the harness's 5 s start limit
(`tests/members/mod.rs:110`). It hit a different test in each of three full runs
and passed on every rerun.

## Installed-system evidence

On 2026-11-03 the release build of this item (commit `ca50bdc`, sha256
`7f4351fd992513de…`) was installed on both members with no `config.toml`
changes, so every value is its default. The devbox (`macbook-pro`) was installed
first, at 12:17:57 UTC. The Mac mini (`mac-mini`, master) followed once a
acme `test Search` run it was hosting had ended.

**A decision records what it scored with.** A light placeable run (`/bin/sh -c
echo …`, label `tuning evidence`) from a fresh scratch repository on the mini went
to the devbox. Outside work held the mini at 100% CPU just after its restart.
`elsecall explain r-a276049b55b8`, abridged:

```text
reason: macbook-pro completes earliest, in 378 s; the calling member cannot start it in time.
parameters.placement: {"keep_local_margin": 0.1, "keep_local_margin_floor": "2s", "run_prior": "1m",
  "sync_prior_warm": "3s", "sync_prior_cold_with_history": "30s", "sync_prior_cold": "2m",
  "cold_run_factor": 3.0, "unknown_remaining": "1m", "maximum_slowdown": 4.0, "reconsider_every": "1s"}
parameters.admission: {"saturated_utilization": 0.9, "memory_pressure": "warning"}
parameters.estimates: {"recency": "14days", "window": 20, "minimum_runs": 3,
  "minimum_workspace_syncs": 2, "minimum_member_syncs": 3, "minimum_repository_syncs": 1}
scoring: {"scored_at": "2026-11-03T12:19:07.061Z", "size": 1.0, "hold_left_ms": 599689, "winner": "macbook-pro"}
mac-mini  saturated | Saturated by outside load (100% CPU).
  inputs: {"caller": true, "speed": 0.9218315534667313, "utilization": 1.0, "holding": [], "ahead": [],
    "copy": {"state": "caller"}, "learned": {"warm_run": null},
    "admission": {"saturated_utilization": 0.9, "memory_pressure": "warning"}}
macbook-pro chosen open | Starts now, completes in 378 s: first sync 120 s (prior) + cold run 258 s (prior × 3 cold).
  inputs: {"caller": false, "speed": 0.6988104996172746, "utilization": 0.2900359789530436, "holding": [], "ahead": [],
    "copy": {"state": "remote", "warmth": "cold", "busy": false}, "learned": {"warm_run": null},
    "admission": {"saturated_utilization": 0.9, "memory_pressure": "warning"}}
arrival: winner macbook-pro; mac-mini not eligible (run 260.35 s, 4× slower under load);
  macbook-pro wait 0 s, sync 120 s, run 257.58 s, completion 377.58 s
```

The devbox's `admission` came in its quote, so each candidate carries the thresholds
of its own gate. The run itself took a fraction of a second against a 258 s cold
prior. That is the kind of error a report on estimate accuracy should surface.

**The fleet view shows both members' load.** `elsecall fleet` on the mini at
12:19:20 UTC, `status.load_history.last_hour` (rounded):

| Member | Seconds | CPU mean / max | Outside | Elsecall in use (mean / max) | Weighted / capacity | Busy | Saturated | Speed |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `mac-mini` | 22 | 0.88 / 1.00 | 0.88 | 0 / 0 | 12 / 9 | 0 s | 9 s | 0.922 |
| `macbook-pro` | 80 | 0.38 / 0.91 | 0.38 | 0.013 / 1 | 9 / 9 | 1 s | 0 s | 0.699 |

The devbox's `load.sqlite` then held two minutes, 12:17 with 3 s (the service
started at 12:17:57) and 12:18 with 60 s; the 80 s include the minute in progress.
`last_day` matched `last_hour`, since history starts with this version.

## Limits

- `decision.parameters` are the deciding member's. For a placed run, the target
  member's gate thresholds are its candidate's `inputs.admission`, as it reported
  them; members on earlier versions report none.
- The arrival estimate lives in memory until the decision is recorded; a request
  waiting across a service restart fails anyway.
- Changing a parameter needs `elsecall service restart`.
- Load history starts when this version is installed; there is none for earlier
  time.

## Replay (`elsecall replay`)

The third step: `elsecall replay [--since D] [--set KEY=VALUE ...]` scores this
member's recorded placement decisions again under changed parameters and reports
which flip and the estimated effect (README, "Tuning scheduling", for the loop and
the output). `elsecall report`, the second step, is its own item.

### Decisions

#### Pure, and exact before it is anything else (`scheduling/replay/`)
- **`change`**: `--set` goes through `Overrides::parse_settings` and the same range
  checks as `config.toml`. Each value applies over each decision's own recorded
  `parameters` (defaults for records from before them), so a replay asks "what if
  this key had been X", whatever the fleet ran with at the time.
- **`recorded`**: what a record lets replay score again, and scoring it. A scoring
  with inputs is scored by `placement::decide` itself from each candidate's
  recorded state and inputs; one recorded before inputs only by `placement::pick`
  from the recorded estimates. No scoring logic is duplicated.
- **Reproduce first.** Every decision is scored under its recorded parameters and
  must reproduce its recorded winner and every candidate's estimate exactly
  (compared as they read back from JSON, so seconds-as-floats compare bit for bit).
  One that does not is `not_reproduced`, listed by run ID, and never replayed: a
  flip is never an artefact of replay. Unchanged parameters therefore always
  report zero flips, and a nonzero `not_reproduced` means scoring or a record
  drifted. S4 records kept no scoring winner, only where the request started; one
  that started elsewhere than its estimates choose (a waiting request the moving
  margin kept) is `started_elsewhere_before_inputs` instead, so it does not read as
  drift.
- **`summary`**: flips, aggregates and what could not be replayed, grouped by why.
  It lists the latest 1000 flips (`flips_truncated` when more flipped) and run IDs
  per reason, so the answer stays far below the CLI's 8 MiB bound; counts and
  effects cover every decision.
- The HTTP adapter (`replay_api`, `POST /v1/replay`) reads the latest 5000
  placeable records since `--since` on a read-only SQLite connection of its own
  (WAL lets it read alongside writes), so a long replay never holds the
  scheduler's records connection, which admission writes through. It calls the
  pure function on a blocking thread. The CLI (`replay_cli`) validates `--since` and
  `--set` first, so a typo exits 2 without the service; the service validates the
  body again.

#### Which scoring is replayed
One per decision, the one that decided where the request went first:

| Record | Replayed from | Changes it can replay |
| --- | --- | --- |
| This item on | `arrival`, with its inputs | `[placement]`, `[admission]` |
| Tuning item | the last scoring (`last_scoring`), or `arrival` when it was the same scoring (the request started at once) | `[placement]`, `[admission]` |
| S4 | the recorded estimates (`recorded_estimates`) | `keep_local_margin`, `keep_local_margin_floor` |

Reconsiderations of a waiting request are not replayed: whether it would have
moved, a second at a time under the moving margin, depends on queues replay does
not simulate.

#### Records gain the arrival scoring's inputs
The tuning item recorded inputs only for the last scoring, which for a request
that waited is not the one that placed it, and for a `fleet_busy` request is taken
at the hold limit, with nothing left to place it in. Replay needs the arrival
scoring, so `decision.arrival` now also holds `size`, `hold_left_ms`, and each
candidate's `state` (as `elsecall runs` reports a member) and `inputs`: about
1 KB per candidate, more with long lists of running and queued work, so a
placeable decision's record roughly doubles. All are optional, so earlier records
read as before.

#### Gates under `[admission]` changes
The gate is part of a candidate's state, decided by that member's own service. To
replay a threshold, replay derives each member's gate again with `load::gate_of`
from its recorded latest sample (memory pressure), window utilization and cores in
use, under its recorded thresholds (`inputs.admission`) with the change in place:
as if every member had set the value. The service's own gate now uses the same
`load::own_share`, so the two cannot drift. Before replaying, every recorded gate
must follow from its recorded load under its recorded thresholds (a member that
reported none is taken to use the defaults, which this checks); otherwise the
decision is `gate_not_reproduced`.

#### Refused, with why
- `placement.reconsider_every` changes when waiting requests are scored, not how a
  scoring decides.
- `[estimates]` change which history is trusted. Records hold what history said,
  already resolved into `learned`; re-deriving it would need the records as they
  stood at each decision. That is possible within the retention window, and is
  left for a later item.

#### Effects, honestly
- `completion_difference_seconds` compares the replayed winner with the recorded
  winner, both under the changed parameters: the effect of the choice under one
  model. When a change is to a prior, both completions move, and the difference is
  still only the choice's.
- `actual` is what the recorded run took from the scoring, beside the estimates,
  so an agent can see when an estimate it is tuning against was far off.
- Queue effects of flipped decisions are not simulated: a flipped decision would
  have changed what later decisions saw. Every output says so in `limits`.

### Automated evidence

`mise run check` passes. Unit tests (`scheduling/replay/tests.rs`) build records
the way the service does (`placement::decide`, `Candidate::scored`,
`Arrival::of`, read back from JSON) in each of the three record generations, over
five situations: a warm faster member, a saturated caller, both saturated
(`fleet_busy`), memory pressure, and one with running and queued work, learned
history in sub-millisecond durations, an unmeasured member and an unavailable copy.

- Unchanged parameters, and values equal to the recorded ones (including
  `[admission]` values, which derive every gate again), reproduce every decision
  with zero flips, in every generation; a decision recorded with a 0.2 margin
  replays under its own margin.
- Known changes flip known decisions: a 0.2 margin keeps a 53 s against 60 s win on
  the caller (+7 s, also from S4 estimates); a 10 s warm sync prior does too, and is
  `recorded_before_inputs` for S4 records; a 0.99 saturation threshold opens a 95%
  caller (240 s against 300 s, −60 s) and places a `fleet_busy` request;
  `memory_pressure = "critical"` opens the faster member (−7 s).
- A request that waited replays its arrival scoring (where it was sent), or, in a
  tuning-item record, its last scoring.
- A `fleet_busy` request whose scoring had a winner counts as `had_a_winner`,
  never as placed by a change.
- A tampered estimate is `not_reproduced`, an inconsistent gate
  `gate_not_reproduced`, an S4 record that started elsewhere
  `started_elsewhere_before_inputs`, and recorded parameters outside today's ranges
  `invalid_parameters`, each by run ID and never replayed; pinned, undecided and
  candidate-less records are left out; at most 1000 flips are listed.
- `Records::placeable_since` reads only placeable requests, the latest within its
  bound, oldest first.

`tests/placement.rs`,
`replay_reproduces_real_decisions_and_flips_them_under_changed_parameters`: real
services in temporary homes make three placement decisions (a warm twice-as-fast
member wins 33 s against 60 s; a saturated caller sends work away; leases on both
members make a request `fleet_busy`). Replay reproduces all three from their arrival
scorings, also with every gate derived again; `keep_local_margin=0.5` flips exactly
the first (+27 s); `unknown_remaining=1s` gives the `fleet_busy` request a place;
`--since` bounds the period; `estimates.window` is refused with exit 2.

### Installed-system evidence

On 2026-11-03 at 13:06 UTC the release build of this item's code (commit `5fbc850`,
before rebasing onto elsecall#25, which changes the run and lease entry path, not
scheduling or records; sha256
`4e6300fc71518e58…`) was installed on the Mac mini (`mac-mini`,
master), with no `config.toml` changes. Placement decisions are made and recorded
on the master, so the devbox (`macbook-pro`) kept the tuning item's build; quotes
are unchanged. The mini was idle when installed.

**The real history reproduces.** `elsecall replay` read the master's 23 placeable
records (08:51 to 12:19 UTC): 22 S4 decisions from their recorded estimates, and the
tuning item's evidence run from its last scoring, which was its arrival scoring.
All 23 reproduced, with zero flips: 13 kept on the mini, 10 sent to the devbox.

**A decision records its arrival inputs.** A light placeable run from a scratch
repository (`/bin/sh -c echo …`, label `replay evidence`, `r-8e1cc0c1acd0`) went
to the devbox: outside work held the mini at 100% CPU. Its record's `arrival` held
`size` 1.0, `hold_left_ms` 599971, and each candidate's `state` and `inputs`, with
the mini's saturated gate and both members' `admission` thresholds. Replay then
counted 24 decisions, 2 replayed from `arrival`.

**A changed keep-local margin**, `elsecall replay --set keep_local_margin=0.5`
(13:07 UTC), flipped 2 of 24. It kept 2 more on the caller (15 against 13) at
+8.8 s of expected completion in total (+5.0 s and +3.8 s):

| Run | Label | Recorded | Replayed | Difference | Actual on the devbox |
| --- | --- | --- | --- | --- | --- |
| `r-d98d6ec2790c` | synthetic burn | devbox, 11.6 s | mini, 16.7 s | +5.0 s | 8.9 s |
| `r-6f17a41b32a1` | synthetic hash | devbox, 6.2 s | mini, 9.9 s | +3.8 s | 9.1 s |

One flip, as `replay` prints it:

```json
{"run_id": "r-d98d6ec2790c", "label": "synthetic burn", "verdict": "started",
 "ran_on": "macbook-pro", "replayed_from": "recorded_estimates",
 "recorded": {"winner": "macbook-pro", "completion_seconds": 11.639975505,
   "reason": "macbook-pro completes earliest, in 12 s, against 17 s on the calling member mac-mini."},
 "replayed": {"winner": "mac-mini", "completion_seconds": 16.666685614,
   "recorded_winner_completion_seconds": 11.639975505,
   "reason": "The calling member mac-mini completes in 17 s; macbook-pro would take 12 s, not earlier by the 8.3 s margin for leaving the caller."},
 "completion_difference_seconds": 5.027,
 "actual": {"member": "macbook-pro", "started_after_seconds": 0.0, "run_seconds": 8.894, "completion_seconds": 8.894}}
```

The other way, `--set keep_local_margin_floor=1s` flipped 2 decisions the 2 s floor
had kept on the mini: `r-99daf9903eca` (8.0 s against 6.6 s on the devbox) and
`r-1404c6ec53dd` (7.6 s against 6.2 s), −1.5 s each by estimate. Both ran on the
mini in 6.0 s and 4.3 s, faster than either estimate. That is the kind of gap a
report on estimate accuracy should weigh before narrowing the margin.

**Other changes.** `--set sync_prior_cold=30s` replayed only the 2 decisions with
inputs, with no flip. The other 22 came back as `recorded_before_inputs`.
`--since 2026-11-03T12:00:00Z --set admission.saturated_utilization=1` derived every
gate in both recent decisions again from the recorded load, with none
`gate_not_reproduced`. Neither flipped: the mini's outside load was 100%, which
saturates even at 1.

### Limits

- Replay reads this member's records only; decisions made while another member was
  master are in that member's.
- At most the latest 5000 placeable requests per replay (`limited`).
- S4 records replay only margin changes; tuning-item records replay the last
  scoring, which for a request that waited is not where it was first sent.

## `elsecall report`

The second step of the tuning loop: from the CLI alone, in JSON, how the fleet did
over a period and how accurate the estimates were. README "Fleet reports" defines
every metric and finding. Replay, the third step, is the section above.

### Decisions

**Each member reads its own history; the reporting member computes.** A report is
made on any member through a port, `MemberHistory` (`reporting/gathering.rs`), with
two adapters: `own_history.rs` reads this member's records and load log, and
`peer_history.rs` asks another member over two new member routes,
`POST /peer/v1/report/facts` (pages of 250 records) and
`POST /peer/v1/report/load` (aligned buckets), whose messages are
`report_protocol.rs`. All members are asked at once. A member asked about a
period ending after its own now answers up to its now, so a clock a little
ahead does not make its history unreadable.
The routes sit behind the member API's authentication; any enrolled member may
ask, as it may ask for status, since a report diagnoses the operator's own fleet.

**Facts, not records, cross the channel.** A record becomes a `Fact`
(`reporting/facts.rs`): label, placement, urgency, size, members, times, outcome,
the copy's warmth, and what was predicted on the member it ran on. No paths,
processes, warnings or arguments. Each request is counted once, by the member it
was made on: the member side of a placed run is dropped, and a run ID seen twice
is kept once, this member's own first. Records written under a machine's host name before it joined the
fleet (both members have some from S1 and S2) are counted under its fleet name.

**Answers are untrusted.** Another member's facts must have a run ID, a failure
code of the form Elsecall writes (lowercase words and `_`), bounded printable
names and labels, a fleet member as where they ran (else they are counted as
`dropped`), fall in the period and come at most 250 a page; buckets are cut to the
period, at most 1,001, with shares clamped to 0..1, Elsecall's cores and the
capacity to the machine's weighted cores, seconds to the bucket and speeds to the
plausible range. Everything that can grow with labels or codes is bounded:
at most 200 label groups per estimate and 20 failure codes, so a unit test's
report of 100,000 requests, each with its own longest label and code, stays
within the CLI's 8 MiB response. A member that cannot
answer, within 10 s a call and 30 s for all its pages, is named in this member's
words, never its own, and the report says it is `incomplete`.

**What is compared with what.**
- *Run*: placement's run estimate for the member that ran it, including the cold
  factor and slowdown, against the command's own time. Other runs: the label
  median behind the hold limit (`estimator: label_median`).
- *Wait*: the arrival estimate the tuning item records, since the recorded
  decision is the last scoring, just before the start, with a wait of about zero.
  Waits and syncs are grouped per member as well as per label, since they depend
  on the member and workspace more than on the label.
- *Speed*: drift from the load history's `speed_min`/`speed_max`, and
  `speed_conversion`: whether two members' run times of one label differ as
  their measured speeds say. Placement converts history between members by speed,
  so this is the error it makes.
- Deferrable waits are reported apart: housekeeping waits for idle capacity by
  design and would dominate interactive percentiles. For the same reason the
  load history now keeps `interactive_queued_max` beside `queued_max` (a JSON
  field with a default, so older minutes still read), and idle-while-queued
  looks only at interactive requests.

**Balance is fleet-wide.** Members' fine buckets line up (aligned to the epoch), so
balance compares them bucket by bucket. `--member` and `--label` narrow requests
and the member list, never balance, and never the `incomplete` findings: a
member outside the filter still feeds what is shown. A member's work `share` is of the whole
fleet's work, so it does not become 1 under `--member`.

**Bounded answers.** At most 1,000 fine buckets per member over any period, and a
unit test serializes the largest page of facts (200 four-byte characters per
label) and of buckets below the 1 MiB member response limit.

**Versioned definitions.** `definitions: 1`; changing a definition or threshold
changes it.

### Automated evidence

`mise run check` passes. Unit tests compute every metric on fixed records and load
buckets (`reporting/fixtures.rs`):
- `facts`: what was predicted where the run ran, the sync taken out of the run,
  the member side dropped, old host names attributed, implausible facts refused;
- `requests`: counts by kind and end, `fleet_busy` and its denominator, the
  placement mix, interactive and deferrable waits, work and shares;
- `load`: totals weighted by sampled seconds, `cpu_p95`, work while saturated,
  speed drift, series points and balance with idle-while-queued;
- `accuracy`: run, sync and wait agreement per member and per label, the label
  median estimator, ratios without zero predictions, speed conversion;
- `findings`: every kind, severities, order and the cut at 10, and none for a
  quiet fleet or small groups;
- `document` and `gathering`: the period and both filters, balance staying
  fleet-wide, each request counted once across members, facts naming a machine
  outside the fleet dropped, `incomplete` whatever `--member` says and when the
  fleet cannot be listed, an unknown `--member`; `window`, `peer_history`
  (believed buckets, a member's error not quoted), `report_cli` (`--since`,
  `--bucket`), records read page by page, the largest member answers fitting a
  member response, and the largest report fitting the CLI's;
- the load history keeping interactive requests queued apart, and older minutes
  counting every queued request.

`tests/report.rs`, `report_covers_every_members_requests_load_and_estimates`:
two real services in temporary homes. A required run, three placeable runs that
go to the warm, twice-as-fast member on the 60 s prior, that member's own pinned
run, a lease and a `fleet_busy` on the master. The master's report counts them,
both members' load and speed, the run accuracy of the prior (3 samples, ratio
under 0.2, bias below −25 s), warm sync and arrival-wait accuracy, and a
`run_estimate` finding. The filters work as documented; the other member's report
reads the master's requests over the member channel; invalid `--since`,
`--member` and `--label` exit 2 with `invalid_request`; with the other member
stopped, the report names it and leads with `incomplete`.

### Installed-system evidence

On 2026-11-03 the release build of this item (commit `94924d8`, sha256
`bad0058dad0fa800…`) was installed on both members, the devbox first, at about
14:09 UTC, with no `config.toml` changes. `doctor` was healthy on both and the
firewall rule held (`member_listener_firewall: allowed`). Records on both
members go back to S1 (2026-11-02); load history starts with the tuning item's
install at 12:17 UTC, so it covers about two hours.

**The whole history, from the master.** `elsecall report --since 30d` answered in
under a second (17.9 kB). Abridged, rounded:

```text
period: 2026-10-04T14:10:18Z to 2026-11-03T14:10:18Z, fine 2,640 s, series 30 h
fleet.requests: 363 (195 runs, 168 leases, 10 nested), failed {member_unavailable: 8,
  fleet_busy: 3, cancelled: 2, connection_lost: 2, request_too_large: 1, workspace_sync_failed: 1}
fleet.fleet_busy: 3 of 326 (0.9%)
fleet.placement: pinned 231, required 98, placeable started 24 (13 on the caller,
  11 elsewhere, all on macbook-pro; 8 on the run prior)
fleet.waits.interactive: 314, p50 0.02 s, p90 18.5 s, max 114 s; deferrable: 25, p90 18.1 s
fleet.balance: 4 periods, cpu_spread_mean 0.41, imbalanced_share 0, fastest mac-mini,
  idle while queued 0 s
```

| Member | Requests read | CPU mean / p95 | Outside | Elsecall share | Saturated | Memory pressure | Speed (drift) | Work share | Interactive wait p90 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `mac-mini` (master) | 296 | 0.78 / 0.95 | 0.77 | 0.9% | 38% | 14 s, `warning`; thermal `heavy` | 0.72 to 1.02 (29%) | 79% | 21.7 s |
| `macbook-pro` | 67 | 0.42 / 0.48 | 0.42 | 0.5% | 1.6% | 0 s | 0.70 to 0.78 (11%) | 21% | 3.9 s |

The devbox's records are its own S1 runs, under its old host name `MacBookPro`
(45), and its own since (22); the 100 member sides of placed runs were left to
the master, which counted them as `placeable` and `required` runs on the devbox.

Accuracy, `by_member`:

| Estimate | Member | Estimator | Samples (priors) | Median predicted → actual | Ratio | Bias | MAE |
| --- | --- | --- | --- | --- | --- | --- | --- |
| sync, warm | `macbook-pro` | placement | 9 (2) | 0.29 → 0.29 s | 1.01 | −0.6 s | 0.7 s |
| sync, cold | `macbook-pro` | placement | 2 (2) | 120 → 0.6 s | 0.01 | −119 s | 119 s |
| run | `mac-mini` | placement | 13 (3) | 8.0 → 7.2 s | 0.75 | −15.9 s | 17.2 s |
| run | `macbook-pro` | placement | 11 (5) | 11.5 → 5.9 s | 0.55 | −64.8 s | 65.4 s |
| run | `mac-mini` | label_median | 43 | 0.16 → 0.15 s | 1.00 | −2.9 s | 9.3 s |
| run | `macbook-pro` | label_median | 13 | 4.1 → 2.2 s | 0.97 | −0.1 s | 0.5 s |
| wait | `macbook-pro` | placement | 2 | 0 → 0.05 s | — | 0.2 s | 0.2 s |

`by_label` had 25 run groups (for example `acme build Uploads` on the
mini: 8 samples, 20.9 s predicted, 20.5 s taken). Waits have two samples: arrival
estimates are recorded only since the tuning item.

`findings`, in order:

```text
high   run_estimate      Label "acme test Formatting" run estimates on mac-mini are 6.6× too high:
                         median actual 12.5 s against 82.2 s predicted, over 3 samples.
high   master_saturated  The master, mac-mini, was saturated by outside load 38% of the time.
high   master_memory_pressure  The master, mac-mini, was under memory pressure for 14 s, so it admitted nothing new.
medium run_estimate      Label "synthetic burn" run estimates on mac-mini are 1.9× too high: median actual
                         6.0 s against 11.6 s predicted, over 8 samples, 3 of them priors.
low    speed_drift       mac-mini's measured speed ranged 0.72 to 1.02 (29% drift); estimates converted by speed move with it.
low    speed_conversion  Label "elsecall housekeeping" runs 0.59× as long on macbook-pro as on mac-mini,
                         but their measured speeds say 1.29×.
low    speed_conversion  (the same for "synthetic hash" 0.81×, "acme build Uploads" 0.99×, "synthetic burn" 1.04×)
```

**Over time.** `elsecall report --since 2h --bucket 15m` compared 113 one-minute
periods: `cpu_spread_mean` 0.37, `imbalanced_share` 2.7%, idle while queued 0 s.
Its series, CPU mean / seconds saturated per 15 minutes:

| From (UTC) | 12:15 | 12:30 | 12:45 | 13:00 | 13:15 | 13:30 | 13:45 | 14:00 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `mac-mini` | 0.90 / 409 | 0.85 / 455 | 0.68 / 277 | 0.88 / 475 | 0.83 / 418 | 0.75 / 261 | 0.72 / 147 | 0.64 / 82 |
| `macbook-pro` | 0.50 / 25 | 0.36 / 6 | 0.51 / 8 | 0.50 / 37 | 0.43 / 26 | 0.45 / 7 | 0.35 / 0 | 0.22 / 0 |

The mini's measured speed fell from 1.02 to 0.72 around 13:00 and was back near
1.0 by 13:45.

**Filters and the other member.** `--member macbook-pro --label "synthetic *"`
kept 9 runs, all placed there from the master (3 on the prior), and only the
devbox in `members`, with its work share of the whole fleet (39% of these
labels' core-seconds). `elsecall report --since 30d` on the devbox read the
master's 296 requests over the member channel and matched the master's report:
363 requests, the same placement mix and the same findings.

**What the report showed.**
- **The master is the busy one.** Outside load, not Elsecall's work (under 1% of
  its cores), kept it saturated 38% of the time and briefly under memory
  pressure, while the devbox averaged 42% CPU. Placement already avoids the
  saturated mini, but it is also the calling member for nearly all work, so
  its interactive waits are over 5× the devbox's at p90.
- **Cold sync priors are far off.** The two first syncs took 0.6 s against the
  120 s prior; two samples are below the findings' minimum of three, but the
  group shows it.
- **Measured speed misleads conversion.** The benchmark puts the devbox at 0.78
  of the mini at best, yet four labels ran 0.59 to 1.04 times as long there; the
  placement slice saw the same with fixed work (0.88). The mini's own measurement
  drifted 29% within the two hours.
- **The label median holds for pinned work** (ratio 1.00 and 0.97 per member);
  placement's run estimates on priors are the ones far off.

**With replay.** After rebasing onto replay (elsecall#26), build `9500272`
(sha256 `17f5ec33bea85a5c…`) was installed on both members at 14:18 UTC. The
30-day report then counted 366 requests (297 and 69 read), with the same
`fleet_busy`, placement mix and findings, and `elsecall replay --since 1d` on the
same build replayed all 24 placement decisions with no flips.

### Limits

- A report reads at most 50,000 requests per member; more marks it `truncated`.
- Each member's requests count where they were made. If the calling member of a
  placed run cannot answer, that run is missing, though the member that ran it
  has a record of it.
- Accuracy compares predictions with what happened where the request ran. What a
  different decision would have done is replay's question.
- Balance and idle-while-queued see a member's own queue. Placeable requests that
  wait on the master for another member, and pinned interactive work that could
  not move, count alike.
- A member may report another member's run IDs ahead of it in the roster; the
  first copy counts. Members are the operator's own enrolled machines, so this
  was not guarded further.
- Load history starts with the tuning item; before it, members have no load and
  balance is empty.
- Members on earlier versions answer neither report route and are reported
  `incomplete`.

### Contract questions

1. **Who may read a member's history.** Any enrolled member, as for status. Should
   reports be limited to the master?
2. **Thresholds.** The findings' thresholds (README "Fleet reports") are chosen,
   not specified. Should they become settings, as placement's are?
3. **Waits that ended in `fleet_busy`** are not in the wait distributions, since
   those requests never started; they are in `fleet_busy`. Should they count as
   waits of their hold limit?
