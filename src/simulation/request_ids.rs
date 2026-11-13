//! Deterministic request identities supplied to the admission port.
use crate::scheduling::run_id::{RunId, RunIds};
use std::{collections::VecDeque, sync::Mutex};

#[derive(Default)]
pub(super) struct Ids(pub Mutex<VecDeque<RunId>>);
impl RunIds for Ids {
    fn next(&self) -> RunId {
        self.0
            .lock()
            .unwrap()
            .pop_front()
            .expect("each submitted request supplies its recorded ID")
    }
}
