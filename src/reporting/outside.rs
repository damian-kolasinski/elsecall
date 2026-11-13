//! A member's outside-pressure history, as a report reads it: the blocked
//! episodes the member recorded, which name the outside processes that held its
//! gate closed, and its early warnings about outside processes that spun or
//! grew before any did. Members answer `POST /peer/v1/report/outside` with it; another
//! member's answer is untrusted and bounded by [`bounded`].
use crate::{
    outside_pressure::{
        blocked::{OutsideProcess, Reason, lenient_list},
        store::Episode,
        warning::{Concern, WarningEpisode},
    },
    reporting::{blocked::Pressure, window::Window},
    scheduling::timestamp::Timestamp,
};
use serde::{Deserialize, Serialize};

/// The most episodes one answer carries, the longest kept, so the largest fits
/// a member response…
pub const MAXIMUM_EPISODES: usize = 200;
/// …each naming at most this many outside processes, as members record them…
pub const MAXIMUM_PROCESSES: usize = 5;
/// …of names at most this long; `ps -c` prints shorter ones.
pub const MAXIMUM_NAME: usize = 64;

/// What a member recorded of outside pressure over a period. Each field is
/// absent from members on earlier versions, and entries a build cannot decode
/// are dropped, so later builds can add to it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct OutsideHistory {
    /// Blocked episodes that overlap the period, oldest first.
    #[serde(default, deserialize_with = "lenient_list")]
    pub episodes: Vec<Episode>,
    /// Early warnings about outside processes that held at some time in the
    /// period, oldest first.
    #[serde(default, deserialize_with = "lenient_list")]
    pub warnings: Vec<WarningEpisode>,
}

/// The most early warnings one answer carries, the latest kept.
pub const MAXIMUM_WARNINGS: usize = 200;

/// Members' clocks may differ by a little.
const CLOCK_SLACK_MS: u64 = 60_000;

fn believable_name(name: &str) -> bool {
    !name.trim().is_empty() && name.chars().count() <= MAXIMUM_NAME && !name.chars().any(misleading)
}

/// Whether a warning is plausible for `window`: a believable name, finite
/// measures, and times in order that end within it and not after its end.
fn believable_warning(episode: &WarningEpisode, window: Window) -> bool {
    let warning = &episode.warning;
    let measures = match warning.concern {
        Concern::Spinning { cpu_cores } => {
            cpu_cores.is_finite()
                && (0.0..=crate::reporting::leftovers::MOST_CORES as f64).contains(&cpu_cores)
        }
        Concern::Growing { .. } => true,
        Concern::Oversized {
            share_of_memory, ..
        } => share_of_memory.is_finite() && (0.0..=1.5).contains(&share_of_memory),
    };
    measures
        && believable_name(&warning.process)
        && warning.started <= warning.since
        && warning.since <= warning.observed_at
        && warning.observed_at.millis() <= window.until.millis() + CLOCK_SLACK_MS
        && warning.since < window.until
        && episode.until.is_none_or(|until| {
            until >= window.since && until >= warning.since && until <= warning.observed_at
        })
}

/// Characters that reorder or hide text where reports are read, or would end
/// the backticks that findings quote a name in.
fn misleading(c: char) -> bool {
    c.is_control()
        || c == '`'
        || matches!(c, '\u{ad}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}')
}

fn believable(process: &OutsideProcess) -> bool {
    believable_name(&process.name)
        && process.cpu_cores.is_finite()
        && (0.0..=crate::reporting::leftovers::MOST_CORES as f64).contains(&process.cpu_cores)
}

/// `history` within bounds, as a member answers it and as another member's
/// answer is believed: episodes of `window` that end no earlier than they
/// start, the [`MAXIMUM_EPISODES`] longest, oldest first, each naming at most
/// [`MAXIMUM_PROCESSES`] plausible processes; and plausible early warnings of
/// `window`, the [`MAXIMUM_WARNINGS`] latest, oldest first.
pub fn bounded(mut history: OutsideHistory, window: Window) -> OutsideHistory {
    history.episodes.retain(|e| {
        e.blocked.since < window.until
            && e.until
                .is_none_or(|until| until >= e.blocked.since && until >= window.since)
    });
    if history.episodes.len() > MAXIMUM_EPISODES {
        history
            .episodes
            .sort_by_key(|e| std::cmp::Reverse(ended(e, window).since(e.blocked.since)));
        history.episodes.truncate(MAXIMUM_EPISODES);
        history.episodes.sort_by_key(|e| e.blocked.since);
    }
    for episode in &mut history.episodes {
        episode.blocked.outside.retain(believable);
        episode.blocked.outside.truncate(MAXIMUM_PROCESSES);
    }
    history
        .warnings
        .retain(|warning| believable_warning(warning, window));
    history.warnings.sort_by_key(|w| w.warning.since);
    let excess = history.warnings.len().saturating_sub(MAXIMUM_WARNINGS);
    history.warnings.drain(..excess);
    history
}

/// When `episode` ended, as far as `window` tells: one still lasting runs to
/// the period's end.
pub fn ended(episode: &Episode, window: Window) -> Timestamp {
    episode.until.unwrap_or(window.until)
}

/// Whether an episode recorded for `reason` was about `pressure`: its processes
/// are ranked by that pressure's measure.
fn about(reason: &Reason, pressure: Pressure) -> bool {
    matches!(
        (reason, pressure),
        (Reason::MemoryPressure { .. }, Pressure::MemoryPressure)
            | (Reason::OutsideSaturation { .. }, Pressure::CpuSaturation)
    )
}

/// The outside processes of the episode in `episodes`, about `pressure`, that
/// overlaps `[from, to)` the longest, leading first; none if no episode does.
pub fn behind(
    episodes: &[Episode],
    pressure: Pressure,
    window: Window,
    from: Timestamp,
    to: Timestamp,
) -> Vec<OutsideProcess> {
    episodes
        .iter()
        .filter(|e| about(&e.blocked.reason, pressure))
        .filter_map(|e| {
            let start = e.blocked.since.max(from);
            let end = ended(e, window).min(to);
            (end > start).then(|| (end.since(start), e))
        })
        .max_by_key(|(overlap, e)| (*overlap, std::cmp::Reverse(e.blocked.since)))
        .map(|(_, e)| e.blocked.outside.clone())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        inspection::MemoryPressure,
        outside_pressure::{
            blocked::Blocked,
            sampling::{Memory, Owner},
        },
    };

    fn at(minute: u64) -> Timestamp {
        Timestamp::from_millis(minute * 60_000)
    }

    fn process(name: &str) -> OutsideProcess {
        OutsideProcess {
            name: name.into(),
            pid: 412,
            owner: Owner::Root,
            memory: Memory {
                resident_bytes: 1 << 30,
                footprint_bytes: None,
            },
            cpu_cores: 1.0,
            uptime_seconds: 60,
        }
    }

    fn episode(since: u64, until: Option<u64>, names: &[&str]) -> Episode {
        Episode {
            blocked: Blocked {
                reason: Reason::MemoryPressure {
                    level: MemoryPressure::Warning,
                },
                since: at(since),
                attributed_at: at(since + 5),
                outside: names.iter().map(|n| process(n)).collect(),
            },
            until: until.map(at),
        }
    }

    fn window() -> Window {
        Window::new(at(100), at(200), at(200)).unwrap()
    }

    #[test]
    fn the_episode_overlapping_longest_names_the_processes() {
        let episodes = [
            episode(90, Some(110), &["mds"]),
            episode(120, Some(170), &["fseventsd", "mds"]),
            episode(180, None, &["backupd"]),
        ];
        let names = |from, to| -> Vec<String> {
            behind(
                &episodes,
                Pressure::MemoryPressure,
                window(),
                at(from),
                at(to),
            )
            .into_iter()
            .map(|p| p.name)
            .collect()
        };
        assert_eq!(names(100, 200), ["fseventsd", "mds"]);
        assert_eq!(names(100, 115), ["mds"]);
        // The lasting episode runs to the period's end.
        assert_eq!(names(175, 200), ["backupd"]);
        assert!(names(111, 119).is_empty());
        // Memory episodes rank by memory, so they do not name CPU culprits.
        assert!(
            behind(
                &episodes,
                Pressure::CpuSaturation,
                window(),
                at(100),
                at(200)
            )
            .is_empty()
        );
    }

    #[test]
    fn another_members_episodes_are_bounded_and_later_ones_tolerated() {
        let mut wild = episode(120, Some(130), &["fseventsd"]);
        let mut bad = process("a\nb");
        bad.cpu_cores = f64::NAN;
        wild.blocked.outside.insert(0, bad);
        let mut busy = process("busy");
        busy.cpu_cores = 1e300;
        for name in ["   ", "evil\u{202e}dc", "x` then `y"] {
            wild.blocked.outside.insert(0, process(name));
        }
        wild.blocked.outside.insert(0, busy);
        wild.blocked
            .outside
            .extend((0..10).map(|n| process(&format!("p{n}"))));
        let backwards = episode(150, Some(140), &["x"]);
        let before = episode(10, Some(20), &["x"]);
        let history = OutsideHistory {
            episodes: vec![wild, backwards, before],
            ..Default::default()
        };
        let believed = bounded(history, window());
        assert_eq!(believed.episodes.len(), 1);
        let names: Vec<&str> = believed.episodes[0]
            .blocked
            .outside
            .iter()
            .map(|p| p.name.as_str())
            .collect();
        assert_eq!(names, ["fseventsd", "p0", "p1", "p2", "p3"]);

        let mut json = serde_json::to_value(&believed).unwrap();
        json["episodes"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({"reason": "disk_full", "since": "2026-11-10T04:48:00Z"}));
        json["warnings"] = serde_json::json!([]);
        let read: OutsideHistory = serde_json::from_value(json).unwrap();
        assert_eq!(read, believed);
        assert_eq!(
            serde_json::from_str::<OutsideHistory>("{}").unwrap(),
            OutsideHistory::default()
        );
    }

    #[test]
    fn keeps_only_plausible_warnings_of_the_period() {
        use crate::outside_pressure::warning::Warning;
        let warning =
            |process: &str, since: u64, observed: u64, until: Option<u64>| WarningEpisode {
                warning: Warning {
                    concern: Concern::Spinning { cpu_cores: 1.0 },
                    process: process.into(),
                    pid: 412,
                    started: at(0),
                    owner: Owner::Root,
                    since: at(since),
                    observed_at: at(observed),
                },
                until: until.map(at),
            };
        let mut nan = warning("nan", 110, 150, None);
        nan.warning.concern = Concern::Spinning {
            cpu_cores: f64::NAN,
        };
        let mut skewed = warning("skewed", 110, 200, None);
        skewed.warning.observed_at = Timestamp::from_millis(at(200).millis() + 30_000);
        let history = OutsideHistory {
            warnings: vec![
                warning("fseventsd", 90, 150, None),
                skewed,
                warning("fsevents\u{202e}d", 110, 150, None),
                warning("bad\nname", 110, 150, None),
                nan,
                warning("ahead", 110, 203, None),
                warning("backwards", 150, 160, Some(140)),
                warning("before", 10, 20, Some(20)),
            ],
            ..Default::default()
        };
        let kept: Vec<String> = bounded(history, window())
            .warnings
            .into_iter()
            .map(|w| w.warning.process)
            .collect();
        assert_eq!(kept, ["fseventsd", "skewed"]);
        let many = OutsideHistory {
            warnings: (0..MAXIMUM_WARNINGS as u64 + 5)
                .map(|n| warning(&format!("p{n}"), 101, 150, None))
                .collect(),
            ..Default::default()
        };
        assert_eq!(bounded(many, window()).warnings.len(), MAXIMUM_WARNINGS);
    }
}
