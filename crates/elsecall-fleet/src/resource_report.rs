//! Resource readings included in a member's fleet status.
pub use elsecall_scheduling::active_grants::{GrantSnapshot, optional as read_grants};
pub use elsecall_scheduling::outside_pressure::{
    Blocked, Reason, Warning, blocked::lenient_list as read_warnings, warning::Concern,
};
