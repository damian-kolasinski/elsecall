//! Offline read-only history adapter, for exports before installing new routes.
use crate::{
    failure::Result,
    load_log::{buckets, store::LoadStore},
    paths::Paths,
    scheduling::{records::Records, timestamp::Timestamp},
    workload::trace::{Member, PAGE, Trace, VERSION},
};
use std::time::Duration;

pub fn local(paths: &Paths, member: String, since: Timestamp, until: Timestamp) -> Result<Trace> {
    let records = Records::open_read_only(&paths.records())?;
    let load = LoadStore::open_read_only(&paths.load_history())?;
    let minutes = load.between(
        Timestamp::from_millis(since.millis() / 60_000 * 60_000),
        until,
    )?;
    let mut trace=Trace {placement_replay: None, rehearsal: None,
        trace_version:VERSION,incomplete:true,since,until,master:None,members:vec![Member {member:member.clone(),load:buckets::buckets(&minutes,Duration::from_secs(60)),errors:vec![],scheduler_settings:records.scheduler_settings(since,until)?}],requests:vec![],leftovers:vec![],demand:vec![],contention:vec![],limits:vec![
        "local-history export: reads SQLite read-only, excludes the minute in progress, other members and current settings; not a full-fleet fidelity dataset".into(),
        "outside_mean is a grant-derived proxy; effective demand uses an idle baseline and may be confounded by changing background load".into(),
        "workspace identities and scheduler_settings events are absent before H2; estimator history before this window and reconsideration events may be missing; contention is observational".into(),
    ]};
    let mut before = None;
    loop {
        let raw = records.overlapping(
            since,
            until,
            before
                .as_ref()
                .map(|(at, id): &(Timestamp, String)| (*at, id.as_str())),
            PAGE,
        )?;
        let page = crate::workload::trace::Page::of_records(&raw);
        for mut r in page.requests {
            r.fact = r.fact.attributed_to(&member);
            trace.requests.push(r);
        }
        trace.leftovers.extend(page.leftovers);
        match page.next {
            None => break,
            Some(c) => before = Some((Timestamp::from_millis(c.requested_at_ms), c.run_id)),
        }
    }
    trace.model();
    Ok(trace)
}
