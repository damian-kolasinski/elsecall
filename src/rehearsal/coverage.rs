//! Minute evidence coverage for rehearsal arrival windows and their drain.
use crate::workload::trace::Trace;

pub(crate) struct Gap {
    pub member: String,
    pub since: u64,
    pub until: u64,
}
impl Gap {
    pub fn reason(&self) -> String {
        format!(
            "missing_drain_load: {}: {}..{}",
            self.member,
            crate::scheduling::timestamp::Timestamp::from_millis(self.since),
            crate::scheduling::timestamp::Timestamp::from_millis(self.until)
        )
    }
}
pub(crate) fn gaps(trace: &Trace) -> Vec<Gap> {
    let end = trace
        .requests
        .iter()
        .filter(|r| r.fact.requested_at < trace.until)
        .filter_map(|r| r.terminal_at())
        .fold(trace.until, std::cmp::max)
        .millis();
    let mut gaps = Vec::new();
    for member in &trace.members {
        let mut at = trace.since.millis() / 60_000 * 60_000;
        let mut open = None;
        while at < end {
            let i = member.load.partition_point(|b| b.at.millis() < at);
            let known = member.load.get(i).is_some_and(|b| {
                b.at.millis() == at
                    && b.seconds > 0
                    && b.cpu_mean.is_some()
                    && b.speed_max.or(b.speed_min).is_some()
            });
            if known {
                if let Some(since) = open.take() {
                    gaps.push(Gap {
                        member: member.member.clone(),
                        since,
                        until: at,
                    });
                }
            } else {
                open.get_or_insert(at.max(trace.since.millis()));
            }
            at += 60_000;
        }
        if let Some(since) = open {
            gaps.push(Gap {
                member: member.member.clone(),
                since,
                until: end,
            });
        }
    }
    gaps
}
