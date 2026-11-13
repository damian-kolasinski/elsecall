# Outside pressure: detection and the fleet and doctor surface

On 2026-11-10 a hung `fseventsd` on macbook-pro spun one core for days and grew to
a 56 GB footprint. The kernel held memory pressure at `warning` from 04:48Z to about
13:10Z, the member's gate stayed closed, and the master kept every run on the mini.
For nine hours, the only trace was `memory_pressure: "warning"` deep in
`elsecall fleet`. This slice makes a member say when something outside Elsecall
holds its gate closed, since when, and which outside processes lead. It shows this
in `elsecall fleet` and in `elsecall doctor`, whose `next_step` names the remedy.
User-facing behaviour is in the README under "Fleet", "Outside pressure".

## Decisions later slices can build on

- **Port.** `outside_pressure::sampling::ProcessSampler` returns every readable
  process as a `ProcessUse`: PID, parent, start time to the second, name, `Owner`,
  `Memory` (`resident_bytes` always, `footprint_bytes` where readable) and
  cumulative CPU time. `usage_between` turns
  two `Snapshot`s into per-process `Usage` (cores busy on average, uptime) and
  measures a process new since the earlier snapshot, or one that reused a PID, from
  its start. The macOS adapter (`macos::process_sampler::MacProcessSampler`) reads
  one `ps -A -c` listing, because `ps` is setuid. Unprivileged libproc refuses
  `PROC_PIDTBSDINFO`, task info and rusage for root daemons, which were the culprit
  here. The adapter then adds `ri_phys_footprint` where `proc_pid_rusage` allows
  it, which is the service user's own processes. Root daemons' footprints, which
  count compressed pages that their resident size leaves out, need
  `top -l 1 -stats pid,mem` (setuid, about 1.8 s a call). That belongs in the
  adapter at a slower cadence, for the early-warning slice. A Linux adapter would
  read `/proc`.
- **Blocked state.** `outside_pressure::blocked::Blocked` holds the `Reason`
  (`memory_pressure` with its level, or `outside_saturation` with the outside
  utilization), `since`, `attributed_at`, and up to five `OutsideProcess`es, leading
  first. Peers read it with `blocked::lenient`, so a later build's reason makes it
  null instead of failing the whole status.
- **When.** `outside_pressure::episode::Watch` is a pure state machine with three
  states: `Open`, `Closed` and `Blocked`. A closure becomes `Blocked` once it has
  lasted `SUSTAINED` (5 minutes), if attribution finds outside processes. A closure
  resets if the gate stays open for `REOPENED` (10 s) first. A blocked state ends
  once the gate has stayed open for `CLEARED` (1 minute). While the gate stays
  closed, processes are sampled every `RESAMPLE` (1 minute), first as a baseline
  for CPU rates and then to refresh the attribution. `Watch::resumed` keeps the
  onset of a block that a restart interrupted.
- **Who.** `outside_pressure::attribution::attribute` takes an `OwnWork`: the
  service and the holders of active grants, whose descendants are all Elsecall's
  work. While a grant is active, the service user's processes started since the
  earliest active grant also count as Elsecall's work, because launchd adopts the
  simulators and build daemons that runs start. Memory pressure is ranked by
  resident size, the one measure every process has, and saturation by CPU. The
  closure is attributed outside only when outside processes outweigh Elsecall's
  own work. For CPU that means all of them together, so many small processes, such
  as an outside `make -j`, still count. For memory it means the five leading ones,
  because resident sizes count shared pages in every process. After a restart, an
  interrupted episode resumes on the first closed observation within 5 minutes of
  its last attribution, and it is attributed at once. The kernel and launchd are never named.
- **Episodes.** `outside_pressure::store::EpisodeStore` keeps one row per episode in
  `outside-pressure.sqlite`, keyed by its onset. Each row holds the latest
  attribution and `until`, and rows expire 30 days after their episode ends.
  `Detector::episodes(since, until)` returns the episodes that overlap a period,
  which is what reports need to name culprits.
- **Wiring.** `outside_pressure::Detector` follows a `GateSource` every second. In
  production that source is `outside_pressure_gate::SchedulerGate`, from
  `Scheduler::own_load`. The detector samples outside its lock, on a blocking
  thread. It reports through `BlockedReport` to `PeerStatus.blocked` (lifted to
  `members[].blocked` in the fleet view and to `FleetBuilds.blocked` for doctor),
  and through `GET /v1/outside-pressure` for this member alone. The remedy text is
  `outside_pressure::remedy::next_step`.

## Not in this slice

- Report findings that name culprits and long queues behind a closed gate: since added
  as `outside_pressure_blocked` and `queued_on_closed_gate` (README "Fleet reports"),
  from members' episodes over `POST /peer/v1/report/outside`.
- Early warning for outside processes that spin or grow for hours before pressure
  closes the gate: see `outside-process-warning-slice.md`.
- Placement explanations that point at the blocked state.
