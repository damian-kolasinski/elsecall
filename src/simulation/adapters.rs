//! Boundary adapter interface for the virtual scheduling fleet. Implementations
//! own machine observation, processes, identity, communication and fleet state
//! separately; consumers import this interface only.
pub(super) use super::{
    channel::{Channel, Registry},
    config_sync::ConfigurationSync,
    observation::{Observation, Speed},
    processes::Processes,
    request_ids::Ids,
    workspace::workspace,
    world::{CopyState, Occupancy, References, World},
};
