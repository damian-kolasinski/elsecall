//! Fleet service settings: a complete document, with defaults and member overlays.
//! Bootstrap identity, trust, transport and installation never enter this schema.
use crate::{
    configuration::{self, ServiceConfiguration},
    failure::{Failure, Result},
    roster::MemberName,
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path};

pub const SOURCE: &str = "fleet-config.toml";
pub const LIMIT: usize = 64 * 1024;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rehearsal: Option<Temporary>,
    #[serde(default)]
    defaults: toml::Table,
    #[serde(default)]
    members: BTreeMap<MemberName, toml::Table>,
}

/// A scheduling-only overlay carried by the sealed service-settings channel.
/// Baseline settings remain in the same document and become effective on expiry,
/// including while the master is unreachable or a controller has been killed.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Temporary {
    pub session: String,
    pub until: crate::scheduling::timestamp::Timestamp,
    pub members: BTreeMap<MemberName, Projection>,
    /// Length of the original document prefix. Older leases lacked this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) baseline_len: Option<usize>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Projection {
    pub reserved: crate::scheduling::capacity::Cores,
    pub parameters: crate::scheduling::parameters::Parameters,
}

/// Holds validated document bytes and parsed settings, never included in diagnostics.
pub struct Configuration {
    bytes: Vec<u8>,
    rehearsal: Option<Temporary>,
    defaults: toml::Table,
    members: BTreeMap<MemberName, toml::Table>,
}

pub fn invalid(why: impl Into<String>) -> Failure {
    Failure::new(
        "invalid_service_configuration",
        why,
        "Fix fleet-config.toml on the master, run elsecall sync service to review it, then sync push. Values are omitted from diagnostics.",
    )
}

fn settings(table: &toml::Table) -> Result<ServiceConfiguration> {
    configuration::parse(
        &toml::to_string(table).expect("TOML table serializes"),
        Path::new(SOURCE),
    )
    .map_err(|e| invalid(e.message))
}

fn overlay(base: &mut toml::Table, extra: &toml::Table) {
    for (key, value) in extra {
        if let (Some(toml::Value::Table(base)), toml::Value::Table(extra)) =
            (base.get_mut(key), value)
        {
            overlay(base, extra);
        } else {
            base.insert(key.clone(), value.clone());
        }
    }
}

impl Configuration {
    /// Validates every member overlay, including members currently offline.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > LIMIT {
            return Err(invalid("Fleet service configuration exceeds 64 KiB."));
        }
        let text = std::str::from_utf8(bytes)
            .map_err(|_| invalid("Fleet service configuration must be UTF-8 TOML."))?;
        let document: Document = toml::from_str(text).map_err(|_| invalid("Expected only [defaults] and [members.<roster-name>] settings; invalid TOML, name or key."))?;
        if document.members.len() > 64 {
            return Err(invalid("Too many member sections."));
        }
        for section in ["capacity", "workspace"] {
            if document.defaults.contains_key(section) {
                return Err(invalid(format!(
                    "[{section}] must be set per member, never in defaults."
                )));
            }
        }
        if document
            .defaults
            .get("placed_runs")
            .and_then(toml::Value::as_table)
            .is_some_and(|t| t.contains_key("path"))
        {
            return Err(invalid("[placed_runs] path must be set per member."));
        }
        settings(&document.defaults)?;
        for member in document.members.values() {
            // Validate keys/types separately, but numeric cross-field constraints
            // against the merged settings (e.g. estimate window and minimums).
            let mut merged = document.defaults.clone();
            overlay(&mut merged, member);
            settings(&merged)?;
        }
        if let Some(temporary) = &document.rehearsal {
            if let Some(len) = temporary.baseline_len {
                let baseline: Document = text
                    .get(..len)
                    .and_then(|prefix| toml::from_str(prefix).ok())
                    .ok_or_else(|| invalid("Invalid rehearsal baseline prefix."))?;
                if baseline.rehearsal.is_some()
                    || baseline.defaults != document.defaults
                    || baseline.members != document.members
                {
                    return Err(invalid("Rehearsal baseline does not match its document."));
                }
            }
            if !crate::rehearsal::protocol::valid_session(&temporary.session)
                || temporary.members.len() > 64
                || temporary.members.is_empty()
                || temporary.until
                    > crate::scheduling::timestamp::Timestamp::now()
                        .after(std::time::Duration::from_secs(3630))
            {
                return Err(invalid("Invalid bounded rehearsal settings lease."));
            }
            for (name, projection) in &temporary.members {
                projection.parameters.validate().map_err(invalid)?;
                let mut base = document.defaults.clone();
                if let Some(extra) = document.members.get(name) {
                    overlay(&mut base, extra);
                }
                if projection.reserved < settings(&base)?.reserved {
                    return Err(invalid("A rehearsal cannot lower an approved reserve."));
                }
            }
        }
        Ok(Self {
            bytes: bytes.to_vec(),
            rehearsal: document.rehearsal,
            defaults: document.defaults,
            members: document.members,
        })
    }

    pub fn member_names(&self) -> impl Iterator<Item = &MemberName> {
        self.members.keys()
    }

    pub fn resolve(&self, name: &MemberName) -> Result<ServiceConfiguration> {
        let mut merged = self.defaults.clone();
        if let Some(member) = self.members.get(name) {
            overlay(&mut merged, member);
        }
        let mut resolved = settings(&merged)?;
        if let Some(temporary) = &self.rehearsal
            && temporary.until > crate::scheduling::timestamp::Timestamp::now()
            && let Some(projection) = temporary.members.get(name)
        {
            resolved.parameters = projection.parameters.clone();
            resolved.reserved = projection.reserved;
        }
        Ok(resolved)
    }
    pub fn temporary(&self) -> Option<&Temporary> {
        self.rehearsal.as_ref()
    }
    pub fn baseline_bytes(&self) -> Vec<u8> {
        match &self.rehearsal {
            None => return self.bytes.clone(),
            Some(temporary) if temporary.baseline_len.is_some() => {
                return self.bytes[..temporary.baseline_len.expect("validated prefix")].to_vec();
            }
            // Compatibility with leases issued before exact-byte restoration.
            Some(_) => (),
        }
        toml::to_string(&Document {
            defaults: self.defaults.clone(),
            members: self.members.clone(),
            rehearsal: None,
        })
        .expect("configuration serializes")
        .into_bytes()
    }
    pub fn with_temporary(&self, mut temporary: Temporary) -> Result<Vec<u8>> {
        #[derive(Serialize)]
        struct Lease {
            rehearsal: Temporary,
        }
        let mut bytes = self.baseline_bytes();
        temporary.baseline_len = Some(bytes.len());
        bytes.push(b'\n');
        bytes.extend(
            toml::to_string(&Lease {
                rehearsal: temporary,
            })
            .expect("configuration serializes")
            .as_bytes(),
        );
        Self::parse(&bytes)?;
        Ok(bytes)
    }
}

pub fn read(path: &Path) -> Result<Vec<u8>> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .and_then(|f| f.take((LIMIT + 1) as u64).read_to_end(&mut bytes))
        .map_err(|e| Failure::io("Read fleet service configuration", e))?;
    Configuration::parse(&bytes)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn lease_round_trip_preserves_source_bytes_and_validates_its_prefix() {
        for original in [
            "# operator formatting\r\n[defaults.placement]\r\nkeep_local_margin = 0.10 # approved",
            "",
            "# comments only\n",
        ] {
            let config = Configuration::parse(original.as_bytes()).unwrap();
            assert_eq!(config.baseline_bytes(), original.as_bytes());
            let baseline = config.resolve(&"alpha".parse().unwrap()).unwrap();
            let bytes = config
                .with_temporary(Temporary {
                    session: "0123456789abcdef01234567".into(),
                    until: crate::scheduling::timestamp::Timestamp::now()
                        .after(std::time::Duration::from_secs(60)),
                    members: BTreeMap::from([(
                        "alpha".parse().unwrap(),
                        Projection {
                            reserved: baseline.reserved,
                            parameters: baseline.parameters,
                        },
                    )]),
                    baseline_len: None,
                })
                .unwrap();
            let lease = Configuration::parse(&bytes).unwrap();
            assert_eq!(lease.baseline_bytes(), original.as_bytes());
            let text = String::from_utf8(bytes).unwrap();
            for len in [text.len(), usize::MAX] {
                let invalid = text.replace(
                    &format!("baseline_len = {}", original.len()),
                    &format!("baseline_len = {len}"),
                );
                assert!(
                    Configuration::parse(invalid.as_bytes()).is_err(),
                    "{invalid}"
                );
            }
            if original.contains("keep_local_margin") {
                let invalid = text.replace(
                    &format!("baseline_len = {}", original.len()),
                    "baseline_len = 0",
                );
                assert!(Configuration::parse(invalid.as_bytes()).is_err());
            }
        }
    }

    #[test]
    fn comparison_names_only_changed_sections_and_binds_the_baseline() {
        let before = ServiceConfiguration::default();
        let mut after = before.clone();
        after.parameters.admission.saturated_utilization = 0.8;
        after.placed_path = Some("/VALUE-SENTINEL/bin".into());
        let key = crate::config_sync::fingerprint::SyncKey::generate().unwrap();
        let plan = compare(&before, &after, &key);
        assert_eq!(plan.changed_sections, ["placed_runs", "admission"]);
        assert!(
            !serde_json::to_string(&plan)
                .unwrap()
                .contains("VALUE-SENTINEL")
        );
        assert!(compare(&before, &before, &key).changed_sections.is_empty());
        assert_ne!(plan.fingerprint, compare(&after, &after, &key).fingerprint);
    }

    #[test]
    fn defaults_merge_by_key_and_paths_require_member_names() {
        let config = Configuration::parse(
            br#"
[defaults.placement]
keep_local_margin = 0.3
[defaults.admission]
saturated_utilization = 0.8
[members.mini.capacity]
reserved_cores = 3
[members.devbox.workspace]
root = "/work"
object_sources = ["/repo"]
[members.devbox.admission]
memory_pressure = "critical"
"#,
        )
        .unwrap();
        let mini = config.resolve(&"mini".parse().unwrap()).unwrap();
        assert_eq!(mini.reserved.to_string(), "3");
        assert_eq!(mini.parameters.placement.keep_local_margin, 0.3);
        let devbox = config.resolve(&"devbox".parse().unwrap()).unwrap();
        assert_eq!(devbox.workspace_root.unwrap(), Path::new("/work"));
        assert_eq!(devbox.parameters.admission.saturated_utilization, 0.8);
        for text in [
            "[defaults.capacity]\nreserved_cores=3",
            "[defaults.workspace]\nroot='/a'",
            "[defaults.placed_runs]\npath='/bin'",
            "[identity]\nkey='x'",
            "[members.mini.workspace]\nroot='relative'",
            "[defaults.placement]\nkeep_local_margin=2",
            "[members.mini.secret]\nvalue='SENTINEL'",
        ] {
            let e = Configuration::parse(text.as_bytes()).err().unwrap();
            assert!(!e.message.contains("SENTINEL"));
        }
    }
}

/// Diagnostics contain identifiers and failure reasons, never setting values.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct Summary {
    pub current: Option<bool>,
    pub generation: Option<u64>,
    pub active_generation: Option<u64>,
    pub fingerprint: Option<String>,
    /// Active parsed scheduling settings and capacity, independent of generation.
    #[serde(default)]
    pub effective_fingerprint: Option<String>,
    pub pending: bool,
    pub rejection: Option<Failure>,
}

/// Managed members start from their verified replica. A retained local file,
/// including later edits to it, cannot override or break an accepted generation.
pub fn load(
    paths: &crate::paths::Paths,
    member: Option<&MemberName>,
) -> Result<ServiceConfiguration> {
    use crate::config_sync::{
        state::State,
        store::{FileStore, ReplicaStore},
    };
    let state = match State::load(&paths.data.join("config-sync/state.json")) {
        Ok(state) => state,
        Err(_) => return configuration::load(&paths.configuration()),
    };
    let member = member.or(state.service_member.as_ref());
    if let Some(manifest) = state.manifest
        && let Some(node) = manifest.service
    {
        let store = FileStore::open(&paths.data.join("config-sync/objects"))?;
        let bytes = store.read(&manifest.key, &node)?;
        let configuration = Configuration::parse(&bytes)?;
        let name = member.ok_or_else(|| {
            invalid("Cannot select managed settings until member identity is available.")
        })?;
        return configuration.resolve(name);
    }
    configuration::load(&paths.configuration())
}

pub fn pending() -> Failure {
    Failure::new(
        "service_configuration_pending",
        "This member is finishing work before applying its service configuration.",
        "Retry after its automatic restart; running and queued work is preserved.",
    )
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct MemberPlan {
    pub fingerprint: String,
    pub changed_sections: Vec<String>,
}

pub fn compare(
    before: &ServiceConfiguration,
    after: &ServiceConfiguration,
    key: &crate::config_sync::fingerprint::SyncKey,
) -> MemberPlan {
    let changes = [
        ("logs", before.logs != after.logs),
        ("retention", before.sync_backups != after.sync_backups),
        ("capacity", before.reserved != after.reserved),
        (
            "workspace",
            before.location != after.location
                || before.workspace_root != after.workspace_root
                || before.object_sources != after.object_sources,
        ),
        (
            "placed_runs",
            before.placed_path != after.placed_path || before.leftovers_cap != after.leftovers_cap,
        ),
        ("housekeeping", before.housekeeping != after.housekeeping),
        (
            "placement",
            before.parameters.placement != after.parameters.placement,
        ),
        (
            "admission",
            before.parameters.admission != after.parameters.admission,
        ),
        (
            "estimates",
            before.parameters.estimates != after.parameters.estimates,
        ),
        ("sync", before.sync_paths != after.sync_paths),
    ];
    MemberPlan {
        // The representation never leaves memory; only the keyed digest is shown.
        fingerprint: key
            .digest_labelled("service-baseline", format!("{before:?}").as_bytes())
            .short(),
        changed_sections: changes
            .into_iter()
            .filter(|(_, changed)| *changed)
            .map(|(name, _)| name.to_owned())
            .collect(),
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RehearsalSettings {
    pub session: String,
    pub until: Option<crate::scheduling::timestamp::Timestamp>,
    #[serde(default)]
    pub settings: std::collections::BTreeMap<String, String>,
}
