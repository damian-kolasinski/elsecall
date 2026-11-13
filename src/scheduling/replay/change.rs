//! The parameters a replay changes: `--set` settings, through the same parser
//! and validation as `config.toml`, limited to what replay can re-score.
use crate::scheduling::parameters::{Overrides, Parameters};
use serde::{Serialize, Serializer, ser::SerializeMap};

/// Changes to the keep-local margin, the only ones a decision recorded without
/// its inputs can be replayed under: they choose between recorded completions.
const MARGIN: [&str; 2] = [
    "placement.keep_local_margin",
    "placement.keep_local_margin_floor",
];

/// Validated changed values, each applied over a decision's own recorded
/// parameters.
#[derive(Debug)]
pub struct Change {
    overrides: Overrides,
    keys: Vec<&'static str>,
}

/// Why replay cannot re-score under a key, or `None` when it can.
fn not_replayable(key: &str) -> Option<String> {
    if key == "placement.reconsider_every" {
        return Some(format!(
            "{key} changes when waiting requests are scored again, not how a scoring decides; replay re-scores recorded scorings and cannot replay it."
        ));
    }
    if key.starts_with("estimates.") {
        return Some(format!(
            "{key} changes which history is trusted; decisions record what history said, already resolved, so replay cannot re-derive it."
        ));
    }
    None
}

impl Change {
    /// Parses `key=value` or `section.key=value` settings. Values are checked
    /// against their documented ranges, and keys replay cannot re-score are
    /// refused with why.
    pub fn parse(settings: &[String]) -> Result<Self, String> {
        let overrides = Overrides::parse_settings(settings)?;
        let keys = overrides.changed_keys();
        if let Some(why) = keys.iter().find_map(|key| not_replayable(key)) {
            return Err(why);
        }
        overrides.apply(&Parameters::default())?;
        Ok(Self { overrides, keys })
    }

    /// The `section.key` names this changes.
    pub fn keys(&self) -> &[&'static str] {
        &self.keys
    }

    /// Whether this changes only the keep-local margin (or nothing).
    pub fn margin_only(&self) -> bool {
        self.keys.iter().all(|key| MARGIN.contains(key))
    }

    /// Whether this changes a load-gate threshold, so that every member's gate
    /// must be derived again from its recorded load.
    pub fn changes_admission(&self) -> bool {
        self.keys.iter().any(|key| key.starts_with("admission."))
    }

    /// `base` with the changed values in place, validated as a whole.
    pub fn apply(&self, base: &Parameters) -> Result<Parameters, String> {
        self.overrides.apply(base)
    }
}

/// As `section.key` and the value it was given.
impl Serialize for Change {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let sections = serde_json::to_value(&self.overrides).map_err(serde::ser::Error::custom)?;
        let mut map = serializer.serialize_map(Some(self.keys.len()))?;
        for key in &self.keys {
            let (section, name) = key.split_once('.').expect("keys have sections");
            map.serialize_entry(key, &sections[section][name])?;
        }
        map.end()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(settings: &[&str]) -> Result<Change, String> {
        Change::parse(&settings.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn replayable_keys_parse_and_others_are_refused_with_why() {
        let change = parse(&["keep_local_margin=0.3", "keep_local_margin_floor=5s"]).unwrap();
        assert!(change.margin_only());
        assert!(!change.changes_admission());
        assert_eq!(
            serde_json::to_value(&change).unwrap(),
            serde_json::json!({"placement.keep_local_margin": 0.3, "placement.keep_local_margin_floor": "5s"})
        );
        let change = parse(&["admission.saturated_utilization=0.95", "run_prior=30s"]).unwrap();
        assert!(!change.margin_only());
        assert!(change.changes_admission());
        assert_eq!(
            change.keys(),
            ["placement.run_prior", "admission.saturated_utilization"]
        );
        assert!(parse(&[]).unwrap().margin_only(), "nothing changed");

        let refused = parse(&["estimates.window=5"]).unwrap_err();
        assert!(refused.contains("cannot re-derive"), "{refused}");
        let refused = parse(&["reconsider_every=2s"]).unwrap_err();
        assert!(refused.contains("placement.reconsider_every"), "{refused}");
        let invalid = parse(&["keep_local_margin=2"]).unwrap_err();
        assert!(invalid.contains("from 0 to 1"), "{invalid}");
        assert!(parse(&["no_such_key=1"]).is_err());
    }
}
