//! UTC day selection shared by offline scheduling tools.
use elsecall_failure::Result;
use elsecall_scheduling_model::timestamp::Timestamp;
pub fn day(value: Option<String>) -> Result<Option<Timestamp>> {
    value
        .map(|d| {
            if d.len() != 10 {
                return Err(crate::simulation::engine_invalid(
                    "--day must be YYYY-MM-DD.",
                ));
            }
            Timestamp::parse_rfc3339(&format!("{d}T00:00:00Z"))
                .ok_or_else(|| crate::simulation::engine_invalid("Invalid UTC --day."))
        })
        .transpose()
}
