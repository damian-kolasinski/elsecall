//! The fleets the window's scenes show: a fixed master, members and their load.
use crate::fleet_source::{
    FleetSnapshot, Gate, LoadAverages, MemberRow, MemoryUse, Reachability, Release, Role, Standing,
};
use elsecall_peers::roster::Readiness;
use std::num::NonZeroU32;

pub(super) fn solo_snapshot() -> FleetSnapshot {
    FleetSnapshot {
        enrolled: false,
        members: vec![member(
            "mac-mini",
            Role::Master,
            Reachability::Reachable,
            Gate::Open,
            Some((0.18, 8)),
            Some(gigabytes(18, 64)),
            Some(LoadAverages {
                one: 1.4,
                five: 1.6,
                fifteen: 1.5,
            }),
        )],
        standing: Standing::Rostered,
        attention: Vec::new(),
    }
}

/// A fresh Mac whose Tailscale is not running: no presence, so no roster.
pub(super) fn no_tailnet_snapshot() -> FleetSnapshot {
    FleetSnapshot {
        enrolled: false,
        members: Vec::new(),
        standing: Standing::OffTailnet {
            hint: "Make sure Tailscale is running and logged in on this machine; the service retries every 30 seconds.".into(),
        },
        attention: Vec::new(),
    }
}

pub(crate) fn members_snapshot() -> FleetSnapshot {
    FleetSnapshot {
        enrolled: true,
        members: vec![
            member(
                "mac-mini",
                Role::Master,
                Reachability::Reachable,
                Gate::Open,
                Some((0.42, 12)),
                Some(gigabytes(36, 64)),
                Some(LoadAverages {
                    one: 4.8,
                    five: 4.5,
                    fifteen: 4.2,
                }),
            ),
            member(
                "macbook",
                Role::Member,
                Reachability::Unreachable,
                Gate::Open,
                None,
                None,
                None,
            ),
            member(
                "macbook-pro",
                Role::Member,
                Reachability::Reachable,
                Gate::Blocked {
                    leading_process: Some("Xcode".into()),
                },
                Some((0.91, 8)),
                Some(gigabytes(52, 64)),
                Some(LoadAverages {
                    one: 12.0,
                    five: 9.4,
                    fifteen: 7.2,
                }),
            ),
        ],
        standing: Standing::Rostered,
        attention: Vec::new(),
    }
}

pub(super) fn member(
    name: &str,
    role: Role,
    reach: Reachability,
    gate: Gate,
    cpu: Option<(f64, u32)>,
    memory: Option<MemoryUse>,
    load: Option<LoadAverages>,
) -> MemberRow {
    let release = match reach {
        Reachability::Reachable => Release::Matching {
            version: "0.2.2".into(),
        },
        Reachability::Unreachable => Release::Unknown { version: None },
    };
    MemberRow {
        name: name.into(),
        // The scenes' master is this Mac.
        this_member: role == Role::Master,
        role,
        reach,
        readiness: Readiness::Ready,
        gate,
        contributors: Default::default(),
        cpu: cpu.map(|(utilization, _)| utilization),
        logical_cpus: cpu.and_then(|(_, count)| NonZeroU32::new(count)),
        memory,
        load,
        release,
        provisioning: None,
        pin_exclusions: None,
    }
}

pub(super) fn gigabytes(used: u64, total: u64) -> MemoryUse {
    MemoryUse {
        used_bytes: used << 30,
        total_bytes: total << 30,
    }
}
