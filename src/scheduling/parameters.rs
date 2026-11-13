//! The values placement, the load gate and learned estimates decide with. The
//! operator sets them in `config.toml`; every one defaults to the value S4 shipped
//! with. Decisions record the values they used, so `explain` shows them and
//! `replay` can re-score a decision under other values.
//!
//! The same keys serve `config.toml` (as sections) and `replay --set` (as
//! `section.key=value`), and both go through the same validation.
use crate::inspection::MemoryPressure;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Every tunable value, grouped as in `config.toml`.
///
/// Records keep these, so decoding is tolerant: a key a record lacks takes its
/// default and a key this version does not know is ignored. `config.toml` and
/// `replay --set` are strict, through [`Overrides`].
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Parameters {
    pub placement: Placement,
    pub admission: Admission,
    pub estimates: Estimates,
}

/// How placement scores candidates. On a fleet, the master's values govern.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Placement {
    /// Another member must complete earlier than the caller by this share of the
    /// caller's expected completion…
    pub keep_local_margin: f64,
    /// …and by at least this much.
    #[serde(with = "text")]
    pub keep_local_margin_floor: Duration,
    /// The run time of a label without history, as work at speed 1.
    #[serde(with = "text")]
    pub run_prior: Duration,
    /// Sync priors by the state of the member's copy.
    #[serde(with = "text")]
    pub sync_prior_warm: Duration,
    #[serde(with = "text")]
    pub sync_prior_cold_with_history: Duration,
    #[serde(with = "text")]
    pub sync_prior_cold: Duration,
    /// How much longer a run into a cold copy takes when no cold run was learned.
    pub cold_run_factor: f64,
    /// How long running or queued work without an expected run time holds its
    /// cores: queued work, and running work as it starts.
    #[serde(with = "text")]
    pub unknown_remaining: Duration,
    /// Running work without an expected run time is assumed to run at least this
    /// share of the time it has already run for longer: the longer it has run,
    /// the longer it is likely to keep running. 0 keeps `unknown_remaining` fixed.
    pub unknown_remaining_per_elapsed: f64,
    /// The most a contended run is assumed to slow down.
    pub maximum_slowdown: f64,
    /// How often waiting placeable requests are quoted and scored again.
    #[serde(with = "text")]
    pub reconsider_every: Duration,
    /// A member's lost runs count against at least this much placed work
    /// there, so one early loss does not read as a member that loses all.
    #[serde(with = "text")]
    pub minimum_loss_exposure: Duration,
}

/// When a member admits nothing new. Each member's own values gate its admission.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Admission {
    /// Outside CPU utilization, averaged over three seconds, at which a member
    /// counts as saturated.
    pub saturated_utilization: f64,
    /// The memory pressure level from which a member admits nothing new.
    pub memory_pressure: BlockingPressure,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockingPressure {
    Warning,
    Critical,
}
impl BlockingPressure {
    pub fn blocks(self, level: MemoryPressure) -> bool {
        match self {
            BlockingPressure::Warning => level != MemoryPressure::Normal,
            BlockingPressure::Critical => level == MemoryPressure::Critical,
        }
    }
}

/// How much history learned run times, sizes and syncs trust.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Estimates {
    /// Only runs and syncs requested this recently count.
    #[serde(with = "text")]
    pub recency: Duration,
    /// At most this many of the latest samples of each kind count.
    pub window: usize,
    /// Runs needed before a run time or size is learned.
    pub minimum_runs: usize,
    /// Warm syncs of the same workspace to a member needed to learn its sync.
    pub minimum_workspace_syncs: usize,
    /// Warm syncs of any workspace to a member needed otherwise.
    pub minimum_member_syncs: usize,
    /// First syncs of the same repository needed to learn a cold sync.
    pub minimum_repository_syncs: usize,
    /// Runs of a label's family needed before they stand in for the label.
    pub minimum_family_runs: usize,
    /// Labels two members must both have run before the ratio of their run
    /// times converts runs between them instead of their measured speeds.
    pub minimum_ratio_labels: usize,
    /// Runs shorter than this are mostly start-up: families and speed ratios
    /// leave them out.
    #[serde(with = "text")]
    pub short_run: Duration,
    /// How far back placed runs that lost their connection, and the placed
    /// work beside them, count against a member. 0 counts none.
    #[serde(with = "text")]
    pub loss_window: Duration,
}

impl Default for Placement {
    fn default() -> Self {
        Placement {
            keep_local_margin: 0.10,
            keep_local_margin_floor: Duration::from_secs(2),
            run_prior: Duration::from_secs(60),
            sync_prior_warm: Duration::from_secs(3),
            sync_prior_cold_with_history: Duration::from_secs(30),
            sync_prior_cold: Duration::from_secs(120),
            cold_run_factor: 3.0,
            unknown_remaining: Duration::from_secs(60),
            unknown_remaining_per_elapsed: 0.5,
            maximum_slowdown: 4.0,
            reconsider_every: Duration::from_secs(1),
            minimum_loss_exposure: Duration::from_secs(60 * 60),
        }
    }
}
impl Default for Admission {
    fn default() -> Self {
        Admission {
            saturated_utilization: 0.9,
            memory_pressure: BlockingPressure::Warning,
        }
    }
}
impl Default for Estimates {
    fn default() -> Self {
        Estimates {
            recency: Duration::from_secs(14 * 24 * 60 * 60),
            window: 20,
            minimum_runs: 3,
            minimum_workspace_syncs: 2,
            minimum_member_syncs: 3,
            minimum_repository_syncs: 1,
            minimum_family_runs: 3,
            minimum_ratio_labels: 1,
            short_run: Duration::from_secs(1),
            loss_window: Duration::from_secs(DAY),
        }
    }
}

impl Placement {
    /// How much earlier another member must complete than the caller.
    pub fn keep_local_margin(&self, caller: Duration) -> Duration {
        caller
            .mul_f64(self.keep_local_margin)
            .max(self.keep_local_margin_floor)
    }
    /// How much earlier another place must complete before waiting work moves:
    /// the keep-local margin, so a request does not flap between near equals.
    pub fn moving_margin(&self, current: Duration) -> Duration {
        self.keep_local_margin(current)
    }
    /// How much longer running work without an expected run time is assumed to
    /// hold its cores, after it has run for `elapsed`: `unknown_remaining`, or
    /// `unknown_remaining_per_elapsed` of `elapsed` once that is longer. Without
    /// `elapsed`, as in records from before it was recorded, `unknown_remaining`.
    pub fn unknown_remaining_after(&self, elapsed: Option<Duration>) -> Duration {
        let grown = elapsed.map_or(Duration::ZERO, |elapsed| {
            elapsed.mul_f64(self.unknown_remaining_per_elapsed)
        });
        self.unknown_remaining.max(grown)
    }
}

/// Keys as `config.toml` sections and `replay --set` name them, every value
/// optional; what is left out keeps its base value.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Overrides {
    #[serde(default)]
    pub placement: PlacementOverrides,
    #[serde(default)]
    pub admission: AdmissionOverrides,
    #[serde(default)]
    pub estimates: EstimatesOverrides,
}
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlacementOverrides {
    #[serde(skip_serializing_if = "Option::is_none")]
    keep_local_margin: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    keep_local_margin_floor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    run_prior: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sync_prior_warm: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sync_prior_cold_with_history: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sync_prior_cold: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cold_run_factor: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    unknown_remaining: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    unknown_remaining_per_elapsed: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    maximum_slowdown: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reconsider_every: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    minimum_loss_exposure: Option<String>,
}
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmissionOverrides {
    #[serde(skip_serializing_if = "Option::is_none")]
    saturated_utilization: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    memory_pressure: Option<BlockingPressure>,
}
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EstimatesOverrides {
    #[serde(skip_serializing_if = "Option::is_none")]
    recency: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    window: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    minimum_runs: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    minimum_workspace_syncs: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    minimum_member_syncs: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    minimum_repository_syncs: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    minimum_family_runs: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    minimum_ratio_labels: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    short_run: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    loss_window: Option<String>,
}

/// Every key, as `section.key`, with the range it accepts.
pub const KEYS: &[(&str, &str)] = &[
    (
        "placement.keep_local_margin",
        "a share from 0 to 1, such as 0.1",
    ),
    (
        "placement.keep_local_margin_floor",
        "a duration up to 10m, such as \"2s\"",
    ),
    ("placement.run_prior", "a duration from 1s to 24h"),
    ("placement.sync_prior_warm", "a duration up to 24h"),
    (
        "placement.sync_prior_cold_with_history",
        "a duration up to 24h",
    ),
    ("placement.sync_prior_cold", "a duration up to 24h"),
    ("placement.cold_run_factor", "a number from 1 to 100"),
    ("placement.unknown_remaining", "a duration from 1s to 24h"),
    (
        "placement.unknown_remaining_per_elapsed",
        "a number from 0 to 10, such as 0.5",
    ),
    ("placement.maximum_slowdown", "a number from 1 to 100"),
    ("placement.reconsider_every", "a duration from 250ms to 1m"),
    (
        "placement.minimum_loss_exposure",
        "a duration from 1m to 7d",
    ),
    (
        "admission.saturated_utilization",
        "a share above 0, up to 1",
    ),
    ("admission.memory_pressure", "\"warning\" or \"critical\""),
    ("estimates.recency", "a duration from 1h to 30d"),
    ("estimates.window", "a count from 1 to 100"),
    ("estimates.minimum_runs", "a count from 1 to the window"),
    (
        "estimates.minimum_workspace_syncs",
        "a count from 1 to the window",
    ),
    (
        "estimates.minimum_member_syncs",
        "a count from 1 to the window",
    ),
    (
        "estimates.minimum_repository_syncs",
        "a count from 1 to the window",
    ),
    (
        "estimates.minimum_family_runs",
        "a count from 1 to the window",
    ),
    ("estimates.minimum_ratio_labels", "a count from 1 to 100"),
    ("estimates.short_run", "a duration up to 10m"),
    ("estimates.loss_window", "a duration up to 30d, 0 for none"),
];

const DAY: u64 = 24 * 60 * 60;

/// A duration as `config.toml` writes it, in whole milliseconds.
fn duration(text: &str) -> Option<Duration> {
    let parsed = humantime::parse_duration(text.trim()).ok()?;
    (parsed.subsec_nanos() % 1_000_000 == 0).then_some(parsed)
}

fn range_error(key: &str) -> String {
    let accepts = KEYS
        .iter()
        .find(|(k, _)| *k == key)
        .map_or("a valid value", |(_, accepts)| accepts);
    format!("{key} must be {accepts}.")
}

impl Overrides {
    /// Parses `section.key=value` settings, as `replay --set` takes them. A
    /// section may be left out, since every key is unique. A value that is not
    /// TOML, such as `5s`, is taken as text.
    pub fn parse_settings(settings: &[String]) -> Result<Self, String> {
        let mut table = toml::Table::new();
        for setting in settings {
            let Some((key, value)) = setting.split_once('=') else {
                return Err(format!("{setting:?} is not key=value."));
            };
            let key = key.trim();
            let full = if key.contains('.') {
                KEYS.iter().find(|(k, _)| *k == key)
            } else {
                KEYS.iter()
                    .find(|(k, _)| k.split_once('.').is_some_and(|(_, name)| name == key))
            }
            .map(|(k, _)| *k)
            .ok_or_else(|| {
                format!(
                    "{key} is not a parameter; the parameters are {}.",
                    KEYS.iter().map(|(k, _)| *k).collect::<Vec<_>>().join(", ")
                )
            })?;
            let (section, name) = full.split_once('.').expect("keys have sections");
            let value = value.trim();
            let value = toml::from_str::<toml::Table>(&format!("v = {value}"))
                .ok()
                .and_then(|mut t| t.remove("v"))
                .unwrap_or_else(|| toml::Value::String(value.to_owned()));
            let section = table
                .entry(section)
                .or_insert_with(|| toml::Value::Table(toml::Table::new()));
            if let toml::Value::Table(section) = section
                && section.insert(name.to_owned(), value).is_some()
            {
                return Err(format!("{full} is set twice."));
            }
        }
        Overrides::deserialize(toml::Value::Table(table)).map_err(|e| e.message().to_owned())
    }

    /// Whether any value is set.
    pub fn is_empty(&self) -> bool {
        self.changed_keys().is_empty()
    }

    /// The `section.key` names this sets, in the order of [`KEYS`].
    pub fn changed_keys(&self) -> Vec<&'static str> {
        let Ok(toml::Value::Table(sections)) = toml::Value::try_from(self) else {
            return Vec::new();
        };
        let set = |key: &str| {
            let (section, name) = key.split_once('.').expect("keys have sections");
            sections
                .get(section)
                .and_then(|s| s.as_table())
                .is_some_and(|s| s.contains_key(name))
        };
        KEYS.iter().map(|(k, _)| *k).filter(|k| set(k)).collect()
    }

    /// `base` with these values in place, validated as a whole.
    pub fn apply(&self, base: &Parameters) -> Result<Parameters, String> {
        let p = &self.placement;
        let a = &self.admission;
        let e = &self.estimates;
        let time = |text: &Option<String>, base: Duration, key: &str| match text {
            None => Ok(base),
            Some(text) => duration(text).ok_or_else(|| range_error(key)),
        };
        let parameters = Parameters {
            placement: Placement {
                keep_local_margin: p
                    .keep_local_margin
                    .unwrap_or(base.placement.keep_local_margin),
                keep_local_margin_floor: time(
                    &p.keep_local_margin_floor,
                    base.placement.keep_local_margin_floor,
                    "placement.keep_local_margin_floor",
                )?,
                run_prior: time(
                    &p.run_prior,
                    base.placement.run_prior,
                    "placement.run_prior",
                )?,
                sync_prior_warm: time(
                    &p.sync_prior_warm,
                    base.placement.sync_prior_warm,
                    "placement.sync_prior_warm",
                )?,
                sync_prior_cold_with_history: time(
                    &p.sync_prior_cold_with_history,
                    base.placement.sync_prior_cold_with_history,
                    "placement.sync_prior_cold_with_history",
                )?,
                sync_prior_cold: time(
                    &p.sync_prior_cold,
                    base.placement.sync_prior_cold,
                    "placement.sync_prior_cold",
                )?,
                cold_run_factor: p.cold_run_factor.unwrap_or(base.placement.cold_run_factor),
                unknown_remaining: time(
                    &p.unknown_remaining,
                    base.placement.unknown_remaining,
                    "placement.unknown_remaining",
                )?,
                unknown_remaining_per_elapsed: p
                    .unknown_remaining_per_elapsed
                    .unwrap_or(base.placement.unknown_remaining_per_elapsed),
                maximum_slowdown: p
                    .maximum_slowdown
                    .unwrap_or(base.placement.maximum_slowdown),
                reconsider_every: time(
                    &p.reconsider_every,
                    base.placement.reconsider_every,
                    "placement.reconsider_every",
                )?,
                minimum_loss_exposure: time(
                    &p.minimum_loss_exposure,
                    base.placement.minimum_loss_exposure,
                    "placement.minimum_loss_exposure",
                )?,
            },
            admission: Admission {
                saturated_utilization: a
                    .saturated_utilization
                    .unwrap_or(base.admission.saturated_utilization),
                memory_pressure: a.memory_pressure.unwrap_or(base.admission.memory_pressure),
            },
            estimates: Estimates {
                recency: time(&e.recency, base.estimates.recency, "estimates.recency")?,
                window: e.window.unwrap_or(base.estimates.window),
                minimum_runs: e.minimum_runs.unwrap_or(base.estimates.minimum_runs),
                minimum_workspace_syncs: e
                    .minimum_workspace_syncs
                    .unwrap_or(base.estimates.minimum_workspace_syncs),
                minimum_member_syncs: e
                    .minimum_member_syncs
                    .unwrap_or(base.estimates.minimum_member_syncs),
                minimum_repository_syncs: e
                    .minimum_repository_syncs
                    .unwrap_or(base.estimates.minimum_repository_syncs),
                minimum_family_runs: e
                    .minimum_family_runs
                    .unwrap_or(base.estimates.minimum_family_runs),
                minimum_ratio_labels: e
                    .minimum_ratio_labels
                    .unwrap_or(base.estimates.minimum_ratio_labels),
                short_run: time(
                    &e.short_run,
                    base.estimates.short_run,
                    "estimates.short_run",
                )?,
                loss_window: time(
                    &e.loss_window,
                    base.estimates.loss_window,
                    "estimates.loss_window",
                )?,
            },
        };
        parameters.validate()?;
        Ok(parameters)
    }
}

impl Parameters {
    /// Checks every value against the range [`KEYS`] documents.
    pub fn validate(&self) -> Result<(), String> {
        let p = &self.placement;
        let a = &self.admission;
        let e = &self.estimates;
        let secs = Duration::from_secs;
        let millis = Duration::from_millis;
        let checks: [(bool, &str); 24] = [
            (
                (0.0..=1.0).contains(&p.keep_local_margin),
                "placement.keep_local_margin",
            ),
            (
                p.keep_local_margin_floor <= secs(600),
                "placement.keep_local_margin_floor",
            ),
            (
                (secs(1)..=secs(DAY)).contains(&p.run_prior),
                "placement.run_prior",
            ),
            (p.sync_prior_warm <= secs(DAY), "placement.sync_prior_warm"),
            (
                p.sync_prior_cold_with_history <= secs(DAY),
                "placement.sync_prior_cold_with_history",
            ),
            (p.sync_prior_cold <= secs(DAY), "placement.sync_prior_cold"),
            (
                (1.0..=100.0).contains(&p.cold_run_factor),
                "placement.cold_run_factor",
            ),
            (
                (secs(1)..=secs(DAY)).contains(&p.unknown_remaining),
                "placement.unknown_remaining",
            ),
            (
                (0.0..=10.0).contains(&p.unknown_remaining_per_elapsed),
                "placement.unknown_remaining_per_elapsed",
            ),
            (
                (1.0..=100.0).contains(&p.maximum_slowdown),
                "placement.maximum_slowdown",
            ),
            (
                (millis(250)..=secs(60)).contains(&p.reconsider_every),
                "placement.reconsider_every",
            ),
            (
                (secs(60)..=secs(7 * DAY)).contains(&p.minimum_loss_exposure),
                "placement.minimum_loss_exposure",
            ),
            (
                a.saturated_utilization > 0.0 && a.saturated_utilization <= 1.0,
                "admission.saturated_utilization",
            ),
            (true, "admission.memory_pressure"),
            (
                (secs(3600)..=secs(30 * DAY)).contains(&e.recency),
                "estimates.recency",
            ),
            ((1..=100).contains(&e.window), "estimates.window"),
            (
                (1..=e.window).contains(&e.minimum_runs),
                "estimates.minimum_runs",
            ),
            (
                (1..=e.window).contains(&e.minimum_workspace_syncs),
                "estimates.minimum_workspace_syncs",
            ),
            (
                (1..=e.window).contains(&e.minimum_member_syncs),
                "estimates.minimum_member_syncs",
            ),
            (
                (1..=e.window).contains(&e.minimum_repository_syncs),
                "estimates.minimum_repository_syncs",
            ),
            (
                (1..=e.window).contains(&e.minimum_family_runs),
                "estimates.minimum_family_runs",
            ),
            (
                (1..=100).contains(&e.minimum_ratio_labels),
                "estimates.minimum_ratio_labels",
            ),
            (e.short_run <= secs(600), "estimates.short_run"),
            (e.loss_window <= secs(30 * DAY), "estimates.loss_window"),
        ];
        match checks.iter().find(|(ok, _)| !ok) {
            Some((_, key)) => Err(range_error(key)),
            None => Ok(()),
        }
    }
}

/// Durations as `config.toml` writes them, such as `"2s"` or `"14days"`.
mod text {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::Duration;

    pub fn serialize<S: Serializer>(value: &Duration, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&humantime::format_duration(*value).to_string())
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Duration, D::Error> {
        let text = String::deserialize(deserializer)?;
        super::duration(&text).ok_or_else(|| serde::de::Error::custom("expected a duration"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid_and_round_trip_through_records() {
        let defaults = Parameters::default();
        defaults.validate().unwrap();
        let json = serde_json::to_string(&defaults).unwrap();
        assert!(json.contains(r#""keep_local_margin_floor":"2s""#), "{json}");
        assert_eq!(serde_json::from_str::<Parameters>(&json).unwrap(), defaults);
        assert_eq!(
            defaults.placement.keep_local_margin(Duration::from_secs(5)),
            Duration::from_secs(2)
        );
        assert_eq!(
            defaults
                .placement
                .keep_local_margin(Duration::from_secs(100)),
            Duration::from_secs(10)
        );
    }

    #[test]
    fn settings_override_with_or_without_their_section_and_are_validated() {
        let settings =
            |list: &[&str]| -> Vec<String> { list.iter().map(|s| (*s).into()).collect() };
        let base = Parameters::default();
        let overrides = Overrides::parse_settings(&settings(&[
            "placement.keep_local_margin=0.25",
            "keep_local_margin_floor=5s",
            "memory_pressure=critical",
            "estimates.minimum_runs = 5",
        ]))
        .unwrap();
        assert_eq!(
            overrides.changed_keys(),
            [
                "placement.keep_local_margin",
                "placement.keep_local_margin_floor",
                "admission.memory_pressure",
                "estimates.minimum_runs"
            ]
        );
        let changed = overrides.apply(&base).unwrap();
        assert_eq!(changed.placement.keep_local_margin, 0.25);
        assert_eq!(
            changed.placement.keep_local_margin_floor,
            Duration::from_secs(5)
        );
        assert_eq!(
            changed.admission.memory_pressure,
            BlockingPressure::Critical
        );
        assert_eq!(changed.estimates.minimum_runs, 5);
        assert_eq!(changed.placement.run_prior, base.placement.run_prior);

        for (bad, message) in [
            (vec!["margin=0.2"], "not a parameter"),
            (vec!["keep_local_margin"], "not key=value"),
            (vec!["keep_local_margin=2"], "from 0 to 1"),
            (vec!["run_prior=0s"], "from 1s to 24h"),
            (vec!["run_prior=soon"], "from 1s to 24h"),
            (vec!["run_prior=1.5ms"], "from 1s to 24h"),
            (vec!["minimum_runs=30"], "to the window"),
            (vec!["minimum_family_runs=0"], "to the window"),
            (vec!["minimum_ratio_labels=101"], "from 1 to 100"),
            (vec!["short_run=11m"], "up to 10m"),
            (vec!["loss_window=31d"], "up to 30d"),
            (vec!["minimum_loss_exposure=30s"], "from 1m to 7d"),
            (vec!["unknown_remaining_per_elapsed=-0.1"], "from 0 to 10"),
            (vec!["unknown_remaining_per_elapsed=11"], "from 0 to 10"),
            (vec!["unknown_remaining_per_elapsed=nan"], "from 0 to 10"),
            (vec!["memory_pressure=sometimes"], "unknown variant"),
            (vec!["window=3", "window=4"], "set twice"),
        ] {
            let error = Overrides::parse_settings(&settings(&bad))
                .and_then(|o| o.apply(&base))
                .unwrap_err();
            assert!(error.contains(message), "{bad:?}: {error}");
        }
    }

    #[test]
    fn recorded_parameters_read_back_with_keys_missing_or_unknown() {
        let read: Parameters = serde_json::from_str(
            r#"{"placement":{"keep_local_margin":0.3,"a_later_key":1},"later_section":{}}"#,
        )
        .unwrap();
        assert_eq!(read.placement.keep_local_margin, 0.3);
        assert_eq!(read.placement.run_prior, Duration::from_secs(60));
        assert_eq!(read.estimates, Estimates::default());
    }

    #[test]
    fn every_key_can_be_set_and_is_named_once() {
        let all: Vec<String> = KEYS
            .iter()
            .map(|(k, _)| {
                let value = match *k {
                    "admission.memory_pressure" => "critical",
                    "estimates.short_run" | "estimates.loss_window" => "2s",
                    "placement.minimum_loss_exposure" => "2m",
                    k if k.starts_with("estimates.") && k != "estimates.recency" => "2",
                    "placement.keep_local_margin" | "admission.saturated_utilization" => "0.5",
                    "placement.cold_run_factor"
                    | "placement.maximum_slowdown"
                    | "placement.unknown_remaining_per_elapsed" => "2.0",
                    _ => "2s",
                };
                format!("{k}={value}")
            })
            .collect();
        let overrides = Overrides::parse_settings(&all).unwrap();
        assert_eq!(
            overrides.changed_keys(),
            KEYS.iter().map(|(k, _)| *k).collect::<Vec<_>>()
        );
        assert!(Overrides::default().is_empty());
    }

    #[test]
    fn unknown_remaining_grows_with_elapsed_time_from_its_floor() {
        let secs = Duration::from_secs;
        let defaults = Parameters::default().placement;
        // Records from before elapsed time was recorded keep the fixed value.
        assert_eq!(defaults.unknown_remaining_after(None), secs(60));
        // The floor holds until half the elapsed time is longer: 120 s.
        for (elapsed, remaining) in [(0, 60), (60, 60), (120, 60), (121, 60), (644, 322)] {
            assert_eq!(
                defaults.unknown_remaining_after(Some(secs(elapsed))),
                if elapsed == 121 {
                    Duration::from_millis(60_500)
                } else {
                    secs(remaining)
                },
                "{elapsed} s"
            );
        }
        // Never shrinks as work keeps running.
        let mut last = Duration::ZERO;
        for elapsed in (0..3600).step_by(7) {
            let remaining = defaults.unknown_remaining_after(Some(secs(elapsed)));
            assert!(remaining >= last);
            last = remaining;
        }
        // 0 turns growth off; a larger share grows faster.
        let fixed = Placement {
            unknown_remaining_per_elapsed: 0.0,
            ..defaults.clone()
        };
        assert_eq!(fixed.unknown_remaining_after(Some(secs(3600))), secs(60));
        let lindy = Placement {
            unknown_remaining_per_elapsed: 1.0,
            unknown_remaining: secs(1),
            ..defaults
        };
        assert_eq!(lindy.unknown_remaining_after(Some(secs(30))), secs(30));
    }

    #[test]
    fn memory_pressure_blocks_from_its_level() {
        assert!(BlockingPressure::Warning.blocks(MemoryPressure::Warning));
        assert!(!BlockingPressure::Critical.blocks(MemoryPressure::Warning));
        assert!(BlockingPressure::Critical.blocks(MemoryPressure::Critical));
        assert!(!BlockingPressure::Warning.blocks(MemoryPressure::Normal));
    }
}
