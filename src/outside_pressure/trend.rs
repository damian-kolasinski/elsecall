//! Which outside processes have spun or grown for long enough to warn about,
//! from periodic rounds of samples. Pure: rounds in, warnings out.
//!
//! A process is followed by its identity, PID and start time, never by name:
//! builds and tests are many short-lived processes, so none of them keeps one
//! identity busy for hours, while a daemon that spins or leaks does. In the
//! 2026-11-10 incident, a hung `fseventsd` held one core in every idle minute
//! for two days, and its memory grew until pressure closed the gate.
use crate::{
    outside_pressure::{
        sampling::Owner,
        warning::{Concern, Warning},
    },
    scheduling::timestamp::Timestamp,
};
use std::{collections::BTreeMap, time::Duration};

/// How often processes are sampled for trends.
pub const INTERVAL: Duration = Duration::from_secs(5 * 60);

/// A process spins when, over this window…
pub const SPIN_WINDOW: Duration = Duration::from_secs(6 * 60 * 60);
/// …it kept at least this many cores busy on average…
pub const SPIN_CORES: f64 = 0.8;
/// …and at least this share of the window's rounds saw it at…
pub const SPIN_STEADY: f64 = 0.9;
/// …this many cores or more. A spinner squeezed by heavy work still passes;
/// Spotlight's bursty indexing, about half a core on average, does not.
pub const SPIN_FLOOR: f64 = 0.5;

/// A process grows when, over this window, its memory rose…
pub const GROWTH_WINDOW: Duration = Duration::from_secs(6 * 60 * 60);
/// …by at least this much…
pub const GROWTH_BYTES: u64 = 1 << 30;
/// …and this share of where it started, in every quarter of the window, to at
/// least the larger of these: an app that loads once and stays is not growing.
pub const GROWTH_SHARE: f64 = 0.25;
pub const GROWN_BYTES: u64 = 2 << 30;
pub const GROWN_SHARE_OF_MEMORY: f64 = 0.05;

/// A process is oversized when it held at least this share of the machine's
/// memory…
pub const OVERSIZED_SHARE: f64 = 0.25;
/// …in every round of this window.
pub const OVERSIZED_FOR: Duration = Duration::from_secs(60 * 60);

/// A window is judged only when at least this share of its rounds were taken:
/// a service that was down most of it cannot tell.
pub const COVERAGE: f64 = 0.8;

/// Rounds keep only outside processes at or above either of these; below
/// both, a process is neither spinning nor large.
pub const CANDIDATE_CORES: f64 = 0.25;
pub const CANDIDATE_BYTES: u64 = 256 << 20;

/// Rounds older than this are not needed to judge any window.
pub const KEPT: Duration = Duration::from_secs(12 * 60 * 60);

/// One outside process as one round saw it.
#[derive(Clone, Debug, PartialEq)]
pub struct Seen {
    pub pid: u32,
    pub started: Timestamp,
    pub name: String,
    pub owner: Owner,
    /// Cores busy on average since the previous round.
    pub cpu_cores: f64,
    /// Physical footprint where readable, resident size otherwise.
    pub memory_bytes: u64,
}

impl Seen {
    /// Whether a round keeps it.
    pub fn candidate(&self) -> bool {
        self.cpu_cores >= CANDIDATE_CORES || self.memory_bytes >= CANDIDATE_BYTES
    }
}

/// One sampling round: when, and the outside candidates it saw. A process
/// missing from a round was below both candidate floors, Elsecall's own work,
/// or not running.
#[derive(Clone, Debug, PartialEq)]
pub struct Round {
    pub at: Timestamp,
    pub seen: Vec<Seen>,
}

type Identity = (u32, Timestamp);

/// What warrants a warning as of the last of `rounds`, which are in time order.
/// `since` is as far back as the rounds show the concern holding.
pub fn concerns(rounds: &[Round], physical_memory: u64) -> Vec<Warning> {
    let Some(last) = rounds.last() else {
        return Vec::new();
    };
    let now = last.at;
    let series: BTreeMap<Identity, Vec<Option<&Seen>>> = last
        .seen
        .iter()
        .map(|process| {
            let identity = (process.pid, process.started);
            let track = rounds
                .iter()
                .map(|round| round.seen.iter().find(|s| (s.pid, s.started) == identity))
                .collect();
            (identity, track)
        })
        .collect();
    let mut warnings = Vec::new();
    for process in &last.seen {
        let track = &series[&(process.pid, process.started)];
        // A concern from the process's first round would start an interval
        // before the process did.
        let warn = |concern: Concern, since: Timestamp| Warning {
            concern,
            process: process.name.clone(),
            pid: process.pid,
            started: process.started,
            owner: process.owner,
            since: since.max(process.started),
            observed_at: now,
        };
        if let Some((cpu_cores, since)) = spinning(rounds, track, process.started) {
            warnings.push(warn(Concern::Spinning { cpu_cores }, since));
        }
        if let Some((share_of_memory, since)) = oversized(rounds, track, physical_memory) {
            warnings.push(warn(
                Concern::Oversized {
                    memory_bytes: process.memory_bytes,
                    share_of_memory,
                },
                since,
            ));
        } else if let Some((growth_bytes_per_hour, since)) =
            growing(rounds, track, process.started, physical_memory)
        {
            warnings.push(warn(
                Concern::Growing {
                    memory_bytes: process.memory_bytes,
                    growth_bytes_per_hour,
                },
                since,
            ));
        }
    }
    warnings
}

/// The index of the first round inside the window that ends with the last
/// round, if the rounds reach back to its start and cover enough of it, and
/// the process lived through it.
fn window(rounds: &[Round], length: Duration, started: Option<Timestamp>) -> Option<usize> {
    let now = rounds.last()?.at;
    let start = Timestamp::from_millis(now.millis().saturating_sub(length.as_millis() as u64));
    if started.is_some_and(|started| started > start) {
        return None;
    }
    let first = rounds.partition_point(|round| round.at <= start);
    let expected = length.as_secs_f64() / INTERVAL.as_secs_f64();
    (streak_start(rounds, first) <= start && (rounds.len() - first) as f64 >= COVERAGE * expected)
        .then_some(first)
}

fn cores(seen: Option<&Seen>) -> f64 {
    seen.map_or(0.0, |s| s.cpu_cores)
}

fn memory(seen: Option<&Seen>) -> u64 {
    seen.map_or(0, |s| s.memory_bytes)
}

/// Since a round's share covers the interval before it, a streak that starts
/// at a round started one interval earlier.
fn streak_start(rounds: &[Round], first: usize) -> Timestamp {
    match first.checked_sub(1) {
        Some(previous) => rounds[previous].at,
        None => Timestamp::from_millis(
            rounds[0]
                .at
                .millis()
                .saturating_sub(INTERVAL.as_millis() as u64),
        ),
    }
}

/// Mean cores over the window, and since when it has kept at least the floor.
fn spinning(
    rounds: &[Round],
    track: &[Option<&Seen>],
    started: Timestamp,
) -> Option<(f64, Timestamp)> {
    let first = window(rounds, SPIN_WINDOW, Some(started))?;
    let shares: Vec<f64> = track[first..].iter().map(|s| cores(*s)).collect();
    let mean = shares.iter().sum::<f64>() / shares.len() as f64;
    let steady = shares.iter().filter(|c| **c >= SPIN_FLOOR).count();
    if mean < SPIN_CORES || (steady as f64) < SPIN_STEADY * shares.len() as f64 {
        return None;
    }
    let mut earliest = first;
    while earliest > 0 && cores(track[earliest - 1]) >= SPIN_FLOOR {
        earliest -= 1;
    }
    Some((
        (mean * 100.0).round() / 100.0,
        streak_start(rounds, earliest),
    ))
}

/// Its share of memory now, and since when every round saw it that large.
fn oversized(
    rounds: &[Round],
    track: &[Option<&Seen>],
    physical_memory: u64,
) -> Option<(f64, Timestamp)> {
    let first = window(rounds, OVERSIZED_FOR, None)?;
    let large =
        |seen: Option<&Seen>| memory(seen) as f64 >= OVERSIZED_SHARE * physical_memory as f64;
    if physical_memory == 0 || !track[first..].iter().all(|s| large(*s)) {
        return None;
    }
    let mut earliest = first;
    while earliest > 0 && large(track[earliest - 1]) {
        earliest -= 1;
    }
    let share = memory(*track.last()?) as f64 / physical_memory as f64;
    Some((
        (share * 100.0).round() / 100.0,
        streak_start(rounds, earliest),
    ))
}

/// Bytes an hour it grew by over the window, from the window's start.
fn growing(
    rounds: &[Round],
    track: &[Option<&Seen>],
    started: Timestamp,
    physical_memory: u64,
) -> Option<(u64, Timestamp)> {
    let first = window(rounds, GROWTH_WINDOW, Some(started))?;
    let values: Vec<u64> = track[first..].iter().map(|s| memory(*s)).collect();
    let (start, now) = (*values.first()?, *values.last()?);
    let floor = GROWN_BYTES.max((GROWN_SHARE_OF_MEMORY * physical_memory as f64) as u64);
    let grown = now.saturating_sub(start);
    if now < floor || grown < GROWTH_BYTES || (grown as f64) < GROWTH_SHARE * start as f64 {
        return None;
    }
    // Steady growth: each quarter's median above the one before, which a single
    // step up, as an app loading a project, does not give.
    let quarter = values.len().div_ceil(4);
    let medians: Vec<u64> = values
        .chunks(quarter)
        .map(|chunk| {
            let mut sorted = chunk.to_vec();
            sorted.sort_unstable();
            sorted[sorted.len() / 2]
        })
        .collect();
    if medians.len() < 4 || medians.windows(2).any(|pair| pair[1] <= pair[0]) {
        return None;
    }
    let hours = rounds.last()?.at.since(rounds[first].at).as_secs_f64() / 3600.0;
    (hours > 0.0).then(|| ((grown as f64 / hours) as u64, streak_start(rounds, first)))
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;
    const T0: u64 = 1_794_000_000_000;

    fn rounds(count: usize, process: impl Fn(usize) -> Option<(f64, u64)>) -> Vec<Round> {
        (0..count)
            .map(|i| Round {
                at: Timestamp::from_millis(T0 + i as u64 * INTERVAL.as_millis() as u64),
                seen: process(i)
                    .map(|(cpu_cores, memory_bytes)| Seen {
                        pid: 412,
                        started: Timestamp::from_millis(0),
                        name: "fseventsd".into(),
                        owner: Owner::Root,
                        cpu_cores,
                        memory_bytes,
                    })
                    .into_iter()
                    .collect(),
            })
            .collect()
    }

    fn kinds(warnings: &[Warning]) -> Vec<&'static str> {
        warnings.iter().map(|w| w.concern.kind()).collect()
    }

    /// Rounds in six hours, the first of them on the window's start.
    const SIX_HOURS: usize = 73;

    #[test]
    fn a_steady_core_spins_once_the_window_is_covered() {
        let spinning = rounds(SIX_HOURS, |_| Some((1.0, 100 << 20)));
        let found = concerns(&spinning, 64 * GIB);
        assert_eq!(kinds(&found), ["spinning"]);
        assert_eq!(
            found[0].since,
            Timestamp::from_millis(T0 - INTERVAL.as_millis() as u64)
        );
        // Four hours of rounds cover too little of the window to judge.
        assert_eq!(concerns(&spinning[..50], 64 * GIB), []);
    }

    #[test]
    fn bursts_and_half_cores_do_not_spin() {
        // Spotlight: about half a core, never a whole one.
        assert_eq!(
            concerns(&rounds(SIX_HOURS, |_| Some((0.55, 0))), 64 * GIB),
            []
        );
        // Four cores for 40 minutes, then idle: a build, not a spinner.
        let burst = rounds(SIX_HOURS, |i| (i >= SIX_HOURS - 8).then_some((4.0, 0)));
        assert_eq!(concerns(&burst, 64 * GIB), []);
        // Half the window at two cores averages one, but is not continuous.
        let half = rounds(SIX_HOURS, |i| Some((if i % 2 == 0 { 2.0 } else { 0.1 }, 0)));
        assert_eq!(concerns(&half, 64 * GIB), []);
    }

    #[test]
    fn a_spinner_squeezed_now_and_then_still_spins() {
        let squeezed = rounds(SIX_HOURS, |i| {
            Some((if i % 12 == 0 { 0.3 } else { 0.95 }, 0))
        });
        assert_eq!(kinds(&concerns(&squeezed, 64 * GIB)), ["spinning"]);
    }

    #[test]
    fn steady_growth_warns_and_a_step_up_does_not() {
        // 3 GB to 9 GB over six hours.
        let leak = rounds(SIX_HOURS, |i| Some((0.0, 3 * GIB + i as u64 * GIB / 12)));
        let found = concerns(&leak, 64 * GIB);
        assert_eq!(kinds(&found), ["growing"]);
        let Concern::Growing {
            growth_bytes_per_hour,
            memory_bytes,
        } = found[0].concern
        else {
            unreachable!()
        };
        assert_eq!(memory_bytes, 9 * GIB);
        assert!(
            growth_bytes_per_hour.abs_diff(GIB) < GIB / 100,
            "{growth_bytes_per_hour}"
        );
        // An app that loads a project: 2 GB, then 8 GB from the third hour on.
        let step = rounds(SIX_HOURS, |i| {
            Some((0.0, if i < 30 { 2 * GIB } else { 8 * GIB }))
        });
        assert_eq!(concerns(&step, 64 * GIB), []);
        // A steady 8 GB is no warning.
        assert_eq!(
            concerns(&rounds(SIX_HOURS, |_| Some((0.0, 8 * GIB))), 64 * GIB),
            []
        );
    }

    #[test]
    fn a_concern_from_the_first_round_starts_no_earlier_than_the_process() {
        let started = Timestamp::from_millis(T0 - 2 * 60_000);
        let mut spinning = rounds(SIX_HOURS, |_| Some((1.0, 100 << 20)));
        for round in &mut spinning {
            for seen in &mut round.seen {
                seen.started = started;
            }
        }
        let found = concerns(&spinning, 64 * GIB);
        assert_eq!(kinds(&found), ["spinning"]);
        assert_eq!(found[0].since, started);
    }

    #[test]
    fn a_quarter_of_memory_for_an_hour_is_oversized_instead_of_growing() {
        let large = rounds(SIX_HOURS, |i| Some((0.0, 10 * GIB + i as u64 * GIB / 6)));
        let found = concerns(&large, 64 * GIB);
        assert_eq!(kinds(&found), ["oversized"]);
        let Concern::Oversized {
            share_of_memory, ..
        } = found[0].concern
        else {
            unreachable!()
        };
        assert_eq!(share_of_memory, 0.34);
        // Large in the last round only: not yet.
        let spike = rounds(SIX_HOURS, |i| {
            Some((0.0, if i + 1 == SIX_HOURS { 40 * GIB } else { GIB }))
        });
        assert_eq!(concerns(&spike, 64 * GIB), []);
    }

    #[test]
    fn a_process_younger_than_the_window_is_not_judged() {
        let mut young = rounds(SIX_HOURS, |_| Some((1.0, 0)));
        for round in &mut young {
            for seen in &mut round.seen {
                seen.started = Timestamp::from_millis(T0 + 60_000);
            }
        }
        assert_eq!(concerns(&young, 64 * GIB), []);
    }
}
