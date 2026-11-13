//! Explicit candidate settings for a simulation; never applies them to a service.
//!
//! In addition to the production `section.key=value` parameter grammar:
//! - `member.NAME.reserved_cores=CORES` changes one enrolled member's reserve;
//! - `label.PATTERN.size=CORES` changes matching requests' declared size;
//! - `hold_limit=DURATION` changes interactive hold limits only.
//!
//! Quote the entire argument when a pattern contains spaces. Patterns use the
//! consumer configuration's glob syntax and first matching rule wins, in argument
//! order. Duplicate keys are errors, as with production parameter overrides.
use crate::{
    consumer_configuration::ConsumerConfiguration,
    failure::{Failure, Result},
    label_pattern,
    roster::MemberName,
    scheduling::{
        capacity::{Capacity, Cores},
        parameters::{Overrides, Parameters},
        request::{MAXIMUM_HOLD_LIMIT, Urgency},
    },
    workload::trace::{Request, Trace},
};
use std::{collections::BTreeMap, time::Duration};

#[derive(Debug)]
pub struct Settings {
    parameters: Overrides,
    reserves: BTreeMap<String, Cores>,
    sizes: Vec<(String, Cores)>,
    hold_limit: Option<Duration>,
}

fn invalid(message: impl Into<String>) -> Failure {
    Failure::new(
        "invalid_request",
        message,
        "Run elsecall simulate --help; candidate settings never change installed services.",
    )
}
fn cores(value: &str, key: &str, positive: bool) -> Result<Cores> {
    Cores::parse(value.trim())
        .filter(|cores| !positive || *cores > Cores::ZERO)
        .ok_or_else(|| {
            invalid(format!(
                "{key} must be {}number of cores with at most three decimal places.",
                if positive {
                    "a positive "
                } else {
                    "a nonnegative "
                }
            ))
        })
}

impl Settings {
    pub fn parse(trace: &Trace, values: &[String]) -> Result<Self> {
        let mut parameters = Vec::new();
        let mut reserves = BTreeMap::new();
        let mut sizes = Vec::new();
        let mut hold_limit = None;
        for setting in values {
            let (key, value) = setting
                .rsplit_once('=')
                .ok_or_else(|| invalid("Settings must be key=value."))?;
            let key = key.trim();
            if let Some(member) = key
                .strip_prefix("member.")
                .and_then(|key| key.strip_suffix(".reserved_cores"))
            {
                let name = member
                    .parse::<MemberName>()
                    .map_err(|e| invalid(e.message))?;
                let baseline = trace
                    .members
                    .iter()
                    .find(|m| m.member == name.as_str())
                    .ok_or_else(|| invalid(format!("{member} is not a member of this trace.")))?;
                let reserve = cores(value, key, false)?;
                if baseline.scheduler_settings.is_empty() {
                    return Err(invalid(format!(
                        "members[{member}].scheduler_settings missing: cannot establish the approved reserve floor."
                    )));
                }
                for recorded in &baseline.scheduler_settings {
                    reserve_floor(member, reserve, &recorded.capacity)?;
                }
                if reserves.insert(member.to_owned(), reserve).is_some() {
                    return Err(invalid(format!("{key} is set twice.")));
                }
            } else if let Some(pattern) = key
                .strip_prefix("label.")
                .and_then(|key| key.strip_suffix(".size"))
            {
                if sizes.iter().any(|(previous, _)| previous == pattern) {
                    return Err(invalid(format!("{key} is set twice.")));
                }
                let size = cores(value, key, true)?;
                // Reuse consumer rule validation, including pattern limits and
                // syntax. Serialization quotes arbitrary literal patterns safely.
                let label = toml::Table::from_iter([
                    ("match".into(), toml::Value::String(pattern.to_owned())),
                    ("size".into(), toml::Value::String(size.to_string())),
                ]);
                let document = toml::Table::from_iter([(
                    "labels".into(),
                    toml::Value::Array(vec![toml::Value::Table(label)]),
                )]);
                ConsumerConfiguration::parse(
                    &toml::to_string(&document).expect("candidate label serializes"),
                )
                .map_err(|error| invalid(error.detail))?;
                if sizes.len() == 64 {
                    return Err(invalid(
                        "At most 64 candidate label size rules are supported.",
                    ));
                }
                sizes.push((pattern.to_owned(), size));
            } else if key == "hold_limit" {
                if hold_limit.is_some() {
                    return Err(invalid("hold_limit is set twice."));
                }
                let duration = humantime::parse_duration(value.trim())
                    .map_err(|_| invalid("hold_limit takes a duration such as 90s or 10m."))?;
                if duration > MAXIMUM_HOLD_LIMIT
                    || !duration.subsec_nanos().is_multiple_of(1_000_000)
                {
                    return Err(invalid(
                        "hold_limit must be at most 24h, in whole milliseconds.",
                    ));
                }
                hold_limit = Some(duration);
            } else {
                parameters.push(setting.clone());
            }
        }
        let parameters = Overrides::parse_settings(&parameters).map_err(invalid)?;
        let settings = Self {
            parameters,
            reserves,
            sizes,
            hold_limit,
        };
        for member in &trace.members {
            for snapshot in &member.scheduler_settings {
                settings.parameters(&snapshot.parameters)?;
            }
        }
        Ok(settings)
    }

    pub fn parameters(&self, baseline: &Parameters) -> Result<Parameters> {
        self.parameters.apply(baseline).map_err(invalid)
    }

    pub fn capacity(&self, member: &str, baseline: &Capacity) -> Result<Capacity> {
        let Some(&reserved) = self.reserves.get(member) else {
            return Ok(*baseline);
        };
        reserve_floor(member, reserved, baseline)?;
        Ok(Capacity {
            weighted_cores: baseline.weighted_cores,
            reserved,
            capacity: baseline.weighted_cores.saturating_sub(reserved),
        })
    }

    /// Only an explicit matching override; callers retain normal size learning
    /// and the original declaration when no candidate rule applies.
    pub fn size(&self, request: &Request) -> Option<Cores> {
        let label = request.fact.label.as_deref()?;
        self.sizes
            .iter()
            .find(|(pattern, _)| label_pattern::matches(pattern, label))
            .map(|(_, size)| *size)
    }

    pub fn urgency(&self, request: &Request) -> Urgency {
        match (request.urgency, self.hold_limit) {
            (Urgency::Interactive { .. }, Some(limit)) => Urgency::Interactive {
                hold_limit_ms: Some(limit.as_millis() as u64),
            },
            (original, _) => original,
        }
    }
}

fn reserve_floor(member: &str, reserved: Cores, baseline: &Capacity) -> Result<()> {
    if reserved < baseline.reserved {
        return Err(invalid(format!(
            "{member}'s candidate reserve {reserved} cores is below its recorded approved floor of {} cores.",
            baseline.reserved
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduling::{request::AtDeadline, timestamp::Timestamp};

    fn trace() -> Trace {
        let mut trace = crate::simulation::engine::tests::trace();
        trace.members[0].scheduler_settings[0].capacity.reserved = Cores::from_millis(500);
        trace.members[0].scheduler_settings[0].capacity.capacity = Cores::from_millis(1500);
        trace.members[0].load[0].capacity = Cores::from_millis(1500);
        trace
    }
    fn values(input: &[&str]) -> Vec<String> {
        input.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn candidate_settings_preserve_production_parsing_and_first_label_match() {
        let trace = trace();
        let settings = Settings::parse(
            &trace,
            &values(&[
                "keep_local_margin=0.2",
                "member.a.reserved_cores=0.75",
                "label.wo*.size=0.5",
                "label.work.size=0.25",
                "hold_limit=1250ms",
            ]),
        )
        .unwrap();
        assert_eq!(
            settings
                .parameters(&Parameters::default())
                .unwrap()
                .placement
                .keep_local_margin,
            0.2
        );
        let capacity = settings
            .capacity("a", &trace.members[0].scheduler_settings[0].capacity)
            .unwrap();
        assert_eq!(capacity.reserved, Cores::from_millis(750));
        assert_eq!(capacity.capacity, Cores::from_millis(1250));
        let mut request = trace.requests[0].clone();
        assert_eq!(settings.size(&request), Some(Cores::from_millis(500)));
        assert_eq!(
            settings.urgency(&request),
            Urgency::Interactive {
                hold_limit_ms: Some(1250)
            }
        );
        request.urgency = Urgency::Deferrable {
            deadline: Timestamp::from_millis(1000),
            at_deadline: AtDeadline::Skip,
        };
        assert_eq!(settings.urgency(&request), request.urgency);
        request.fact.label = None;
        assert_eq!(settings.size(&request), None);
    }

    #[test]
    fn invalid_candidates_cannot_erase_reserve_or_use_unknown_or_ambiguous_keys() {
        let mut trace = trace();
        for input in [
            vec!["member.a.reserved_cores=0.25"],
            vec!["member.unknown.reserved_cores=1"],
            vec!["member.a.reserved_cores=1", "member.a.reserved_cores=1"],
            vec!["label.work.size=0"],
            vec!["label..size=1"],
            vec!["label.a\nb.size=1"],
            vec!["label.work.size=1", "label.work.size=2"],
            vec!["hold_limit=25h"],
            vec!["hold_limit=1ns"],
            vec!["hold_limit=0s", "hold_limit=1s"],
            vec!["saturated_utilization=1.1"],
            vec!["unknown=1"],
        ] {
            assert_eq!(
                Settings::parse(&trace, &values(&input)).unwrap_err().code,
                "invalid_request",
                "{input:?}"
            );
        }
        trace.members[0].scheduler_settings.clear();
        assert!(Settings::parse(&trace, &values(&["member.a.reserved_cores=1"])).is_err());
    }

    #[test]
    fn reserve_floor_is_checked_against_each_applied_baseline_and_sizes_are_literal_globs() {
        let trace = trace();
        let settings = Settings::parse(
            &trace,
            &values(&["member.a.reserved_cores=0.75", "label.a=b ?.size=0.25"]),
        )
        .unwrap();
        let mut later = trace.members[0].scheduler_settings[0].capacity;
        later.reserved = Cores::ONE;
        later.capacity = Cores::ONE;
        assert!(settings.capacity("a", &later).is_err());
        let mut request = trace.requests[0].clone();
        request.fact.label = Some("a=b X".into());
        assert_eq!(settings.size(&request), Some(Cores::from_millis(250)));
        assert_eq!(
            Settings::parse(&trace, &[])
                .unwrap()
                .capacity("a", &later)
                .unwrap(),
            later
        );
    }
}
