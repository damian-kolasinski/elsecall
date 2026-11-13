# Early warning for outside processes that spin or grow

On 2026-11-10 a hung `fseventsd` on macbook-pro had spun a core for days and grown
to a 56 GB footprint before memory pressure closed the member's gate at 04:48Z and
kept it out of the fleet for eight hours. The outside-pressure slice says so once the
gate is closed. This slice warns before: an outside process that has spun about a
core, or grown in memory, for hours shows per member in `elsecall fleet` and
`elsecall doctor`, as a warning and not a block, and as findings in `elsecall
report`. User-facing behaviour is in the README under "Fleet", "Early warnings".

## What the history showed

The MacBook's own `load.sqlite` (read-only, Nov 3 12:17Z to Nov 10 13:10Z) has the
CPU floor of minutes in which Elsecall held no grant, in cores of 10:

| Period (UTC) | Floor | Memory-pressure minutes |
| --- | --- | --- |
| Nov 3 18:00 to Nov 7 06:00 | 2.01 | 0 |
| Nov 7 12:00 to Nov 8 18:00 | 3.02 | 0 |
| Nov 8 21:00 to Nov 10 12:00 | 1.15 to 1.32 | rising to every minute |

The floor is the *least* busy idle minute, so outside processes held whole cores
continuously at minute resolution, for days. Spinners are not bursty. The step to
1.16 from Nov 8 21:00 is `fseventsd` at a core over a 0.15-core baseline. What held
two or three cores before cannot be named: no per-process history existed.

On the mini, the largest long-lived consumer is Spotlight's `mds_stores`: 0.69 core
on average over 64 hours, in 45-second samples 0.42 to 0.65. No process on either
machine would warn today.

## Decisions

- **Sampling.** The outside-pressure slice's `ProcessSampler` port and its one `ps`
  listing (setuid, so root daemons are readable; about 20 ms for 1,200 processes),
  every 5 minutes, whether or not the gate is open. The first round after a start
  only sets the CPU baseline. Footprint is used where libproc allows (the service
  user's processes), resident size otherwise; `top -l 1` would read root daemons'
  footprints but costs about a second of system time a call, and before pressure
  starts, resident size and footprint of a leaking daemon are close.
- **Identity, not name.** A process is followed by PID and start time. Builds and
  test runs are many short-lived processes, so none keeps one identity busy for six
  hours; a spinning or leaking daemon does.
- **Own work.** `attribution::outside` excludes the service, grant holders and their
  descendants, this user's processes started during an active grant, the kernel and
  launchd, exactly as blocked states do. On top of that, a process that was ever own
  work stays so until it exits (`own` table): Gradle or Bazel servers, sccache and VM
  processes one grant starts serve later grants that started after them. The
  tradeoff is that this user's apps started during a grant are never warned about;
  root and other users' daemons, the motivating case, are unaffected. An
  Elsecall-granted build never accumulates a window.
- **Continuity.** `since` is clamped to the process's start, so peers never see a
  concern that predates its process. A warning that holds again within an hour of
  ending continues its episode, so a one-round lapse (a collection, noise) does not
  split it. A restart shows the warnings the store left open until its first judged
  round confirms or ends them; a failed sample clears what is shown.
- **Thresholds** (`trend`): spinning is a 6-hour mean of 0.8 cores or more with 90% of
  rounds at half a core or more. 0.8 leaves room for a full-core spinner squeezed by
  heavy work; the 90% floor rejects bursts and Spotlight-like churn. Growing is at
  least 1 GiB and 25% over 6 hours, to at least the larger of 2 GiB and 5% of
  physical memory, with each quarter's median above the last, which a one-off load
  step does not give. Oversized is a quarter of physical memory in every round of an
  hour, and replaces growing for the same process. A window is judged only when its
  rounds reach back to its start and at least 80% of them were taken.
- **Storage** (`trend_store`, `outside-processes.sqlite`): rounds of candidates (a
  quarter core or 256 MiB) for 12 hours, so a restart does not restart a window, and
  warnings keyed by concern, process identity and onset, ended at the last round
  that saw them, for 30 days. Names, PIDs and start times only.
- **Surface.** `PeerStatus.outside_warnings`, lifted to `members[].outside_warnings`
  in `elsecall fleet` and `FleetBuilds.outside_warnings` for doctor, whose
  `next_step` uses `remedy::warning_step` when nothing more urgent applies. The local
  `GET /v1/outside-pressure` adds `warnings`. Peers read both leniently, so a later
  concern is dropped rather than failing a status.
- **Reports.** The report-findings slice's member route,
  `POST /peer/v1/report/outside`, answers `reporting::outside::OutsideHistory`; this
  slice adds `warnings` beside its blocked `episodes`. `bounded` holds another
  member's warnings to believable names, finite measures and times in order within
  the period (a minute of clock difference allowed), the 200 latest. Members'
  `outside_warnings` in the report carry how long each had held; findings
  `outside_process_spinning` and `outside_process_growing` name the process and use
  the same remedy wording as blocked findings (`remedy::act`). `definitions` is 7.
- **Remedy.** Elsecall never stops anything. The wording reuses the blocked state's
  restart advice: `sudo kill PID` for root and other users' processes after checking
  the PID still names them, `kill PID` for this user's, closing apps or rebooting for
  processes whose restart ends the session.

## Not in this slice

- Root daemons' footprints through `top`, at a slower cadence than CPU.
- Naming which of Elsecall's lost runs left an outside process behind.
