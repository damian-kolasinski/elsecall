//! The operations currently holding capacity on one member.
use elsecall_contract::scheduling_model::{capacity::Cores, request::Kind, run_id::RunId};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ActiveGrant {
    pub run_id: RunId,
    pub kind: Kind,
    pub label: Option<String>,
    pub size: Cores,
}

/// Keep a frequently polled peer status bounded even with many fractional grants.
pub const REPORTED_GRANTS: usize = 32;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GrantSnapshot {
    pub total: usize,
    pub assigned: Cores,
    #[serde(deserialize_with = "read_operations")]
    pub operations: Vec<ActiveGrant>,
}

fn read_operations<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<ActiveGrant>, D::Error> {
    let values = Vec::<serde_json::Value>::deserialize(d)?;
    if values.len() > REPORTED_GRANTS {
        return Err(serde::de::Error::custom("too many reported grants"));
    }
    Ok(values
        .into_iter()
        .filter_map(|value| serde_json::from_value(value).ok())
        .collect())
}

/// An unreadable optional report must not turn a reachable member into an offline one.
pub fn optional<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<GrantSnapshot>, D::Error> {
    let value = Option::<serde_json::Value>::deserialize(d)?;
    Ok(value
        .and_then(|v| serde_json::from_value::<GrantSnapshot>(v).ok())
        .filter(|s| s.operations.len() <= s.total))
}

pub trait ActiveGrants: Send + Sync {
    fn active_grants(&self) -> GrantSnapshot;
}
