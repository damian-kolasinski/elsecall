//! Bounded versioned trace-file decoding, shared by simulation and rehearsal.
use crate::{
    failure::{Failure, Result},
    workload::trace::Trace,
};
use std::io::Read;
pub fn read(path: &std::path::Path) -> Result<Trace> {
    let file = std::fs::File::open(path).map_err(|e| Failure::io("Read workload trace", e))?;
    let mut bytes = Vec::new();
    file.take(64 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| Failure::io("Read workload trace", e))?;
    if bytes.len() > 64 * 1024 * 1024 {
        return Err(crate::simulation::engine_invalid("Trace exceeds 64 MiB."));
    }
    #[derive(serde::Deserialize)]
    struct Header {
        trace_version: u32,
    }
    let header: Header = serde_json::from_slice(&bytes)
        .map_err(|_| crate::simulation::engine_invalid("Malformed workload trace JSON."))?;
    if header.trace_version != crate::workload::trace::VERSION {
        return Err(Failure::new(
            "unsupported_trace_version",
            "Unsupported workload trace version.",
            "Use the Elsecall version that supports this trace's major version.",
        ));
    }
    let trace: Trace = serde_json::from_slice(&bytes)
        .map_err(|_| crate::simulation::engine_invalid("Malformed workload trace JSON."))?;
    Ok(trace)
}
