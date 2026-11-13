# Workload declarations and terminal observations

This optional contract describes operations inside one admitted command. It does
not change admission, size learning, hold limits or dispatch permissions. Its one
use in decisions is the run estimate of a declared sequential plan, below ("Plan
run estimates"). Legacy process-duration learning continues unchanged, including
its treatment of nonzero exits. Consumers must not interpret that legacy learning
as full-plan runtime evidence.

A human `label` remains a display string and selects existing label rules. A
producer that has a sequential plan can declare its separate stable identity:

```sh
elsecall run --label 'project tests' --workload '{"kind":"sequential","planned_units":3,"stop_policy":"stop_on_failure","estimate_key":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","variant_key":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}' -- ./wrapper
```

`--workload` accepts at most 2048 bytes of JSON. `POST /v1/admissions` accepts the
same optional `workload` object. Absence retains legacy behavior; explicit
`{"kind":"opaque"}` declares that unit counts are unavailable. Metadata is valid
only for runs, including nested runs, not leases.

Sequential fields are required, and unknown fields are rejected:

- `planned_units`: an integer from 1 through 1,000,000.
- `stop_policy`: `stop_on_failure` or `continue_all`.
- `estimate_key`: a producer-generated SHA-256 digest of the ordered plan and its
  duration-relevant modes. Different plans can share a display label and must
  receive different keys.
- `variant_key`: a SHA-256 digest identifying compatible producer semantics and
  duration-relevant modes, such as iterations, filters, cache policy and producer
  algorithm version. Equal variants can describe plans of different lengths.

Both keys are exactly 64 lowercase hexadecimal characters. The producer owns
canonicalization and hashing. The example keys are placeholders. Never put raw
arguments, environment values, credentials or secret-derived identifiers in
metadata. Elsecall neither parses project-specific labels nor receives target
lists. Existing `workspace.repository` supplies repository scope, and existing
label rules supply family boundaries; no new caller namespace/family fields are
introduced. Neither key is used for learning yet: plans are told apart only by
their planned units.

## Reporting from a wrapper

After observing its declared operations, and before exiting, a wrapper invokes:

```sh
elsecall workload observe --observation '{"status":"all_units_observed","coverage":{"attempted_units":3,"terminal_units":3,"failed_units":1,"elapsed_ms":12345}}'
```

The CLI requires `ELSECALL_ADMISSION=granted` or `nested` and uses
`ELSECALL_RUN_ID` inherited by the command. Outside that context it clearly
rejects with `invalid_request` (exit 2). Explicit off/unmanaged runs can inherit
an outer run ID; they must not report their inner plan into that outer grant.
The environment check is only a context opt-out, never authorization: the
server still requires the live PID/grant relationship. The equivalent endpoint
is `POST /v1/runs/{run_id}/observation`. It accepts one JSON observation, at most
2048 bytes. A report cannot supply a workload, run ID in the body, repository,
label, process outcome or execution/return phase timings. It cannot change the
admitted plan. A future wrapper can use the CLI directly or post through the
local Unix API from its own process tree. The CLI prints the usual JSON envelope
with `data.recorded=true` and the run ID after acknowledgement.

Statuses are:

| Status | Fields and meaning |
| --- | --- |
| `all_units_observed` | Required `coverage`; all declared operations reached an observed terminal result, including failures. |
| `prefix_observed` | Required `coverage` and `reason` (`failure` or `requested_stop`); fewer than all operations reached an observed terminal result. |
| `censored` | `coverage` is an object or `null`; `reason` is `interrupted`, `timeout`, `cancelled`, `transport_lost`, `holder_exited` or `service_restart`. |
| `unknown` | No unit coverage assertion. |

Coverage contains required nonnegative integer `attempted_units`,
`terminal_units`, `failed_units`, and optional `elapsed_ms`. It obeys
`failed <= terminal <= attempted <= planned`. All-unit coverage requires
`terminal == planned` (and therefore attempted equals planned). A failure-stopped
prefix requires `stop_on_failure` and at least one failed terminal unit. Timing
is at most 604,800,000 milliseconds (seven days). Coverage requires an admitted
sequential descriptor; opaque work can report unknown or censored without
coverage. Reasons are closed enums, never free text.

A unit means the producer's declared operation. Terminal coverage does not claim
that all tests inside an operation executed or passed. A finish-all plan with
failed operations can have all-unit coverage. A failed prefix cannot claim that
the full plan was observed. Exit code zero, exit code one, phase timing, and
`HolderExited` alone never establish all-unit coverage.

## Authorization and lifecycle

Admission fixes the descriptor. The existing command-start endpoint binds the
executing child by PID and process start time. An observation must come from
that command or a live descendant, for its nearest executing grant. A nearer
run requester blocks reporting into outer grants even while queued or waiting
for its command binding. Requesters, sibling commands, other grants, reused
PIDs and ended grants cannot write reports. Knowing another run ID is
insufficient. Existing peer authentication and master/member placement
authorization remain required; metadata grants no dispatch permission.

A very fast child may invoke observe before its executor finishes registering
the command. The CLI retries only `not_found` report responses at 20ms intervals for at most one monotonic second, including request time. It never retries admission, execution or attach. Raw API clients can retry that same report after binding. The first valid authorized report wins;
all subsequent writes, including identical reports, fail. Invalid/unauthorized
writes do not mutate the descriptor, assertion, outcome or phase data.

The assertion records what the wrapper observed. It remains immutable if the
process is later signaled or the result stream fails. Process outcome and phase
facts still independently describe that failure. With no assertion, an ordinary
exit becomes `unknown`; signals, holder loss and transport loss become censored
with no counts. Restart recovery preserves assertions and live grants; a running
record closed during recovery without an assertion becomes `service_restart`
censored. Queued work without execution has unknown coverage. Legacy records
remain absent; they are never upgraded into structured full-plan evidence.

For remote work, the authenticated placement offer carries the fixed descriptor.
The command reports to the executing member's local endpoint. The member sends
its accepted assertion in `Exited` over the existing authenticated run stream;
the original caller relays it through its own authorized terminal endpoint. No
additional attach or command execution occurs. The serving member's record can
retain an assertion that the caller never received after transport loss; the
caller then truthfully records censored/unknown coverage. Equal knowledge is not
assumed after a broken stream.

An invalid peer observation (wrong shape, oversized JSON, contradictory counts,
or any assertion without an admitted descriptor) is dropped independently of a
confirmed process exit. Structured coverage becomes unknown and a bounded generic
warning is recorded. The exit, CPU measurement and execution/result-return facts
are preserved, including a later result-return failure. Invalid metadata content
is not echoed. Direct invalid local terminal API requests are rejected before
mutation; the production remote consumer filters untrusted optional telemetry
before constructing that request.

## Storage, reporting and mixed versions

Descriptors and observations persist with records and appear in `runs`,
`explain`, report facts and workload trace requests. No raw command or environment
is added to records, logs, reports or traces. Missing optional fields deserialize
as absent. Trace/report consumers must keep process completion separate from
`plan_observation.status`; existing `completed` retains its process meaning.

New clients omit absent workload/observation fields. Old local admission servers
and old placement members reject the unknown descriptor field; clients never
strip a declared descriptor and retry as legacy work. New placement members
explicitly acknowledge contract version 1 in accepted offers; the caller refuses
structured placement without that acknowledgement. A refused offer is not
attached and its ordinary attachment window releases the member grant.

Old clients without descriptors continue on the old wire shape. New callers
accept old terminal messages with no observation as unknown. Missing telemetry
is not inferred from an exit. A newer service paired with an older executing
wrapper/CLI can therefore have a declared plan and unknown coverage. Producers
must check the observe acknowledgement before treating metadata as recorded.
The contract does not promise exactly-once report delivery across a lost local
acknowledgement: a retry after acceptance is rejected as already recorded.

## Plan run estimates

A family's runs are mostly of other plan sizes: on 2026-11-06 to 2026-11-09,
Acme batches of 15 or more modules were estimated from single-module runs at
a median of about 210 s and took a median of about 3,300 s. So a request that
declares a sequential plan of `n` units, whose label has too few runs of its own,
is estimated from a line of run time against units, `overhead + per_unit × n`,
fitted by Theil–Sen (median slopes, then the median overhead, neither below zero)
through the latest `window` runs of its label and family whose observations give
their units. That is `attempted_units` of an `all_units_observed` or
`prefix_observed` assertion on a run that exited. Censored, unknown and
assertion-less runs say nothing about units. Runs on other members convert as
the family's do. It needs `minimum_family_runs` such runs, in at least two
sizes; otherwise the family's median stands as before. The estimate's
`run_from` is `{"from": "plan_runs", "samples", "units", "conversions"}` at level
`plan`. A running plan's expected remaining time, which the waits of work
behind it are scored with, comes from the same estimate. Hold limits are
unchanged. For a declared plan, the label's own runs count at the label levels
only when they observed a plan of the same size. Opaque work, and requests
without a descriptor, are estimated as before.

Known limits: the line extrapolates past the largest observed plan without a
bound; backtests in `elsecall report` still score declared plans at the family
level, since report facts carry no plan observations; and an older build reading
a `plan_runs` source or `plan` level (a rolled-back member's records, or an
older member's fleet report reading a newer member's facts) fails to parse it,
so every member should run the same build, as `elsecall fleet upgrade`
distributes it.

Count models keyed by `estimate_key` or `variant_key`, policy-specific occupancy
estimates and producer instrumentation are separate work.
