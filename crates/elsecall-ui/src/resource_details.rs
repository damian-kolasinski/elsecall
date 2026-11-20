//! The evidence available for each resource and owner in the fleet window.
use elsecall_contract::scheduling_model::{request::Kind, timestamp::Timestamp};
use elsecall_fleet::resource_report::{Blocked, Concern, GrantSnapshot, Reason, Warning};

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Contributors {
    pub(crate) grants: Option<GrantSnapshot>,
    pub(crate) pressure: Pressure,
    pub(crate) warnings: Vec<Warning>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) enum Pressure {
    #[default]
    None,
    Sample(Box<Blocked>),
    Unreadable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Metric {
    Cpu,
    Memory,
}

impl Metric {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Cpu => "CPU",
            Self::Memory => "memory",
        }
    }
}

pub(crate) struct Reading {
    pub(crate) label: String,
    pub(crate) quantity: Quantity,
    pub(crate) detail: String,
}

pub(crate) struct Quantity {
    pub(crate) value: String,
    pub(crate) amount: f64,
}

pub(crate) struct Details {
    pub(crate) caption: String,
    pub(crate) readings: Vec<Reading>,
}

// CPU is measured in logical cores and memory in GiB throughout the breakdown.
pub(crate) fn gib(bytes: u64) -> String {
    format!("{:.2} GiB", bytes as f64 / (1u64 << 30) as f64)
}

fn process_label(name: &str) -> &str {
    match name {
        "Simulator" => "Device simulator",
        "fseventsd" => "File system events",
        "mds" | "mds_stores" | "mdworker" | "mdworker_shared" => "Spotlight indexing",
        "WindowServer" => "Window compositor",
        other => other,
    }
}

fn sample_time(sample: Timestamp, refreshed: Timestamp) -> String {
    let utc = sample.to_string();
    let clock = &utc[11..16];
    if sample > refreshed {
        return format!("{clock} UTC · ahead of refresh");
    }
    let seconds = refreshed.since(sample).as_secs();
    let age = match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m", seconds / 60),
        3600..86400 => format!("{}h", seconds / 3600),
        _ => format!("{}d", seconds / 86400),
    };
    format!("{clock} UTC · {age} before refresh")
}

impl Contributors {
    pub(crate) fn managed(&self) -> Details {
        let Some(grants) = &self.grants else {
            return Details {
                caption: "This member did not report active grants.".into(),
                readings: vec![],
            };
        };
        let caption = if grants.total == 0 {
            "No active grants.".into()
        } else {
            format!(
                "{} weighted cores assigned across {} active grants. Live CPU and memory use per operation is unavailable.",
                grants.assigned, grants.total
            )
        };
        Details {
            caption: if grants.total > grants.operations.len() {
                format!(
                    "{caption} Details available for {} of {}.",
                    grants.operations.len(),
                    grants.total
                )
            } else {
                caption
            },
            readings: grants
                .operations
                .iter()
                .map(|grant| {
                    let kind = match grant.kind {
                        Kind::Run => "Run",
                        Kind::Lease => "Lease",
                    };
                    Reading {
                        label: grant.label.as_deref().unwrap_or(kind).into(),
                        quantity: Quantity {
                            value: format!("{} weighted cores", grant.size),
                            amount: grant.size.as_f64(),
                        },
                        detail: format!("{kind} · {}", grant.run_id),
                    }
                })
                .collect(),
        }
    }

    pub(crate) fn outside(&self, metric: Metric, at: Timestamp) -> Details {
        let mut readings = vec![];
        if let Pressure::Sample(blocked) = &self.pressure
            && matches!(
                (metric, &blocked.reason),
                (Metric::Memory, Reason::MemoryPressure { .. })
                    | (Metric::Cpu, Reason::OutsideSaturation { .. })
            )
        {
            for process in &blocked.outside {
                let value = match metric {
                    Metric::Cpu => format!("{:.2} cores", process.cpu_cores),
                    Metric::Memory => match process.memory.footprint_bytes {
                        Some(bytes) => format!("{} footprint", gib(bytes)),
                        None => format!("{} resident", gib(process.memory.resident_bytes)),
                    },
                };
                readings.push(Reading {
                    label: process_label(&process.name).into(),
                    quantity: Quantity {
                        value,
                        amount: match metric {
                            Metric::Cpu => process.cpu_cores,
                            Metric::Memory => process
                                .memory
                                .footprint_bytes
                                .unwrap_or(process.memory.resident_bytes)
                                as f64,
                        },
                    },
                    detail: format!(
                        "{} · PID {} · {}\nSampled · {}",
                        process.name,
                        process.pid,
                        process.owner.spoken(),
                        sample_time(blocked.attributed_at, at)
                    ),
                });
            }
        }
        for w in &self.warnings {
            let value = match (metric, w.concern) {
                (Metric::Cpu, Concern::Spinning { cpu_cores }) => {
                    format!("{cpu_cores:.2} cores · 6h mean")
                }
                (
                    Metric::Memory,
                    Concern::Growing { memory_bytes, .. } | Concern::Oversized { memory_bytes, .. },
                ) => gib(memory_bytes),
                _ => continue,
            };
            readings.push(Reading {
                label: process_label(&w.process).into(),
                quantity: Quantity {
                    value,
                    amount: match w.concern {
                        Concern::Spinning { cpu_cores } => cpu_cores,
                        Concern::Growing { memory_bytes, .. }
                        | Concern::Oversized { memory_bytes, .. } => memory_bytes as f64,
                    },
                },
                detail: format!(
                    "{} · PID {} · {}\n{} warning · {}",
                    w.process,
                    w.pid,
                    w.owner.spoken(),
                    w.concern.kind(),
                    sample_time(w.observed_at, at)
                ),
            });
        }
        let caption = if self.pressure == Pressure::Unreadable {
            "This build could not read the member’s pressure sample.".into()
        } else if readings.is_empty() {
            format!("No outside {} contributors reported.", metric.name())
        } else {
            String::new()
        };
        Details { caption, readings }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::socket_source::snapshot_from_value;
    use serde_json::{Value, json};

    #[test]
    fn absent_empty_and_unreadable_grants_are_distinct_from_measured_usage() {
        let mut fleet: Value =
            serde_json::from_str(include_str!("../tests/fixtures/resource-load.json")).unwrap();
        for grants in [Value::Null, json!({"assigned":"future"})] {
            fleet["members"][0]["status"]["active_grants"] = grants;
            let fleet = snapshot_from_value(&fleet).unwrap();
            assert!(
                fleet.members[0]
                    .contributors
                    .managed()
                    .caption
                    .contains("did not report")
            );
            assert_eq!(
                fleet.members[1].contributors.managed().caption,
                "No active grants."
            );
        }
    }

    #[test]
    fn shows_readable_grants_and_only_the_processes_leading_on_the_selected_resource() {
        let mut fleet: Value =
            serde_json::from_str(include_str!("../tests/fixtures/resource-load.json")).unwrap();
        let row = &mut fleet["members"][0];
        row["status"]["active_grants"]["operations"][0]["kind"] = "future".into();
        row["outside_warnings"] = json!([]);
        let at = Timestamp::parse_rfc3339("2026-11-01T14:00:00Z").unwrap();
        for (reason, relevant, other) in [
            ("memory_pressure", Metric::Memory, Metric::Cpu),
            ("outside_saturation", Metric::Cpu, Metric::Memory),
        ] {
            fleet["members"][0]["blocked"]["reason"] = reason.into();
            fleet["members"][0]["blocked"]["outside_utilization"] = 0.95.into();
            let snapshot = snapshot_from_value(&fleet).unwrap();
            let evidence = &snapshot.members[0].contributors;
            let managed = evidence.managed();
            assert_eq!(managed.readings.len(), 1);
            assert_eq!(managed.readings[0].quantity.value, "2.5 weighted cores");
            assert!(managed.caption.contains("1 of 2"));
            assert_eq!(evidence.outside(other, at).readings.len(), 0);
            let rows = evidence.outside(relevant, at).readings;
            assert_eq!(rows.len(), 2);
            assert!(rows[0].detail.contains("22s before refresh"));
        }
        fleet["members"][0]["blocked"]["reason"] = "future".into();
        let snapshot = snapshot_from_value(&fleet).unwrap();
        assert!(
            snapshot.members[0]
                .contributors
                .outside(Metric::Cpu, at)
                .caption
                .contains("could not read")
        );
    }

    #[test]
    fn separates_sustained_cpu_and_memory_warnings_with_their_sample_times() {
        let mut fleet: Value =
            serde_json::from_str(include_str!("../tests/fixtures/resource-load.json")).unwrap();
        let base = json!({"process":"mds","pid":12,"started":"2026-11-01T00:00:00Z","owner":"root",
            "since":"2026-11-01T06:00:00Z","observed_at":"2026-11-01T12:30:00Z"});
        let mut cpu = base.clone();
        cpu["concern"] = "spinning".into();
        cpu["cpu_cores"] = 1.2.into();
        let mut memory = base;
        memory["concern"] = "oversized".into();
        memory["memory_bytes"] = (8u64 << 30).into();
        memory["share_of_memory"] = 0.25.into();
        fleet["members"][1]["outside_warnings"] = json!([cpu,memory,{"concern":"future"}]);
        let fleet = snapshot_from_value(&fleet).unwrap();
        let member = &fleet.members[1].contributors;
        let cpu = member.outside(Metric::Cpu, Timestamp::now()).readings;
        assert_eq!(cpu.len(), 1);
        assert_eq!(cpu[0].label, "Spotlight indexing");
        assert_eq!(cpu[0].quantity.value, "1.20 cores · 6h mean");
        assert!(cpu[0].detail.contains("12:30 UTC"));
        let memory = member.outside(Metric::Memory, Timestamp::now()).readings;
        assert_eq!(memory.len(), 1);
        assert_eq!(memory[0].quantity.value, "8.00 GiB");
    }
}
