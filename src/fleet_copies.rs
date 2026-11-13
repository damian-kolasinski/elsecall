//! What the fleet reports of this member's copies: the adapter of
//! `fleet::CopiesReport` over housekeeping.
use crate::{
    failure::Result,
    fleet::{CopiesDisk, CopiesReport, CopiesUsage},
    housekeeping::pass::Housekeeper,
};
use std::sync::Arc;

pub struct FleetCopies(pub Arc<Housekeeper>);

impl CopiesReport for FleetCopies {
    fn usage(&self) -> Result<CopiesUsage> {
        let summary = self.0.summary()?;
        Ok(CopiesUsage {
            copies: summary.copies,
            copies_bytes: summary.copies_bytes,
            unmeasured_copies: summary.unmeasured_copies,
            stores: summary.stores,
            stores_bytes: summary.stores_bytes,
            disk: summary.disk.map(|disk| CopiesDisk {
                available_bytes: disk.available_bytes,
                threshold_bytes: disk.threshold_bytes,
                pressure: disk.pressure,
            }),
            last_pass_at: summary.last_pass_at,
        })
    }
}
