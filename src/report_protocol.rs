//! What members exchange for reports: the requests and answers of
//! `POST /peer/v1/report/facts` and `POST /peer/v1/report/load`. The member that
//! reports asks with `peer_history`; the member asked answers in `report_api`.
use crate::reporting::{facts::Fact, leftovers::LeftoverFact};
use serde::{Deserialize, Serialize};

/// Records read for one page of facts: their facts stay well within a member
/// response.
pub const PAGE: u32 = 250;

/// Where a page of facts continues: below this request time and run ID.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cursor {
    pub requested_at_ms: u64,
    pub run_id: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FactsRequest {
    pub since_ms: u64,
    pub until_ms: u64,
    #[serde(default)]
    pub before: Option<Cursor>,
}

/// At most [`PAGE`] facts, newest first, and where the next page starts.
#[derive(Debug, Serialize, Deserialize)]
pub struct FactsPage {
    pub facts: Vec<Fact>,
    /// The leftovers of placed runs the member hosted, from the same records;
    /// absent from members on earlier versions.
    #[serde(default)]
    pub leftovers: Vec<LeftoverFact>,
    pub next: Option<Cursor>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoadRequest {
    pub since_ms: u64,
    pub until_ms: u64,
    pub width_ms: u64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeerIoRequest {
    pub since_ms: u64,
    pub until_ms: u64,
}

/// `POST /peer/v1/report/outside`: the member's outside-pressure history of the
/// period, answered with a [`crate::reporting::outside::OutsideHistory`].
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutsideRequest {
    pub since_ms: u64,
    pub until_ms: u64,
}
