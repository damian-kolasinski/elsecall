# Scheduling tuning harness (H1–H4)

This is the shared contract for the workload exporter (H1), fleet simulator (H2),
settings search (H3) and compressed fleet validation (H4). H1 ships the export and
empirical inputs, not a simulator or tuner. Existing `replay` still re-scores isolated
recorded decisions; it does not simulate their effects on subsequent queues.

## Operator decisions

1. Minimise interactive time to result, preserving each member's reserve (especially
   the master), keeping `fleet_busy` at or below 1%, and limiting contention slowdown
   to about 25%. Prefer the strongest eligible machine.
2. Build the simulator first. Synthetic execution on real machines validates the top
   candidates after simulation; it is not the inner search loop.
3. Tune settings first: label sizes, reserves, keep-local margins, run/sync priors,
   cold factor, admission gates and hold limits, then estimate trust settings. Other
   placement policies are a later experiment, with their own fidelity baseline.
4. Proposals only. The operator applies a proposal using the fleet's synced service
   configuration. Export, simulation and search never apply settings. A/B rehearsal
   alone temporarily leases scheduling settings through fleet sync and restores
   them; it never writes a proposal into the production source or consumer files.

## Objective, constraints and search ordering

A trial is a replay of one fixed trace, initial state and seed. Its population is
all non-nested interactive root requests arriving in the evaluation window, both
runs and leases; deferrable work and carry-in still consume capacity. Nested work
shares its outer grant and is never counted as independent demand. Record counts
for runs and leases separately alongside the combined objective. A lease's run
interval is the period its holder keeps capacity, not process-tree CPU.

For an admitted request, **time to result** is terminal time minus arrival:
queue wait + setup/sync + execution + result return. For a lease it ends at release
or holder exit. Count failed command exits too: removing slow failures must not
improve a candidate's score. Cancelled callers and infrastructure failures are
reported separately and fixed across trials; a trial that invents or loses one is
invalid. H2 may model negligible result return as zero, but must disclose it and
pass the recorded-day check. Existing `waited_ms` ends at admission; remote
`run_ms` in a record includes sync, whereas trace `run_ms` excludes it. Therefore
trace queue wait + `sync_ms` + `run_ms` is the comparable observed duration.

The primary objective is per-request scheduling overhead:

```
H_i = (T_i - I_i) / I_i
mean = sum(H_i) / N
p90 = sorted(H)[ceil(0.90 * N) - 1]
```

`T_i` includes queue wait, setup/sync, execution (including contention) and the
recorded terminal/result time. The simulator explicitly models result return as
zero. Rehearsals use the caller record's terminal timestamp; extra result delivery
after that timestamp is not separately instrumented. Nonzero command exits stay
in the population. A scheduling refusal has no completed-work result: it remains
in the population with unknown overhead and makes the objective unscorable.
Missing/zero ideals, missing terminal records, interrupted/partial rehearsal
populations and changed fixed external endings cannot produce an accepted score.
Descriptive known-subset distributions remain separate from scorable mean/p90.

**Ideal, frozen before evaluating settings:** the run time alone on the fastest
free eligible member, without wait or sync. “Free” removes transient occupancy,
queue/hold effects and sync delay; it does not remove placement requirements,
workspace hosting availability or baseline capacity requirements. Pinned and
required requests keep their constraints; a nonmaster caller cannot freely place.
Explicitly unreachable members and unavailable workspaces are excluded from
simulator ideals using source-time evidence. Rehearsals use the current inspected
fleet capacity/availability and preserve the source structural placement constraints.

- In the simulator, use the input's uncontended execution reference, adjusted by
  member speed, physical CPU limits and the baseline cold/warm run model, and take the smallest estimate
  over the frozen eligible members. Missing speed/reference evidence yields an
  unknown ideal; observational model uncertainty remains explicit.
- In rehearsals, freeze synthetic work units from the reference execution
  member's effective demand × compressed uncontended duration × calibrated work
  rate. Each eligible member's aggregate rate is its calibrated single-worker rate
  × logical demand, capped at its logical core count. Ideal is work / the fastest
  eligible aggregate rate. Fractional demand is paced, multicore work is split
  across children. Work units convert to a fixed process-CPU budget on each member;
  they are not an instruction-count requirement. Calibrations and aggregate
  runtimes are retained in the trace.
  Zero-CPU inputs are timed holders; their intended duration is retained. Mixed
  zero/positive per-member demand is rejected because it cannot conserve CPU work.

`outcomes[].ideal_ms` records simulator denominators. Rehearsal mappings include
`ideal_ms` and `fixed_work`; the private trace stores `rehearsal.calibration`,
`work_iterations`, `uncontended_ms` and `ideal_ms`, keyed by synthetic run ID.
Simulation and tuning result versions are now 2 because `metrics.score` and the
default ranking change units/meaning; the trace stays version 1 with additive
controlled-work fields. Selected traces also carry optional per-request
`execution_reference` inputs (known duration/identification/source speed, or
explicit unknown), frozen before cropping load and competitor history. Window
validation and final planning therefore conserve the same source work, including
requests that finish after the arrival-window end. These are model references,
not measurements of the live ideal. Fleet-history pages cannot supply these
locally derived fields; imported trace references are bounded at the simulator
input boundary. The new objective is `metrics.scheduling_objective`, with `overhead.mean/p90`,
per-request ratios, hard limits, and `stopping_target_met`. The shorter
`metrics.objective.overhead` aliases its distribution. `metrics.score` and `tune`
rank by mean overhead, subject to constraints. All ratios are fractions: 0.05
means five percentage points of overhead, not 5% relative improvement.

The old `objective.mean_charged_seconds`, admitted-time distributions and
same-member `slowdown` remain diagnostics and fidelity inputs. A `fleet_busy`
charge retains the original hold floor plus execution reference; it is never
substituted for overhead. Existing fidelity tolerances are unchanged.

Hard limits are constraints, not weighted penalties:

1. **Reserve violation seconds = zero.** Integrate admitted declared Elsecall
   grants beyond usable capacity (weighted capacity minus the approved reserve
   floor, or a higher candidate reserve), with half-open grant intervals and exact
   settings boundaries. Count a violating second per member, not once per request.
   `scheduling_objective.admission_reserve` reports violating member-seconds,
   excess core-seconds and unknown member-seconds. The simulator uses its admission
   ledger. Rehearsals reconstruct grants from arrival + admission wait through
   terminal time, including sync, with recorded settings. Missing intervals/settings
   or forced incomplete overlap yield unknown coverage, never a verified zero.
   `attributable_reserve` is a separate load measure: effective work + inferred
   outside load beyond the approved floor in simulation, excluding
   outside-only exogenous excess. Live aggregate host CPU cannot attribute outside
   processes or reconstruct instantaneous per-process CPU; those values stay null
   with unknown coverage. This uncertainty does not erase a known admission result.
   The legacy `reserve` fields retain modeled excess/exogenous diagnostics.
2. **`fleet_busy` <= 1%.** Exact refusal numerator divided by terminal interactive
   root scheduling verdicts, with numerator, denominator and ratio published.
3. **p90 contention slowdown <= 25%.** For started root runs, actual command
   execution duration / the same fastest-eligible ideal; the ratio must be <=1.25.
   Publish the distribution, `p90_excess` and unknown runs. Exclude sync and leftover
   setup wait. Include deferrable and overlapping carry-in runs in the limit.
   Unknown ideals/physics cannot certify the limit; known modeled violations still
   fail. Point, lower and upper simulator envelopes remain visible to search.
4. **No more failed/timed-out runs than baseline.** Count nonzero exits, signals,
   scheduling and infrastructure failures conservatively across root run arrivals,
   including deferrable work. Leases remain separate even though rehearsal uses a
   run grant to synthesize a holder. Missing outcomes make failure evidence unknown.
   Simulator candidates rank and gate against unchanged-settings point trials; lower/upper comparisons report risk only;
   measured comparisons use A's measured failure count. Deadline exhaustion by a
   fixed-work worker exits 124 and counts as failure; no work is silently truncated.

Every limit has a true/false/null result under `hard_limits`. Unknown evidence
never becomes a pass. Infeasible results have score `"infinity"`; invalid or
unscorable results have null scores. Search uses only the point estimate, ordering feasibility first, then mean
request overhead. Lower and upper envelopes are reported beside every candidate
and never block readiness, ranking or a simulated win. Strongest-eligible placement, fewer effective changes and lexical
settings are tie breaks, never a band that can override lower overhead. The
violation vector includes admission excess, p90 slowdown excess and extra failures;
legacy diagnostic vector fields remain for compatibility.

A measured candidate is accepted only when mean overhead improves by at least
`max(0.05, 2 * aa_mean_noise)`, p90 overhead does not increase, and every hard limit
holds. A/A mean and p90 noise are the respective absolute differences between two
identical-settings arms. Without a supplied matching A/A artifact there is no
acceptance verdict. A single A/A pair is a noise floor, not a confidence interval.
Stop a hill climb when mean <=0.15 **and** p90 <=0.50, two consecutive iterations
have no accepted candidate, or the operator's deadline arrives. The harness
reports the target test; the operator's iteration controller owns streak/deadline
state. `tune` proposes candidates and never runs or applies an unattended climb.

## Export and versioned trace

```
elsecall trace export --since 7d --output workload.json
# Select a rehearsal workload automatically; save the JSON selection explanation:
elsecall trace export --since 7d --rehearsal-window busiest:2h --output rehearsal.json > selection.json
# Override the default minimum of 40 interactive root arrivals:
elsecall trace export --since 7d --rehearsal-window busiest:30m --rehearsal-min-interactive 60 --output rehearsal-60.json > selection-60.json
elsecall trace export --since 2026-11-03T00:00:00Z --member devbox --output devbox.json
# Read this machine's retained history without using or installing a new service:
elsecall trace export --since 7d --local-history --member <this-machine-name> --output local.json
```

`--since` defaults to 7d, accepts the same duration/RFC 3339 grammar and 30-day
retention bound as `report`. `--until` is an optional RFC 3339 arrival cutoff,
no later than now and strictly after `--since`. The arrival window is half-open;
load and settings evidence continues through the terminal results of its arrivals.
It does not admit requests arriving during that drain tail. Missing history stays
unknown; selecting a rehearsal workload rejects gaps with member, interval and
affected run IDs. The default output is `elsecall-trace.json`, a new 0600
JSON file. An existing path, including a symlink, is refused. The CLI's normal JSON
response describes the file, byte count, models and completeness; the file itself
is the trace, without a CLI response envelope. `--local-history` requires the
member name to attribute pre-enrolment records; it opens existing SQLite databases
read-only, does not initialise/migrate/prune them, and excludes the unfinished
load minute. It is explicitly local and incomplete for fleet fidelity.

Normal export queries all enrolled members over the same authenticated member
channel used by reports. Any enrolled member may read fleet history; local routes
retain same-user socket authorization. Requests are read on a separate SQLite
connection in pages of 10 raw records. Load is read in aligned 12-hour pages to
preserve minute resolution even across 30 days. A missing/older member is named
with an error code; no fallback to empty successful history. Keep partial exports
for diagnosis, but do not trust them for tuning. Paginated reads have a fixed end
time, not a transaction spanning all members: work still running at export may
complete during the read. A fidelity day must end before export and contain no
unresolved requests. The current fleet roster/master is not a historical roster.

The v1 file contains:

| Field | Contract |
| --- | --- |
| `trace_version` | Integer 1; independent of the CLI protocol and report definitions versions. Readers reject unknown major versions. Optional additive fields can be ignored; changed units, semantics or required fields require a new version. |
| `since`, `until` | UTC RFC 3339 millisecond timestamps, half-open arrival window. Durations ending in `_ms` are milliseconds; CPU milliseconds are core-milliseconds of the measured process tree. Sizes and effective demand are weighted-core equivalents. |
| `incomplete` | Collection failed for a member/roster, or this is a deliberately local-only or member-filtered snapshot. Missing model evidence is separately reported by model status and limits. |
| `master`, `members` | Observed master, member names, errors and one-minute load rows. Each row contains weighted cores, usable capacity, in-use grants, queues, speed min/max, CPU mean/max, outside-load proxy, memory/thermal pressure and seconds observed. Historical reserve is weighted cores minus usable capacity. No row means unknown, not idle. |
| `requests` | One caller-side record per run ID, including retained carry-in. Arrival, calling member, label/family, kind, size, size basis in `resources`, original placement/required member, urgency/deadline/action, resolved hold limit, execution member, warmth, sync time, queue wait, command run time, outcome, nesting and capacity-sharing IDs. `resources` preserves command start/end and process-tree `cpu_ms`. |
| `requests[].decision` | Recorded parameters, arrival/final scoring inputs, eligible candidates, grants ahead/holding, queue counts, capacity, gates, speed and estimates. Free-form reason/why text is blanked. The actual placement and scored winner remain distinct. |
| `leftovers` | Hosting member's run ID, start, cap, duration, CPU, and ending, or null duration/ending while running. Includes retained commands ended before the window whose leftovers overlap it. PIDs and process groups are excluded. |
| `demand` | Per member and family (label fallback where no family), declared median, process-tree CPU median, effective estimate and sensitivity band, fit/idle counts, background baseline, residual error and explicit identification status. |
| `contention` | Per member/family matched observations in competing-declared-core bins, duration ratios, sample counts/ranges, unmatched count and identification status. |
| `limits` | Data provenance and missing evidence affecting replay or interpretation. A consumer must check these plus member errors. |

Optional/null fields retain missing historical evidence. A local copy has no
remote warmth or sync; null is not fabricated cold/warm telemetry. Family is the
matched consumer rule pattern, just as estimates learn it today; with no rule,
use the exact label, and never guess a consumer from a prefix. Hosted duplicate
records do not create new arrivals; the host contributes its leftovers. Repeated
lease acquires already share one record. Unknown carry-in before retention is
unrecoverable. Export includes records containing overlapping leftovers even if
their original request predates the window. Arrival-window metrics must exclude
those earlier arrivals.

No command arguments, environment values, secret values, raw workspace paths,
process identifiers, warning text, cancellation text, sync file/ref names or
consumer telemetry payloads are exported. Labels/family names may identify consumer
modules, as they already do in records. Trace files should stay private. Peers'
projections are validated before modelling; unknown versions of an API fail
explicitly. The raw trace stays outside the repository; only aggregate evidence
belongs in the review guide.

### Required offline dry run before live preflight

Run `elsecall tune preflight` **before any live preflight or unattended hill climb**.
It reads an existing fleet export, or collects one itself using the read-only trace
routes when `--trace` is omitted. It never installs, dispatches synthetic work,
reads current trust, changes settings or writes a trace/proposal. File mode is
entirely offline, with disposable simulator state removed after each trial.

```sh
elsecall tune preflight --trace fleet.json --rehearsal-window busiest:50m
# Or collect current full-fleet evidence read-only:
elsecall tune preflight --since 7d --rehearsal-window busiest:2h --compress 12
# Exact arrival bounds, fitted against the export's full retained history:
elsecall tune preflight --trace fleet.json \
  --since 2026-11-04T20:26:03.260Z --until 2026-11-04T22:44:03.260Z \
  --rehearsal-window explicit --compress 12
elsecall trace export --since 2026-11-04T20:26:03.260Z \
  --until 2026-11-04T22:44:03.260Z --rehearsal-window explicit \
  --output workload.json
elsecall rehearse --trace fleet.json \
  --since 2026-11-04T20:26:03.260Z --until 2026-11-04T22:44:03.260Z
```

File/rehearsal bounds must be RFC 3339, nonempty and inside the source arrival
window; export/collection `--since` also accepts a duration back from now. Explicit
selection uses `--rehearsal-window explicit`; `busiest:DURATION` searches within
the specified bounds. Rehearsal still accepts older source workloads without
historical load, because it records its own live observations; the dry run requires
that evidence to verify offline simulation first.

The report collects independent blockers across requests and requirements,
including terminal/reference/demand/hold inputs on the same record, carry-in,
nested coverage, leftovers, population, fleet collection and drained load. Each
candidate window carries all applicable `blockers`, in addition to its decisive
ranking `reason`. On selection failure it also diagnoses the densest rejected
window without claiming it is usable. It runs the actual tuner and checks every
sampled candidate (baseline included) at the point estimate, with lower/upper
envelopes alongside as risk information. It checks mean and p90 overhead, reserve violation seconds, busy rate, p90 contention ratio, failure
count and all four hard-limit results. Unknown point evidence blocks even when a known
violation already makes a hard limit false. Defined false limits remain visible
modeled violations; this command checks offline readiness, not live acceptance.

`--sample` defaults to 3 (2–16); `--seed` defaults to 42. `--min-interactive`
defaults to 40 and is never lowered automatically. Exit 0 requires a valid workload,
fully defined point objective/limit evidence, ranked candidates and a planned compressed
span within `--limit` (default 15m). Blockers produce the complete JSON report and
exit 1. Invalid arguments or malformed inputs still fail at the input boundary.
The default finite search sample is small; passing it does not guarantee every
later candidate is viable. The report estimates a full `--iteration-budget`
(default 64) search plus four compressed arms (A/A and A/B), preparation,
restoration and safety allowances. It labels this estimate explicitly; real
calibration, sync, safety and cleanup timing must be measured by live preflight.

### Placement at replay time

`rehearse`, `simulate`, `tune` and `tune preflight` accept
`--placement historical|current`. **Historical is the default** and preserves
recorded pinned, required-member and placeable placement for fidelity against
history. **Current is recommended for tuning workloads**: supply
`--placement current --rules PATH` with the consumer's current `.elsecall.toml`.
Elsecall reads and validates that file, at most 64 KiB, and never modifies it.
It uses the first matching label rule's placement for each source request; no
matching placement means the consumer default, pinned. Leases and deferrable work
remain pinned under the admission contract. Internal and already-synthetic records
retain their placement. A supplied rules file with historical mode, or current
mode without a file, is an error.

This mode changes only placement. Measured work, size, urgency, hold limits and
historical label families stay frozen; applying a current size rule would change
the workload being optimized. Hosting still requires a workspace and a master
caller. The rehearsal window selector supplies scratch identities for legacy
consumer workspaces and still requires identified demand on every newly eligible
member. Eligibility does not force a remote assignment: quotes, cold-sync priors,
reserves and keep-local margins still choose execution members normally.

The response records mode, rules digest and the placement split; offline preflight
also reports its selected `placement_split` and each candidate's point/lower/upper
`execution_members`. Counterfactual traces retain typed `placement_replay`
provenance. Current placement cannot certify historical fidelity, including the
tuner's internal fidelity check. A subsequent completed live rehearsal records
actual synthetic placements and may certify those observations normally.
Use the same mode and rules file throughout preflight, certification, search and
A/A or A/B arms, including held-out tuning input.

For example, read acme's reviewed rules without changing its checkout:

```sh
git -C /Users/me/Developer/acme-ios show mergegate/main:.elsecall.toml > /tmp/acme-rules.toml
elsecall tune preflight --trace workload.json --rehearsal-window explicit \
  --placement current --rules /tmp/acme-rules.toml --seed 42
elsecall rehearse --trace workload.json --placement current \
  --rules /tmp/acme-rules.toml --compress 6 --seed 42
elsecall tune --trace workload.json --placement current \
  --rules /tmp/acme-rules.toml --budget 64 --seed 42
```

### Arrival density and busy-hour calibration

`rehearse`, `simulate`, `tune` and `tune preflight` accept `--load-factor F`,
default 1, finite range **1–32**. A root arriving `t` milliseconds after the
selected window starts now arrives at `round(t/F)`. Command work, reference
execution duration, demand, size, placement, holds and urgency remain fixed.
Deferrable deadline slack moves with its arrival. Nested requests move with their
root, preserving the tree's internal timing, and remain in the simulation's drain.
Historical background observations and settings keep their clock; source physics
is frozen before moving requests, and outside CPU is inferred from the original
history. Millisecond rounding preserves every root in the half-open cohort.
`--fidelity` requires F=1 because changed arrivals cannot certify historical fidelity.

For rehearsal, apply compression **after** density: arrival offset is
`round(round(t/F)/compress)` and command duration is still divided by compression
alone. Density increases overlap, rather than shrinking the work. A/A noise and
rehearsal provenance record F; noise from a different F cannot accept an A/B pair.
Iteration timing includes the greater of planned endpoints and the baseline's
simulated point drain divided by compression, plus preparation/restoration and
safety allowances. Fixed live timers and sync can still make arms longer.

Preflight selects F only from complete **observed rehearsals** of the selected
workload. It no longer chooses F from a simulated pressure match. On an idle
fleet, run a small set of installed-build arms with identical workload, build,
settings, compression and seed, changing only F. Retain the private trace from
each arm. Never overlap production or install an experimental build to calibrate.

```sh
elsecall rehearse --trace workload.json --load-factor 1 --compress 6 \
  --seed 42 --limit 15m --output observed-f1.json
elsecall rehearse --trace workload.json --load-factor 1.25 --compress 6 \
  --seed 42 --limit 15m --output observed-f125.json
elsecall tune preflight --trace fleet.json \
  --since 2026-11-03T19:50:32.424Z --until 2026-11-03T20:40:32.424Z \
  --rehearsal-window explicit --calibrate-busy-hours \
  --busy-since 2026-11-04T07:00:00Z --busy-until 2026-11-04T13:00:00Z \
  --busy-rehearsal observed-f1.json --busy-rehearsal observed-f125.json
```

The workload passed to `rehearse` must be either the exact selected preflight
export, or the same preflight source with the selected `--since`/`--until` arrival
bounds. Preflight accepts the exact serialized digest of either representation:
window selection adds diagnostic limits and trims inactive settings/leftovers,
whereas explicit rehearsal bounds retain them. No recording is rewritten and
unrelated workloads remain invalid. Compression, seed and current-rule provenance
must match. Busy bounds must be
minute-aligned and inside the full source. The master needs at least 90% load
coverage, CPU readings and positive capacity. The report includes the measured
reference and each trial. The matching share and queue belong to the **master**,
where the busy-hour evidence was collected. Admitted share is exposure-weighted
`in_use / contemporaneous capacity`; CPU remains diagnostic. Each shape also
reports `member_admitted_capacity_share` keyed by member, including the devbox.
These peer shares are diagnostic, not additional matching gates; absent or less
than 90% observed coverage is null, never zero. Rehearsal master queue
peaks are reconstructed from **caller arrival to admission** events in
**source-minute bins**, including requests that the master dispatches to peers, and
waits multiply by compression, so changing compression does not silently change
the comparison's units. Pressure includes the observed drain. Reference wait p90
uses started interactive roots **across the fleet**, regardless of execution
member. Both reference and rehearsal use each caller's `waited_ms`, measured from
that request's original arrival to its grant. Post-grant workspace sync and command
setup do not extend the queue interval. Pre-admission work remains in the caller's
measured wait; no subtraction or guess from command-start time is needed.
Reference refusals/cancellations without measured admission waits are excluded,
rather than invented as zero.

Choose the smallest tested observed F with master admitted share within **0.10**,
master queue p50/p90 and maximum within **one request**, and fleet interactive
wait p90 within **max(10 seconds, 25%)** of the reference. If none fits every band, select the observed arm with the
smallest **normalised distance** and label `selection: nearest_match` (`matched: false`). For each of
the five metrics, let `[L,U]` be reference ± its tolerance above. Its signed
`deviation` is `observed - clamp(observed,L,U)`; its `normalised_distance` is
`abs(deviation)/(U-L)`. The selection's `distance` is the arithmetic mean of those
five distances, with equal weights. Bands use their full width, without clipping
to physical bounds. Values inside a band contribute zero. This measures proximity
to the accepted region, rather than favouring its centre or metrics with large
units. Exact distance ties choose the smallest F; all-band matches have distance
zero and keep `selection: matched`. The report publishes the chosen F, aggregate
distance and every metric's observed value, bounds, signed deviation and normalised
distance, also for each trial. CPU and peer shares remain diagnostics.

For the run-7 reference (share 0.8515282825458954, queue 1/4/7, wait p90
268952ms), the five observed arms F=1.25/1.75/1.9/2/2.25 have no all-band match.
The rule selects F=2 with distance ≈0.200326: queue p50 is two requests above
its upper bound (normalised distance 1), and admitted share is ≈0.000326 above
its upper bound (normalised distance ≈0.001629); the other three metrics
contribute zero. A nearest selection permits preflight to continue using that
observed F, while exposing the misses; it does not certify fidelity or waive
other readiness checks.

There is no interpolation, assumption of monotonicity, or simulated fallback.
Missing observations block readiness and leave `chosen_load_factor` null. Factors from different builds,
settings, compression, masters or workloads cannot be pooled. Workload identity
includes current-placement provenance and the rules digest: use the same
`--placement current --rules PATH` for preflight and every observed arm when
tuning under current rules. Historical placement remains supported with its
original workload identity. Placeable and required-member requests, including
remote sync, are accepted with complete observed admission waits and terminal
execution intervals. Interrupted arms, missing planned requests and incomplete
recordings remain invalid, even when their observed subset matches the target.

The matching bands and smallest-tested-F rule for all-band matches are unchanged.
Broadening waits to the fleet and attributing master queue entries by caller instead of host
prevents offloading from hiding queue pressure. The reference queue is the
master's sampled **local admission** `queued_max`; its telemetry does not include
remote placing phases. The reconstructed rehearsal queue deliberately counts all
pending master callers, including remote placement and pre-admission work, against
that historical pressure budget. The report exposes this distinction in
`queue_basis` alongside the raw statistics. This is a conservative comparison:
a mixed historical reference with remote placing phases can reject even identical
replay behavior. It does not claim exact queue fidelity for that population.
Using admission instead of command
start removes post-grant sync from that pressure; it is equivalent for the prior
local pinned cohort without setup work. Peer load remains visible without imposing
a new matching requirement on busy-hour evidence collected on the master.
Each rehearsal also needs at least 90% clipped load coverage, independently of its completeness flag. Recalibrate after
installing a changed worker build; an observed match is not a simulator fidelity
certificate.

### Automatic rehearsal window selection

`--rehearsal-window busiest:DURATION` selects a half-open arrival window within
the collected `--since` history. The duration must be from 1ms to 30d and fit
inside that history. It requires full fleet collection and a known master;
`--member` and `--local-history` cannot be combined with selection.

Rank by the number of real, non-nested interactive root arrivals, including
leases, nonzero command exits and scheduling refusals. Break ties by earliest
start. Nested and other competing consumer arrivals stay in the workload;
they do not increase its score. Known internal service work (`elsecall housekeeping`
and `elsecall fidelity drift`, pinned or deferrable) is excluded from the synthetic
replay population, including carry-in checks and demand requirements. Its recorded
CPU remains in the load history and becomes inferred background/outside load;
it is never assigned invented effective demand. Consumer labels are matched
exactly, with the placement/urgency check, rather than by an `elsecall` prefix.
Full-history exports and fidelity-against-history keep the original requests and
strict evidence rules. Rehearsal/synthetic arrivals are rejected.

A valid window has at least **40 interactive root arrivals** by default.
`--rehearsal-min-interactive N` changes this positive minimum and requires
`--rehearsal-window`. Sparse windows are rejected with their actual and required
populations; the selector never silently lowers the minimum.

A valid start has no retained earlier request still queued, admitted, syncing or
running on any member. An end exactly at the start is idle; a missing terminal
time is unknown and blocks later starts. Retained leftover work overlapping the
window is rejected because rehearsal cannot synthesize it. Never remove carry-in
to make a window look idle. Work before retention remains unknowable.

This selects a **synthetic rehearsal workload**, not a fidelity-against-history
window. Historical workspace and settings are optional: the rehearsal uses
current settings and records its own inputs and outcomes. Selection requires load
with CPU and speed evidence on every member from the start through the historical
drain, including buckets beyond the arrival cutoff. Every available later load
bucket is retained for slower simulated sensitivity trials; genuinely missing
simulated drain evidence blocks the offline dry run.
Existing workspace identities are preserved. Requests without one get deterministic
SHA-256 scratch workspace/repository identities grouped by recorded label family
(exact label fallback) and calling member. Repeated family/caller work reuses a
scratch tree and can create cold then warm remote copies; different families or
callers remain separate. These identities are marked `synthetic_workspace: true`;
they do not backfill historical workspace evidence.

The leading candidates also pass the actual rehearsal planner's input checks:
identified effective demand for every eligible execution member, reference
duration, resolved hold limits, supported terminal outcomes and nested-root
coverage. Missing demand reports its identification status. Model fits use the
full collected history, including idle minutes outside the chosen window; they
are preserved when slicing the workload. Earlier completed requests remain as
estimator/reference history, while load and settings retain the available drain
evidence after the selected arrival window. Selection checks input feasibility at the maximum supported compression;
`rehearse` still checks the requested `--compress` and execution limit.

The response's `data.rehearsal_window` gives the source interval, chosen bounds,
interactive count, ranking rule and candidate explanations. Candidates represent
the earliest start of every interval where score or eligibility can change;
later starts within it cannot improve the ranking. Rejected candidates name a
decisive reason, including run IDs or member/load gaps where applicable.
`lower_rank` candidates were not planner-checked after a better candidate passed.
The trace's `limits` retains selection provenance; keep the response for the full
candidate audit. The report includes `minimum_interactive`. A failure returns
`no_rehearsal_window` with the same structured report in `error.details`, and
creates no workload file. Missing settings/load stay missing; effective demand,
duration, terminal outcomes and other execution inputs still pass the real planner.

For **fidelity against real history**, export ordinary history without
`--rehearsal-window`, then use `simulate --fidelity` (or `--day YYYY-MM-DD`).
This route still requires the recorded workspace identities, settings at the day
start and complete load coverage. Synthetic scratch identities explicitly block
historical fidelity. Rehearsal certification instead compares the simulator with
the completed rehearsal's own fully recorded trace; selecting a workload never
certifies it or changes the fidelity tolerances.

## Effective demand and uncertainty

Process-tree CPU undercounts work delegated to external services: an IDE build
service or simulator can use cores outside a test's process tree. It is reported
beside, never substituted for, effective demand. CPU utilisation times weighted
cores is a weighted-core equivalent, not hardware-counter attribution across core
types. It is suitable for scheduling comparisons with an explicit error band.

There is a particularly important trap: `outside_mean` is
`mean(max(0, machine_cpu_share - declared_grants/weighted_cores))`. Subtracting it
from machine CPU and regressing the result largely rediscovers declared sizes.
It is **not** an independent outside-load measurement. Preserve it for admission
replay, but fit demand from total CPU instead:

```
CPU cores in minute = background baseline + sum(active family count * family demand) + residual
```

Counts are fractional interval overlap, with sync removed and nested work excluded.
Fit separately per member, with an intercept anchored by at least three idle
minutes, using ordinary least squares and pivoted matrix inversion. Reject minutes
with less than 90% sample coverage, memory pressure, >=98% CPU (censored demand),
unfinished work, unknown labels, or reconstructed grants differing from observed
mean grants by more than 0.25 cores. The baseline assumes unrelated CPU is locally
stable; correlated background work is a confounder. No CPU fit can prove a
simulator belongs to a label from this telemetry alone.

Fit at most the 32 families with most observed exposure (at least one active
minute of exposure and squared fractional counts summing to 0.05). Exclude unsupported
columns, and exclude rows containing work omitted from the fit. Leave unsupported
families explicit and unknown. Singular/confounded designs, negative demand or an
implausibly negative baseline return unidentified demand, never a declared-size
fallback disguised as a measurement. At least three residual degrees of freedom
are required. The sensitivity band is point estimate ± (4.5 residual standard
errors + idle p90-minus-p10 CPU variation, with a 0.1-core floor), clipped at zero.
This is **not a calibrated 95% confidence interval**: adjacent minutes and changes
in outside load are correlated. Always publish residual RMSE, run counts, coverage
and the baseline assumption. H2 must vary this band and background load; H4 must
validate effective demand before accepting proposals.

`report` exposes the same model as `demand` (the 200 largest groups by run count;
`demand_omitted` counts the rest), fitting with all available context
before filtering the displayed families and members. An `effective_demand` finding
requires at least five distinct runs contributing to fitted minutes, an absolute gap >=0.5 cores, and the entire
band outside ±25% of the declared-size median. It asks the operator to investigate;
it is not permission to resize work. Learned/default sizes do not count as declared
sizes. Sparse data and process-tree CPU alone produce no disagreement finding.

## Contention inputs and optional consumer telemetry

For each completed root run >=1s with >=90% minute-load coverage, compute the mean
sum of other root grants' declared cores over its execution interval. Compare its
run time with the median of at least three low-concurrency (<0.5 other cores) runs
of the **same exact label, member and warmth**. Pool these ratios by family into
bins [0,.5), [.5,2), [2,4), [4,infinity). Each point contains counts, median and
observed min/max, not a confidence interval. Two bins with at least three runs
are needed for `observational_curve`; otherwise the status says evidence is
insufficient. Unmatched runs remain counted. Do not mistake label/input mix,
external-service work or outside CPU for causal contention. In particular, CPU
utilisation alone does not establish a slowdown curve.

H1 does not ingest consumer telemetry and contains no consumer-specific readers.
If richer telemetry is used in H2/H4, convert it outside Elsecall into this generic
import contract (a later importer must validate it; v1 export does not accept it):

```json
{"telemetry_version":1,"source":"consumer-measurements",
 "members":[{"member":"mini","at":"2026-11-03T10:00:00.000Z",
              "duration_ms":60000,"machine_cpu_cores":7.2,
              "independent_outside_cpu_cores":1.1}],
 "runs":[{"run_id":"r-000000000001","member":"mini",
          "work_class":"warm-small-fixture","uncontended_run_ms":12000,
          "concurrent_effective_cores":4.2,"run_ms":14800}]}
```

Units and intervals must be explicit, samples nonoverlapping per member, numbers
finite/nonnegative and member/run IDs match the trace. `source` is provenance,
never an executable plugin or filesystem path. Work class must be consumer-supplied
and stable across comparable input sizes. Imports must have no arguments, env,
secrets, paths or module-specific fields; reject unknown fields and retain whether
outside CPU was independently measured. Do not infer an independent baseline from
a grant-derived proxy. Consumer data never replaces the original trace evidence.

## H2 simulator and fidelity gate

Use a virtual clock and deterministic events (timestamp, event class, member,
run ID tie-break), executing Elsecall's real admission, placement and estimate
functions. Inject clock, machine observations, member transport and execution at
system ports; never duplicate scheduling formulas to make a trace fit. Reconstruct
history as it existed at each decision, including runs before the evaluation
window, estimator windows, warm/cold state, learned speed ratios, carry-in,
leftovers, local and master queues, required members, nesting, deadlines, hold
expiry, periodic reconsideration, member availability, capacity/reserve, outside
load/pressure, sync occupancy and cold builds. Execution duration must respond to
simulated contention and speed, rather than staying at its already-contended
recorded duration after a placement flip. Preserve external outside load as a
fixed exogenous timeline; change leftover load when work moves/ends.

The simulator is **trusted for tuning** after the fixed fidelity gate passes on a
complete, fully recorded rehearsal. It must finish within its bound, submit every
planned request, collect every terminal record and member's settings/load, have no
production overlap or interruption, and complete cleanup. Incomplete or stopped
rehearsals never issue certificates. Do not tune against certification residuals.
The former complete recorded UTC day plus held-out day is a stronger, optional
validation route; it is no longer the first-iteration gate. Fixed comparisons:

- reproduce >=99% of execution-member placements exactly (100% for pinned and
  required requests); reproduce every refusal code and terminal scheduling count;
- wait errors: median absolute <=1s, p95 <=max(2s, 5% of the recorded p95 wait),
  maximum <=max(5s, 10% of the corresponding request's recorded wait);
- per-member queue-length maxima per recorded minute: **every observed minute
  within ±1 request**, peak within one request, zero invented queues in
  continuously idle minutes. Compare interactive queues separately when available;
- charged mean time to result within 5%, and observed run-duration median/p95 within
  10% per member/family with >=5 runs. Report every smaller group without claiming
  it passed a statistical gate.

Queue fidelity uses a deterministic sampling tolerance, not a confidence claim:
minute maxima can differ by one when an arrival/admission crosses a sampling
boundary. Requiring 95% exact equality made a six-minute arm effectively require
six exact matches, even though every error was already bounded by one request.
Instead require a nonempty sample with `within_tolerance_count` equal to
`minimum_matching_count` (the full recorded-minute count), no missing simulated
minutes, maximum error ≤1, peak difference ≤1 and no invented idle queues.
This applies separately to total and available interactive queues at every sample
length. `exact_count` remains diagnostic. Run 8's A/A second arm has 4/6 exact
minutes but 6/6 within tolerance, maximum error 1 and peaks 6/6, so it passes.
Run 5's 16 versus 4 peaks and maximum minute error 13 still fail. No fraction of
large errors is excused, including on long samples; a fully observed idle minute
still permits no invented queue even of length one.

Publish numerator/denominator, outliers by run ID and timelines; no rounding an
outlier away. For waits, measure arrival to admission and end-to-end sync separately.
Minute sampling permits queue tolerance, not missing placements. Freeze tolerances
before selecting the workload. For the optional recorded-day route, a separate
held-out day checks overfitting. A/B rehearsals validate selected proposals.

H1's historical records may be insufficient: they lack workspace identities for
local work, full settings-change history, all intermediate reconsiderations,
second-by-second outside CPU, or older load. The gate must return **blocked by
missing evidence**, with exact missing fields, rather than using current settings
as if they were historical. The optional recorded-day check may need an additive recording change and new
complete inputs before it can pass. The trace remains useful for model development
and single-decision replay meanwhile. A trace version is not a fidelity certificate.

## H3 proposals, H4 validation and result reporting

Search only validated settings and declared candidate ranges. Persist a result
artifact with trace digest/version, code/build identity, settings and model versions,
seed, baseline, fidelity evidence, exact changed settings per member/label, full
metrics and violation vectors, sensitivity results, uncertainty/coverage, and a
short explanation for each change. Show before/after distributions as well as
means, busiest/strongest-member usage, memory/thermal gates and leftovers. List
infeasible and unscorable candidates explicitly. Never claim the charged objective
is the user's measured successful latency. Proposals include a reviewable config
diff and rollback, with no apply side effect.

The H1–H4 operator loop is:

1. H1 exports a representative trace; H4 `rehearse` certifies H2 in minutes.
2. H3 `tune` searches the simulator with a current rehearsal certificate. Missing
   demand or constraint evidence remains explicit; simulator trust does not turn
   uncertain feasibility into a proven tuning win.
3. H4 `rehearse --compare PROPOSAL` runs the same frozen trace and seed under the
   current settings and proposal, reports all observed metrics side by side, and
   restores settings. New refusals or altered external-failure populations cannot
   manufacture an improvement.
4. The operator chooses and applies winning service settings through fleet sync,
   and label sizes/hold options through the owning consumers' existing rules.
5. Each member automatically checks newly completed recorded UTC days as its own
   pinned deferrable housekeeping-style work. `report.tuning_evidence` retains the
   results. Missing inputs yield `not_checkable`, never passes. A failed fidelity
   day produces a high-severity `simulator_drift` finding and revokes trust across
   the fleet until a subsequent rehearsal passes. A passing day cannot clear it.
   Monitoring attempts run at least hourly (or at a shorter housekeeping interval).
   Each admitted pass checks at most one day, newest first. Recent uncheckable
   days retry at most daily for two days; older incomplete evidence stays reported.

No installation or real-fleet run is part of implementing this contract. Live
rehearsals belong to the separately authorized release verification session.

## H1 validation

Fixed fixtures recover known demands despite misleading process-tree CPU and
declared grants, reject confounded/unsupported histories, avoid nested double
counting, recover known contention growth, and check projection privacy and cursor
carry-in. Real-service tests create records on isolated members then export over
the authenticated member channel. The review guide records `mise run check`, the
read-only seven-day master snapshot, demand/curve summaries, byte count and known
coverage limits. Installed-system verification is left to the release verification
session; no installation or production configuration changes are part of H1.

## H2 implementation

`elsecall simulate --trace FILE [--set key=value ...] [--seed N] [--day YYYY-MM-DD]`
is offline. A private temporary fleet runs the production `Scheduler`, including
its queues, offers, admission pump, hold/deadline expiry, periodic placement,
estimator windows, learned sizes and speed conversions, sync history and restart
recovery. `Clock` and `RunIds` are explicit scheduling ports. Controlled driving
uses the same observation and reconsideration functions as the background service;
virtual machine/process/member adapters replace only system boundaries. SQLite
records in the temporary fleet are real records used by real learning code.

Events use millisecond time and stable member/run-ID order. Due service timers are
drained before time advances; settings restarts, external endings, execution
completions, observations and arrivals then run in stable order. Offer retries and
subsecond holds are explicit next-event boundaries. Causal history includes all
trace arrivals before an evaluation day. Already admitted carry-in is restored
through the same admission adoption operations as recovery, preserving capacity,
remote hosting, nested families and elapsed time rather than admitting it again.
An omitted active capacity ancestor is an explicit initial-state error. Earlier
requests retained solely for overlapping leftovers are not new arrivals.

The half-open `since..until` interval selects arrivals, not an execution cutoff.
After `until`, the simulator admits no further arrivals and drains all selected
interactive, deferrable and nested work to its terminal result, including queued
work that starts later. Objectives, failure counts and fidelity use the same
arrival population and retain its recorded terminal results after `until`.
Reserve accounting follows admitted work through completion; queue/CPU timelines
remain clipped to the arrival window. Missing post-window load remains explicit
uncertainty. Missing execution references stay unresolved and unscorable, never
successful zero-duration work; the existing maximum drain guard still applies.

The virtual executor removes point-model contention from each recorded command
interval to freeze its input work, integrating across competing grants, minute
background and leftover transitions. Selected-member speed and copy warmth then
convert that work; progress responds to current simulated contention and effective
demand. Command exits teach subsequent decisions through the normal run channel;
external cancellations/infrastructure endings remain fixed and never become
successful training samples. Sync occupies a grant and copy, and the end-to-end
hold can expire during sync. Leases hold capacity to release/holder exit; nested
requests reuse their actual outer grant. Leftovers move with their command, consume
CPU after its grant ends, and retain the copy until they end. Configured reserve
and capacity changes replay through real shutdown/recovery, including its treatment
of queued requests and lost remote connections.

The background timeline is fixed across candidates: subtract fitted observed
request demand and observed leftover exposure from total machine CPU when
identified; otherwise use the exported grant-derived proxy, removing recorded
leftovers before adding simulated ones. This remains inferred outside demand,
not independent attribution. Lower/point/upper trials vary effective-demand bands,
background by residual RMSE and contention by observed ratio ranges. Unknown or
extrapolated bins and unidentified demand remain explicit. Physical fallback priors
and input work are frozen before candidate overrides; changing an estimate prior
cannot make actual work or sync cheaper. Unknown reference work is unscorable,
with its run ID, rather than silently omitted from the objective. Result-return
latency is currently zero.

The JSON response exposes the charged mean, actual admitted mean/median/p90/p95/
maximum and run/lease counts; exact refusal populations; fixed external endings;
reserve violating member-seconds/core-seconds and maximum excess; declared-capacity
excess and exogenous overload; slowdown distributions and uncertain run IDs;
strongest-eligible assignment counts with frozen evidence and coverage; per-member
CPU/grant/leftover exposure and queue/gate timelines; every terminal or unresolved
request; and lower/upper sensitivity metrics. JSON represents an infeasible score
as the string `"infinity"`. Invalid, unscorable and validation-needed trials stay
separate. Inferred outside CPU cannot certify an independently measured reserve
constraint, even when its point violation vector is zero.

Candidate keys include all validated production placement/admission/estimate
parameters plus `member.NAME.reserved_cores=CORES`, `label.PATTERN.size=CORES`
(first matching supplied pattern wins) and interactive `hold_limit=DURATION`.
Reserve candidates require historical settings and cannot reduce their approved
floor. Repeated keys and malformed traces are rejected. The seed is retained in
output; the current envelopes have no random draws. Byte-identical deterministic
output includes neither wall-clock elapsed timing nor temporary paths. Input
artifacts are bounded to 64 MiB and validated before any model or scheduler runs.

`--fidelity` rejects overrides and uses a rehearsal's own window when its
provenance is present; otherwise it selects the busiest finished UTC day by root
interactive arrivals; `--day` selects an explicit recorded day. It computes the
unchanged placement, refusal/count, wait, queue, charged-mean and grouped-duration
gates above. Smaller duration groups remain descriptive. CPU utilisation is
reported with matched exposure; no numerical tolerance was specified in H1, so
its status says `unspecified_tolerance`. A missing outcome cannot shrink the
recorded denominator. Exact blockers and outliers are published even when other
metrics can be computed. The simulator never relaxes a tolerance or substitutes
current installed configuration for missing historical inputs.

### Additive recording inputs

Fleet collection already uses authenticated member history routes for every
enrolled member, preserving partial exports with member errors. H2 adds:

- `members[].scheduler_settings`: timestamped full scheduling parameters and
  capacity/reserve snapshots at service start, retaining the latest baseline before
  the export window and subsequent changes. Parameters are immutable between
  service starts, so these are exact historical configuration events.
- `requests[].workspace`: opaque SHA-256 workspace/repository identities, including
  local pinned runs. Paths stay out of traces. `workspace_recorded` distinguishes
  known outside-tree work from legacy missing identity; old records default false.
- `requests[].pre_run_attempts` and `pre_run_attempts_complete`: sanitized pre-run
  configuration-sync attempts, with member, optional start time, duration and cause
  code. New records retain successful busy retries as well as failures, bounded at
  4,096 attempts with explicit incompleteness/truncation. Legacy final/fallback
  summaries remain partial, and hold-cutoff durations remain right-censored. No
  synced paths, generations or values enter this projection. Trace pages consume
  at most ten raw records and use a smaller prefix when history approaches the
  signed-channel response budget; cursors still name the last consumed record.

The virtual configuration-sync adapter replays these attempts, including delay,
failure, fallback and hold expiry through the real service. Missing attempts for
a newly selected member, partial legacy histories, extra retries and censored
durations are exact blockers, not fabricated successful history. Initial pending
requests retain resolved sizes, source metadata, estimates and original absolute
holds/deadlines; only new arrivals resolve candidate defaults again.

These fields are optional v1 additions. Existing history is not backfilled from
current settings. Pre-window estimator/copy snapshots, historical fleet role/
roster changes and sub-minute load/intermediate decisions are still incompletely
recorded. A day without an estimator-recency window of causal context or an initial
history snapshot reports that missing evidence. Minute pressure and speed extrema
are approximations; fidelity is the test of whether those approximations suffice.

### H2 automated and recorded-data evidence

Unit tests cover virtual time, exact two-member metrics, deterministic output,
subsecond holds and recorded refusal-charge floors, demand/curve envelopes,
piecewise work normalization, leftover attribution, fixed failure populations,
reserve and candidate validation, strongest eligibility and every fidelity boundary.
Real services in temporary homes record pinned, required, cold/warm placeable and
refused work; the simulator reproduces their decisions after both services stop,
including the learned fourth placement returning to the caller. Carry-in and
settings-restart integration tests exercise real admission and durable recovery.
The review guide records the full check and release timing.

The read-only attempt on 2026-11-04 found both production members reachable on
build `8483879b404da223`, which predates the trace routes. Normal fleet export and
signed peer trace reads returned `not_found`; neither machine was installed,
restarted or reconfigured. Existing authenticated report reads supplied 979 fleet
facts (820 master, 159 devbox), but their old projection lacks simulator inputs.
The busiest finished UTC day, November 3, has 504 fleet arrivals / 413 root
interactive requests. Only 701 master and 703 devbox load minutes exist, beginning
12:18 / 12:17 UTC, so there is no complete recorded day.

The untouched local master projection additionally contains five 61-sample minute
rows, rejected by the strict trace boundary. A separately labelled incomplete
diagnostic copy applies the existing peer adapter's 60-second exposure cap,
preserves the raw artifact, and lists each altered row in its limits. It cannot
supply a fidelity certificate. Historical settings/identity, early-day load and
the devbox's trace arrivals remain missing. The guide reports the diagnostic's
actual comparisons and timing. Tuning results remain untrusted; collect a new
complete fleet day after separately authorized release verification. A held-out
day is still required before H3.

The reported `run_ms` keeps H1's comparable interval (total admitted duration
minus workspace sync). Where a prior copy's leftovers delay setup, the simulator
also publishes `leftovers_wait_ms`; its execution slowdown excludes that setup.
The next run can claim a lingering copy and then wait/stop its leftovers before
sync, using the production executor's wait budget. Unknown stopping latency is
an explicit sensitivity limit. Missing load exposure is published as unknown,
with null CPU means when wholly unobserved; reserve checks continue through
post-day execution and list missing member/minutes rather than calling them idle.

The final release diagnostic for November 3, including point/lower/upper trials
and JSON output, took **5.08 seconds wall** (1.05 user, 0.66 system) on this Mac.
This measured an explicitly incomplete diagnostic, not a complete fleet day.
Fidelity reports `blocked_by_missing_evidence` with 2,465 exact blockers and
`trusted_for_tuning: false`: placements match 357/425 (pinned/required 342/398),
terminal scheduling populations are 390 recorded versus 389 simulated, and wait
errors have median 8 ms, p95 30,756 ms and maximum 370,092 ms. Fixed external
endings match 3/9, so the trials are invalid even where a comparable mean falls
within tolerance. The review guide contains all aggregate comparisons and private
artifact digests. No tolerance was changed to obtain these results.

New recordings supply settings, workspace and pre-run attempt inputs, while
authenticated fleet collection includes compatible members' arrivals. A later
authorized H4 replay can provide fully recorded short-run evidence. Certification
now uses a complete rehearsal; complete recorded and held-out days remain optional. The
legacy duplicate-sample load-counter defect remains a recording-quality check for
release verification; adding request metadata does not retroactively repair it.

## H4a controlled synthetic fleet rehearsal

```sh
elsecall rehearse --trace workload.json --compress 6 --limit 15m --seed 17
# Optional new artifact path; existing files and links are refused:
elsecall rehearse --trace workload.json --output rehearsal.json
elsecall simulate --trace rehearsal.json --fidelity --seed 17
```

This command **executes synthetic work on the enrolled fleet**; it is separate
from the offline simulator and does not install, restart or reconfigure services.
Run it on the current master with compatible builds on every member. The default
compression is **6**: an hour's arrival span becomes ten minutes, leaving five
minutes of the default 15-minute bound for preparation, queues and actual sync.
This is deliberately moderate: real sync, network, process startup and the
scheduler's second-scale reconsideration timers do not become six times faster.
Compression must be finite and in [1, 1000]; the execution bound must be positive
and at most one hour. Faster compression can distort very short work; published
arrival errors and the unchanged fidelity comparisons expose this.

Freeze and validate the entire input before creating anything. Each root arrival
is scheduled at `(requested_at - trace.since) / compress`; execution lasts
`run_ms / compress` (rounded to a millisecond, minimum one). Due arrivals launch
concurrently in batches of at most 32, with a fleet safety check between batches.
The result publishes intended and recorded arrival offsets and signed arrival
error per request; comparison against the exported arrivals alone cannot detect
source-timing distortion. Each source caller launches its normal `elsecall run`
client with the source size, urgency, pinned/placeable/required placement and
**resolved, unscaled hold limit**. Absolute deferrable deadlines are rebased to the
rehearsal epoch and compressed, preserving the deadline action. Admission,
placement, reserve, gates, pre-run configuration sync and copy locking are the
production paths, with their current settings. No scheduling policy is changed.

CPU duty comes from H1's identified effective-demand point for the source family
on each eligible execution member, rather than declared size or process-tree CPU.
Missing demand on an eligible member blocks execution; it never silently uses the
size as a measurement. The worker converts weighted-core equivalents to logical
CPU duty using that execution member's actual performance/efficiency core counts:
`logical_duty = effective_demand * logical_cpus / weighted_cores`. It uses the
system Perl and Time::HiRes, process-CPU-clock budgets, cumulative fractional
sleep pacing and forked workers for multiple cores. Every child spends
`work_iterations / member_calibration_rate / workers` CPU seconds, checking its
CPU clock after batches of at most 8192 operations, with smaller final batches. A slower core or lower CPU frequency
changes how many instructions execute, not how much CPU is demanded. This is a
CPU-service surrogate for H1's measured CPU demand, not an instruction-throughput
benchmark; no QoS override or affinity changes production scheduling. Each member
calibrates the same CPU kernel during preparation using its process CPU clock, so scheduler
preemption cannot lower the calibration rate; CPU and wall elapsed times are both
recorded. The rate sets work-unit conversion across members, rather than assuming
the idle core's instruction rate remains available under concurrency. Work does
not end when its intended wall lifetime expires. Workers stay in the command's process group, wait for children,
and fail with exit 124 on absolute expiry if work remains. Outside contention
therefore stretches completion. Regression tests preserve CPU budgets under
injected slower instruction service,
fractional/multicore pacing and explicit scheduler stalls. Simulator fidelity
tolerances remain unchanged.

To diagnose an older instruction-budget recording without installing anything:

```sh
elsecall simulate --trace observed.json --rehearsal-cpu-replay
```

This forensic replay uses measured process-tree CPU divided by child count,
bounded below by the frozen uncontended runtime, as execution cost. Recorded
waits and wall runtimes remain comparison outcomes, not scheduler execution
inputs. It reports `fidelity.status=diagnostic`, never grants tuning trust, and
cannot issue a certificate. Normal simulation and certification still use the
frozen input physics and unchanged fidelity thresholds. Rehearsal outside-CPU
inference removes measured process-tree CPU exposure in weighted units; that
residual remains inferred, not independent host attribution.

Each recorded workspace identity gets a private scratch git repository on its
original caller. Repeated requests reuse it and therefore really create cold,
then warm member copies and perform normal syncs. Different workspace identities
get different repositories. The payload contains only the synthetic worker and
an empty consumer configuration, not the original repository's bytes/history,
build caches, tools or consumer environment. Consequently real sync/startup costs
are observed for this small payload, not reproduced from the original workspace's
size. Automatically selected workloads also synthesize absent identities as
described above. For a directly supplied trace, known outside-tree requests get
a stable per-caller scratch identity; legacy missing identity requires workload
selection first.

Root leases are represented as synthetic pinned **run grants**, keeping size,
urgency and the recorded holding duration; released/holder-exited leases end the
synthetic command successfully. The source kind is retained in the mapping.
Nested work already contributes to its root's process-tree/effective demand and
is listed as `aggregated_nested`, without another independent grant. Aggregation
requires a present replayed root whose command/holding interval covers the entire
nested interval; missing ancestors or surviving nested capacity block explicitly.
Active carry-in also blocks: choose a trace beginning before that work arrived.
H4a accepts 1–10,000 root arrivals, with each compressed command at most one hour
and each worker target at most 256 logical cores, with terminal command exits or scheduling
refusals. For a refusal lacking a run duration it freezes the median known source
family duration and publishes the synthetic target; an unknown reference blocks.
Fixed cancellations, signals and infrastructure failures cannot be synthesized
faithfully and block instead of being converted to successful work. Existing
leftover processes are not synthesized. These are controlled workload inputs,
not a replacement for H2's recorded-day reconstruction.

### Namespace and production isolation

The reserved namespace is **`rehearsal:SESSION:ORIGINAL_LABEL`**, with a random
24-hex-digit session identity. Consumer labels must not use `rehearsal:`.
The prefix/session allowance leaves the original 200-character label intact.
Stored caller and host records and exported facts have `synthetic: true`.
`rehearsal.requests` in the private trace artifact, and `requests` in the command
result, map source run ID, kind, exact label and original family to the synthetic
label/run ID, target effective cores and intended/observed timing. Remove the first
two colon-delimited components to recover the original label; use the explicit
mapping for its original consumer family, not a guessed label prefix. Synthetic
families in the observed trace use their own namespaced labels; the scratch
consumer file deliberately applies no production rules.

Synthetic records are excluded **before bounded history limits** from production
run/family/size/speed learning and member-wide sync learning. They do not train
production pools, even through fleet-wide speed ratios or warm-sync fallback.
`report` definitions version 4 excludes synthetic requests, leftovers and accuracy
samples, including with `--label`; machine load still reports actual CPU use.
Demand fitting keeps synthetic work as explanatory context before hiding its
families, so its CPU is not attributed to a production label. Trace export keeps
synthetic records, allowing their own offline replay. The rehearsal never seeds production estimates. Only an explicit `--compare`
leases scheduling service settings for its second run.

### Shared-machine safety and cleanup

- The master checks **every enrolled member**, failing closed on an unavailable
  member or unsupported API. Refuse startup while any non-rehearsal run, lease,
  queue or supervised leftover is active, unless `--force` explicitly allows that
  existing work. Forced overlap is marked incomplete in the exported evidence.
- Service-owned work carries `internal: true`, established from the service's
  local process identity; labels alone cannot bypass the production guard.
  Internal work never counts as a production arrival. Legacy known internal labels
  remain recognizable when selecting historical synthetic workloads.
- Every member pauses queued internal deferrable work during preparation, before
  calibration, until its rehearsal cleanup completes. An already active pass drains
  first, with a 20-second preparation wait; otherwise preparation refuses and
  releases the pause. All members prepare before synthetic execution and all callers
  stop before cleanup. Drift and housekeeping then run normally after the pause.
  Their deadline clock pauses for exactly the overlapping rehearsal duration
  (from arrival for work requested during the pause). The extended absolute deadline
  and an extension warning remain in the record. Already-promoted deadline work
  keeps its priority. Consumer deadlines are unaffected. The member expiry watchdog
  releases pauses through cleanup when the controller disappears.
- Poll production arrivals every 100 ms plus member-call latency while waiting, and between due
  batches. The arrival history also detects production work that starts and ends
  between polls. A new production arrival stops **new submissions**, including
  with `--force`; already submitted requests finish normally, up to `--limit`.
  This is cooperative observation, not a fleet-wide lock against concurrent
  production arrivals. A concurrently dispatched batch is already submitted.
- `--limit` bounds repository preparation, submissions and synthetic execution.
  At expiry, Ctrl-C, SIGTERM or an orchestration failure, stop synthetic callers
  and their private local process groups; remote callers use normal signal/link
  cleanup. Production processes are never signalled. Cleanup has a separate
  **15-second total deadline**; history export and offline fidelity follow and can
  extend command wall time. A pending cleanup or orchestration failure makes the
  command exit unsuccessfully and is explicit in its JSON.
- Each prepared member owns an expiry watchdog, so killing/disconnecting the
  controller does not strand the workload. A private recovery journal retains
  scratch roots and caller identities across service failure. Restart cleanup
  verifies PID/start identities, including durable live worker bindings when the
  caller died, before signalling its private group. Cleanup failures retry; a new
  rehearsal on that member is blocked until its previous cleanup completes.
- Remove synthetic source trees, their caller sync caches, member copies, unused
  object stores and per-copy state. Targeted removal uses the existing catalog,
  copy/store reservations, identity checks and confined trash deletion; it never
  runs a general prune over unrelated copies. Deletion records retain the store
  identities needed to retry after the final copy has already gone. A disconnected
  member keeps its watchdog/recovery responsibility until reachable/restarted.

### Peer I/O diagnosis and bounds

Every failed outgoing HTTP attempt records member, operation category, attempt
number, failure kind, phase and duration in milliseconds, including failed attempts
that a retry recovers. Categories distinguish connect, authentication, send,
receive, timeout, malformed protocol and verified remote refusal. A timeout also
retains its active phase; an outer whole-operation timeout uses `request`.
Roster endpoints are numeric socket addresses, so this adapter does not resolve
DNS. Tailscale supplies encrypted transport; Elsecall uses signed HTTP and sealed
upgraded channels rather than TLS. No DNS/TLS phase is invented for this transport.
No request bodies, paths, credentials, signatures or remote diagnostic text enter
the attempt journal.

Connect gets **one second per endpoint**, within the caller's whole-request bound,
so a stale first address cannot consume the whole budget. Status and quote calls
have a **six-second total budget**, at most two attempts of at most three seconds,
and a 100 ms backoff. Explicit idempotent status/roster reads, quotes and read-only rehearsal
inspection, polling, load and trust queries retry transport failures/timeouts
(at most two attempts, sharing their operation bound); authentication, malformed protocol and verified remote
refusals never retry. Offers, commands, rehearsal mutations and sync commits never
retry automatically. Each attempt has a fresh nonce. Existing operation-specific
bounds remain: offers 10 s, history pages 10 s, rehearsal operations 30 s, channel
attachment 10 s, config-sync exchanges 120 s. Upgraded result streams use 10 s of
inactivity, refreshed by traffic/heartbeats, instead of a whole command deadline;
sealed configuration-sync streams retain their 120 s exchange bound. Upgrade
streams retain the upgrade protocol’s existing content/reply budgets. Their send,
receive, authentication and timeout failures are recorded too.

A failed quote leaves that member out of the current decision and records the
failure in `decision.unreachable[].peer_io`. A member that answers and declines,
for example while an upgrade drains it, is recorded with `failure_kind: remote`,
`phase: request` and its stable code in `refusal` (`member_upgrading`); the
member's message is not retained. The caller and other responding
eligible members remain candidates; the scheduler never invents capacity from a
failed quote. The existing 750 ms initial quote wait, previous-answer cache,
reconsideration and hold limits still apply. A required-member request cannot
silently fall back to another member.

`fleet` exposes each member's recent outgoing `status.peer_io`; an immediate
failure also carries `error.details.peer_io`. `explain` includes contextual
outgoing failures during the request's time interval in `peer_io` (this is not
causal attribution to that request). `report.peer_io.members` gathers outgoing
history from every selected member over authenticated read routes. Label filters
do not attribute network failures to consumer labels. History retains the latest
10,000 failed attempts for at most 30 days; each response includes at most 1,000,
with `truncated`, `coverage_since` and explicit errors for missing history. Startup
of this recording never backfills old failures. Service logs also retain sanitized
failure events under their existing rotation limits.

### Rehearsal certification and A/B validation

On normal completion, production interruption, limit/signal or an orchestration
error, the live controller collects its fixed arrival window through the normal
authenticated trace export routes, retains only its session's requests/leftovers,
writes the reserved new **0600** artifact and runs the same simulator entry point
as `elsecall simulate --fidelity --seed S`. Partial/missing records remain explicit.
The JSON includes observed request/completion/refusal counts, admitted mean time
to result, source mappings, cleanup status and the full simulation/fidelity result.
A forcibly killed controller cannot write a final artifact; member watchdogs still
clean up, and retained records can be exported afterward.

**No numeric fidelity tolerance is compressed, multiplied or relaxed.** Compare
actual compressed milliseconds, including real sync and service timers. Complete
rehearsals replace UTC-day coverage with coverage of their own bounded window.
A dedicated bounded recorder captures the member's existing one-second scheduler
observations, including the initial boundary observation; pre-rehearsal minute
samples cannot satisfy coverage. Require at least 90% sample coverage and no
observation gap over two seconds, including boundaries. Queue/CPU minute summaries
clip each real observation to at most its following second and the rehearsal
window, splitting exposure at UTC-minute boundaries. Coverage uses elapsed
observed milliseconds, so sampler jitter or duplicates cannot inflate it. They
use only these session observations. Missing or restarted recorders block trust.
Namespaced synthetic sessions start with no own learning/copy history; their
controlled worker duty and intended duration are explicit inputs, not fabricated
observational demand/contention curves. Other missing-evidence checks still apply.

A passing complete rehearsal issues a private durable certificate on every member:
rehearsal/session identity, canonical trace and source digests, compression/seed,
latest per-member scheduling settings plus complete-history digests, executable
build, bounded fidelity summary and full-evidence digest,
issue time and expiry. The private trace artifact retains full settings and fidelity
evidence; original H4a v1 artifacts remain readable as uncertified replay inputs.
It expires on **any build change or after seven days**.
Seven days bounds stale environmental assumptions while allowing many minute-scale
iterations; continuous recorded-day checks revoke it sooner on drift. `tune`
requires current fleet evidence and fails closed on unavailable members. Editing a
trace to claim rehearsal provenance cannot supply trust; a stored current
certificate is required. Recorded-day validation cannot bypass drift revocation.

Choose a representative arrival window that fits the bound (one hour becomes ten
minutes at compression six); an entire seven-day trace does not fit that bound.
Keep H1's demand-evidence requirements explicit. Broader retained traces may still
be used separately for offline search.

```sh
elsecall trace export --since 1h --output workload.json
elsecall rehearse --trace workload.json --compress 6 --limit 15m --seed 42
elsecall tune --trace workload.json --budget 64 --seed 42 > proposal.json
elsecall rehearse --trace workload.json --compare proposal.json --seed 42
```

`--compare` accepts a tune result (including its CLI envelope) or a canonical
key-to-string settings map. Parse/freeze the source and validate the entire
proposal before either run. Both runs use the same arrivals, seed and controlled
work. Synthetic sizes and interactive holds come from the proposal only in B;
consumer `.elsecall.toml` files are never edited. Refusal charging retains the
source's original hold floor and reference duration in both runs.
When the service override map is empty (including a consumer-size-only proposal),
neither application nor restoration issues a settings lease. The sync generation
and settings document remain unchanged.

The fleet must already use synced service settings. A scheduling-only, bounded
lease overlays that accepted document through the existing sealed sync channel;
the original document stays alongside the overlay on every replica. Wait for all
members' actual active scheduling snapshots and fresh load/speed observations
before B; readiness remains inside the rehearsal's preparation bound. Preserve unrelated settings
and the master's original `fleet-config.toml`. Completion, error, limit, signal and
production interruption restore the baseline and wait for activation. A killed or
disconnected controller is bounded by member workload watchdogs and lease expiry
(execution limit plus 25 seconds for settings activation); every replica resolves
the original settings on expiry, even without the master. Application/reversion
uses the existing idle service-relaunch boundary, preserving in-flight production
work. Ordinary sync scans preserve the live lease. Master moves copy only baseline
settings into the new source. Restoration failures remain explicit, with the lease
expiry and member recovery responsibility; they never report successful cleanup.

Removing a newly issued lease restores the **exact original document bytes**,
including comments, whitespace and line endings, from the validated document
prefix carried with the overlay. This applies to the sealed replica, not just the
master's source file. Partial delivery is retried; a removal retry re-delivers
the restored manifest without issuing another generation. Lease application and
removal can each advance sync generation; neither generation nor active generation
is a restoration identity.
Existing leases from older builds retain their older serialization behavior;
upgrade all members before starting a comparison under this contract.
The original bytes plus overlay must fit the existing 64 KiB document limit;
an oversized lease is refused before any settings are issued.

For each member, freeze `service.fingerprint` and `service.effective_fingerprint`
from `elsecall sync status` (`members[].sync.service`) or `elsecall fleet`
(`members[].status.config_sync.service`) before comparing. The first fingerprints
the document bytes. The second fingerprints active parsed placement, admission
and estimate parameters plus weighted cores, usable capacity and reserve. It
excludes document formatting, sync generation, start time and transient load.
Members may have different capacities or overlays: compare each with its own
baseline, not with another member. A pending document still reports the running
scheduler's effective fingerprint, not its desired settings.

`--compare` reports the per-member maps in `effective_fingerprints.before` and
`.after` and asserts equality after cleanup, including for consumer-only and A/A
runs. A mismatch or uncheckable restoration sets `settings_restored=false`, clears
acceptance, reports `rehearsal_error`, and exits unsuccessfully. Operators should
check both fingerprints, `pending=false` and `rejection=null`. After controller
loss, wait for lease expiry and the normal idle activation boundary before
checking the effective fingerprint, pending state and rejection. The expired
overlay may remain in the sealed document until the next ordinary sync scan, so
document byte identity is guaranteed on lease removal, while expiry guarantees
effective restoration. A generation increase alone is expected for a service
proposal and does not invalidate restoration.

The A/B result contains measured overhead mean/p90, admission-ledger reserve,
separate attribution coverage, busy rate, p90 slowdown and failures for both arms,
with deltas and `comparison.accepted`. Simulation reports remain separate from
measured timing. Restoration/cleanup or population failures remove the verdict.
`wins_within_noise` aliases the new acceptance result for compatibility.

```sh
elsecall rehearse --trace workload.json --aa --seed 42 \
  --output aa-trace.json --noise-output aa-noise.json > aa-result.json
elsecall rehearse --trace workload.json --compare proposal.json --seed 42 \
  --noise aa-noise.json --output ab-trace.json > ab-result.json
```

An empty canonical proposal (`{}`) is also an A/A request. `--noise` accepts the
standalone noise file or the complete A/A CLI JSON result. Artifacts must match
source digest, build, seed, compression and baseline fleet settings/capacity; invalid/mismatched inputs fail before
fleet mutation. Both B arms and later `--noise` comparisons reuse A/A's calibration
and explicitly frozen per-source ideals, so work and denominators cannot drift with the candidate. Each arm also
reports its freshly measured calibration for inspection. New noise files are
0600, refuse existing paths/symlinks, and are written only for complete scored A/A
pairs. Older fixed-lifetime recordings remain readable but cannot establish the
new objective or issue a fixed-work certificate. The operator decides whether to
apply any proposal permanently.

Automated evidence uses two real services in private temporary homes on loopback,
with fake tailnet discovery and controlled machine observations. It covers arrival
timing, pinned/required execution, real cold/warm sync, nonzero command exits,
production refusal/interruption, learning isolation, execution limits, controller
loss, service/caller recovery, full cleanup and identical offline fidelity against
the rehearsal's own export. CPU and planning tests cover fractional/multicore duty,
weighted units, deadlines, hold preservation, nesting and missing-input blockers.
No production member is installed, restarted or exercised by these tests.

## H3 implementation: offline settings proposals

```sh
elsecall tune --trace workload.json --budget 64 --seed 42
elsecall tune --trace workload.json --day 2026-11-03 \
  --held-out-day 2026-11-04 --budget 64 --seed 42 --top 5
elsecall tune --trace training.json --held-out validation.json --space space.json
```

This command runs entirely offline. It prints one normal CLI JSON document and
never writes a proposal, service setting, consumer rule or result file. Redirect
stdout explicitly to retain an artifact privately. The simulator's disposable
SQLite state stays in temporary directories and is removed after each trial.
Search contacts no installed service; the CLI reads current fleet trust after
search when a local service/evidence store exists. `--day` selects a UTC arrival window while
retaining earlier arrivals as causal context. Without it, the whole trace arrival
window is searched. Held-out evaluation windows must not overlap tuning windows;
a day without `--held-out` uses the same input trace. Held-out inputs are never
used to select candidates or refine settings.

### Search method and bounds

The method is seeded shuffled coordinate search on an explicit finite grid with
random restarts. First evaluate the unchanged recorded settings, then shuffle
all one-setting changes at that baseline. This first sweep supplies one-factor
sensitivity. Subsequent sweeps change one coordinate around the best evaluated
candidate. At a stationary sweep, draw a seeded random grid point (each coordinate
can also inherit its baseline), then refine again. Cache exact setting maps to
avoid repeated simulations. Stop at the budget, exhausted discovery, or a bounded
restart-attempt limit. This is a heuristic, not a global optimum guarantee.

The budget counts unique tuning simulations, including baseline, and defaults to
64 (range 1–1024). Fidelity checks and the two held-out simulations are additional
and excluded from the search speed measurement. `--top` defaults to 5 alternatives
(range 1–20). Every simulation includes point, lower and upper model envelopes.
Settings, proposals and metrics are deterministic for the same input, build,
budget and seed; only the explicitly separate wall-clock performance fields vary.

The default finite grid is below. Every coordinate additionally permits inheriting
all recorded baseline values, including heterogeneous settings across time/members.
The exact grid is printed in `space` before interpreting any result.

| Setting | Default candidate values |
| --- | --- |
| `label.LABEL.size` | 0.25, 0.5, 1 and 2 times that exact label's largest recorded declaration, rounded to 0.001 cores and clipped to [0.001, 1,000,000] |
| `member.NAME.reserved_cores` | Maximum recorded approved reserve, plus 0.5 and 1 core, omitting values above the smallest recorded weighted capacity |
| `placement.keep_local_margin` | 0, 0.1, 0.25 |
| `placement.keep_local_margin_floor` | 0s, 2s, 5s |
| `placement.run_prior` | 15s, 60s, 240s |
| `placement.sync_prior_warm` | 0s, 3s, 10s |
| `placement.sync_prior_cold_with_history` | 10s, 30s, 90s |
| `placement.sync_prior_cold` | 30s, 120s, 300s |
| `placement.cold_run_factor` | 1, 3, 6 |
| `admission.saturated_utilization` | 0.7, 0.8, 0.9 |
| `admission.memory_pressure` | warning, critical |
| `hold_limit` | 30s, 120s, 600s, interactive requests only |

Default label coordinates use stable lexical order, exclude literal labels
containing `*` or `?` (an explicit glob space is required), and cover at most
64 labels, further limited to keep the whole space within 128 coordinates after
allowing for all members. No reserve coordinate is created without recorded
settings: a floor cannot be invented from current installed configuration.

`--space SPEC` replaces the default grid with a JSON **file**, at most 64 KiB,
mapping canonical parameter keys to arrays of strings, for example:

```json
{"label.build *.size":["1","2","4"],
 "placement.keep_local_margin":["0","0.1","0.25"],
 "member.mini.reserved_cores":["1","1.5"],
 "hold_limit":["120s","600s"]}
```

A custom grid has 1–128 coordinates and 1–32 distinct values each. It can use all
production parameter keys, beyond the default scope, but never an alternate
placement policy. Each value must be valid as a single change to the recorded
baseline; generated combinations are validated again, including cross-field
constraints, before simulation. Rejected combinations appear separately and do
not consume simulation budget. Service parameters use the production `Overrides`
and `Parameters` validators; sizes use the consumer-rule parser; reserves and
holds use the same candidate validator as `simulate`. Overlapping label patterns
use lexical key order, also used by the proposed consumer snippets; preserve that
precedence when integrating rules.

### Selection, sensitivity, trust and proposed changes

The artifact includes trace digest/version, build and model identities, baseline,
best candidate, top alternatives, every evaluated candidate's metrics and all
three envelopes, rejected combinations, and per-parameter one-factor sensitivity.
Sensitivity publishes evaluated values and their objective deltas at the recorded
baseline; it is descriptive and is explicitly `budget_limited` when incomplete.
All distributions, refusal populations, violations, reserve coverage, uncertainty,
strongest-member evidence, member CPU/grant/queue/gate/leftover summaries and
missing reference IDs come directly from H2. Use `simulate --set` for detailed
per-request and minute timelines for a selected candidate.

The search orders certified feasible results before validation-needed exploratory
results, then infeasible results, then invalid/unscorable results. No uncertainty
is converted to feasibility. Only point hard limits determine candidate feasibility;
lower and upper envelopes always remain visible as nonblocking risk information;
infeasible candidates retain their violation vector for exploration. Within each
nonviolating tier, rank mean per-request overhead first, then strongest-eligible
fraction, fewer **effective** changed settings, and lexical setting order. Infeasible exploration uses the published violation vector
lexicographically; it cannot displace a nonviolating candidate. When all trials
are invalid/unscorable, retain the baseline and propose no change.

Held-out output compares every metric and envelope of the selected candidate
against that day's/trace's baseline. It uses the same strongest-member preference;
`wins_only_on_tuning_trace: true` flags a tuning win that fails to win on held-out
data, including held-out constraint violations. Invalid/unscorable comparisons
remain null, not fabricated wins or losses.

`tune` recomputes recorded-settings fidelity for diagnostic evidence and accepts
a current, unrevoked rehearsal certificate as simulator trust. It reads current
fleet certificate/revocation status when connected; a missing member cannot be
assumed current. A complete recorded-day check remains optional stronger evidence,
with independent held-out evaluation available to check overfitting. None can
bypass a monitored-day revocation. The output separates `trusted_for_tuning` from
proposal validation: a trusted result is `awaiting_ab_rehearsal`, never an automatic
application. Constraint uncertainty, including inferred outside CPU, remains
`validation_needed` in individual evaluations. Search and ordinary simulation
still run on incomplete/untrusted inputs and publish exact blockers.

`proposed_change` contains a scheduling-only diff for `fleet-config.toml` member
overlays, exact changed keys with recorded before/after values and timestamps,
rollback values, and suggested `[[labels]]` sizes for consumers' `.elsecall.toml`.
Consumer snippets retain the complete selected label-rule set, including
precedence guards and shadowed rules; do not remove a rule merely because its
standalone value matches a recorded size. Explicit hold overrides are also retained.
The diff is against the latest **recorded** per-member scheduling projection,
not an exact patch of an installed source file. Merge only those keys into the
fleet's synced service settings, preserving unrelated configuration. Missing
service baselines are explicit, with unresolved overrides rather than invented
old values. Label ownership is not inferred from a prefix: merge changes into the
owning consumers' existing rules, retaining other fields and precedence. Hold
limits are consumer/request options, shown separately with instructions to change
only interactive rules. Restore original consumer rules for rollback. Nothing is
applied or written automatically.

### H3 recorded-data evidence (2026-11-04)

A fresh read-only local export from this Mac contains 859 requests (454 arrivals
on November 3), no historical settings snapshots, and five load minutes with 61
samples. The strict boundary rejects it with `invalid_trace`; it was left
untouched. November 3 has only 701 local load minutes. There is still no complete
recorded fleet day on which to certify fidelity or benchmark a complete-day search.

The final release diagnostic therefore uses H2's existing, explicitly incomplete
fleet diagnostic copy described above: November 3, 701/703 member load minutes,
with the previously documented exposure caps and provenance limits. Command:

```sh
target/release/elsecall tune \
  --trace /tmp/elsecall-h2-partial-fleet-normalized.json \
  --day 2026-11-03 --budget 8 --seed 42
```

It evaluated **8 candidates in 39.03 search seconds: 12.30 candidates/minute** on
this Mac, including all three envelopes for every candidate and excluding the
additional fidelity evaluation. This measures the available incomplete recorded
day, not a complete-day capacity claim; machine load affects wall time.

All eight trials were invalid, with six fixed external-ending mismatches in the
baseline. The charged objective and score remain null, rather than ranking a
misleading descriptive mean. Fidelity is `blocked_by_missing_evidence` with 2,465
exact blockers; `trusted_for_tuning: false`. The **untrusted proposal is to retain
the baseline**, with no service or consumer changes and no accepted win. For
context only, admitted latency is mean 124.684s / median 107.2215s / p95 367.401s;
`fleet_busy` is 2/389, maximum simulated slowdown is 19.0149, observed reserve
excess is 7,182.593 core-seconds, and 88,620 member-seconds have unknown reserve
coverage. Neither the slowdown nor reserve constraints can pass. No independent
held-out recorded day was supplied for this diagnostic; synthetic held-out
regression coverage is automated. Raw inputs and JSON artifacts remain private,
outside the repository; the review guide records their digests.

Automated H3 verification covers deterministic/bounded search, production
validation and cross-field rejection, hard budgets, point feasibility with nonblocking envelopes, the 1%
strongest-member preference, effective change counts, proposal/rollback goldens
including overlapping label guards, an optimum within a declared synthetic grid,
held-out reserve regression, and an offline CLI run that leaves its input and
working directory unchanged. Full `mise run check` passed on the H2 base; final focused tests,
formatting and clippy passed after review fixes. Installed-system verification is
left to the release verification session, including collecting compatible complete
fleet days, fidelity/held-out gates and review of proposals before any application.


After integrating H4a's shared CLI and workload reader, formatting/clippy pass,
and all 10 rehearsal, 1 simulator and 7 tuner integration tests pass. The full
check stops at the new timing-sensitive
`rehearsal::worker::tests::fractional_and_multicore_worker_cpu_matches_h1_target`:
403 library tests pass and that one fails (target 0.25 cores, measured 0.07268).
The same test also fails on untouched `mergegate/main` at `9309ab3` under this
machine's load (393 library tests pass, measured 0.07502 cores for target 0.25).
This baseline rehearsal timing failure is outside H3; it does not change the
search, simulator tolerances or proposal acceptance rules.
