//! This member's history for reports: its request records and load history,
//! read in pages so neither a report nor a member's answer reads everything at
//! once.
use crate::{
    failure::{Failure, Result},
    load_log::{LoadReport, buckets::Bucket},
    report_protocol::PAGE,
    reporting::{
        facts::Fact,
        gathering::{Facts, MAXIMUM_FACTS, MemberHistory},
        leftovers::LeftoverFact,
        outside::OutsideHistory,
        window::Window,
    },
    scheduling::{service::Scheduler, timestamp::Timestamp},
};
use futures_util::future::BoxFuture;
use std::{sync::Arc, time::Duration};

/// Where the next page starts: below this request time and run ID.
pub type Cursor = (Timestamp, String);

/// One page of facts, newest first, and where the next one starts.
pub struct Page {
    pub facts: Vec<Fact>,
    /// The leftovers of placed runs this member hosted, from the same records.
    pub leftovers: Vec<LeftoverFact>,
    pub next: Option<Cursor>,
}

#[derive(Clone)]
pub struct OwnHistory {
    pub scheduler: Arc<Scheduler>,
    pub load: Arc<dyn LoadReport>,
    /// Outside pressure's detector, which keeps blocked episodes; `None` when
    /// its history could not be opened.
    pub outside: Option<Arc<crate::outside_pressure::Detector>>,
    /// Early warnings about outside processes; `None` when their history could
    /// not be opened.
    pub warnings: Option<Arc<dyn crate::outside_pressure::WarningReport>>,
}

impl OwnHistory {
    /// The page of `window`'s requests below `before`. The member side of a
    /// placed run is left to its calling member.
    pub fn page(&self, window: Window, before: Option<&Cursor>) -> Result<Page> {
        let records = self.scheduler.records_between(
            window.since,
            window.until,
            before.map(|(at, id)| (*at, id.as_str())),
            PAGE,
        )?;
        let next = (records.len() == PAGE as usize)
            .then(|| records.last())
            .flatten()
            .map(|last| (last.requested_at, last.run_id.to_string()));
        Ok(Page {
            facts: records.iter().filter_map(Fact::of).collect(),
            leftovers: records.iter().filter_map(LeftoverFact::of).collect(),
            next,
        })
    }

    /// Every request of `window`, up to [`MAXIMUM_FACTS`], the latest first.
    pub fn all(&self, window: Window) -> Result<Facts> {
        let mut facts = Vec::new();
        let mut leftovers = Vec::new();
        let mut before = None;
        loop {
            let page = self.page(window, before.as_ref())?;
            facts.extend(page.facts);
            leftovers.extend(page.leftovers);
            match page.next {
                Some(_) if facts.len() + leftovers.len() >= MAXIMUM_FACTS => {
                    facts.truncate(MAXIMUM_FACTS);
                    leftovers.truncate(MAXIMUM_FACTS);
                    return Ok(Facts {
                        facts,
                        leftovers,
                        truncated: true,
                    });
                }
                Some(next) => before = Some(next),
                None => {
                    return Ok(Facts {
                        facts,
                        leftovers,
                        truncated: false,
                    });
                }
            }
        }
    }

    pub fn buckets(&self, window: Window, width: Duration) -> Result<Vec<Bucket>> {
        self.load.buckets(window.since, window.until, width)
    }

    /// The blocked episodes and early warnings recorded over `window`. Either
    /// history alone still answers; neither is a failure.
    pub fn outside_history(&self, window: Window) -> Result<OutsideHistory> {
        if self.outside.is_none() && self.warnings.is_none() {
            return Err(Failure::new(
                "outside_pressure_unavailable",
                "This member's outside pressure history could not be opened.",
                "Run elsecall doctor on the member.",
            ));
        }
        let episodes = match &self.outside {
            Some(detector) => detector.episodes(window.since, window.until)?,
            None => Vec::new(),
        };
        let warnings = match &self.warnings {
            Some(report) => report.between(window.since, window.until)?,
            None => Vec::new(),
        };
        Ok(crate::reporting::outside::bounded(
            OutsideHistory { episodes, warnings },
            window,
        ))
    }
}

fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> BoxFuture<'static, Result<T>> {
    Box::pin(async move {
        tokio::task::spawn_blocking(work).await.unwrap_or_else(|_| {
            Err(Failure::new(
                "report_failed",
                "This member's history could not be read.",
                "Run elsecall report again; if it persists, run elsecall doctor.",
            ))
        })
    })
}

impl MemberHistory for OwnHistory {
    fn outside(&self, window: Window) -> BoxFuture<'static, Result<OutsideHistory>> {
        let history = self.clone();
        blocking(move || history.outside_history(window))
    }

    fn peer_io(
        &self,
        window: Window,
    ) -> BoxFuture<'static, Result<crate::peer_diagnostics::History>> {
        blocking(move || Ok(crate::peer_diagnostics::between(window.since, window.until)))
    }

    fn trace_page(
        &self,
        window: Window,
        before: Option<crate::report_protocol::Cursor>,
    ) -> BoxFuture<'static, Result<crate::workload::trace::Page>> {
        let history = self.clone();
        blocking(move || {
            let records = history.scheduler.trace_records(
                window.since,
                window.until,
                before
                    .as_ref()
                    .map(|c| (Timestamp::from_millis(c.requested_at_ms), c.run_id.as_str())),
                crate::workload::trace::PAGE,
            )?;
            let mut page = crate::workload::trace::Page::of_records(&records);
            if before.is_none() {
                page.scheduler_settings = history
                    .scheduler
                    .settings_history(window.since, window.until)?;
            }
            Ok(page)
        })
    }

    fn facts(&self, window: Window) -> BoxFuture<'static, Result<Facts>> {
        let history = self.clone();
        blocking(move || history.all(window))
    }

    fn load(&self, window: Window, width: Duration) -> BoxFuture<'static, Result<Vec<Bucket>>> {
        let history = self.clone();
        blocking(move || history.buckets(window, width))
    }
}
