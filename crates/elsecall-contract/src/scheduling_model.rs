//! The scheduler's vocabulary: requests and their settings, times, capacity, the
//! load gate, run ids, speed and workload metadata.
pub mod capacity;
pub mod gate;
pub mod request;
pub mod run_id;
pub mod settings;
pub mod speed;
pub mod timestamp;
pub mod workload_metadata;
