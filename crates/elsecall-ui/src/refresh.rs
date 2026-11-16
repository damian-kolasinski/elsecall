//! Polls a [`FleetSource`](crate::fleet_source::FleetSource) off the UI thread
//! and hands each result to the window through [`rune::Latest`].

use std::sync::Arc;
use std::thread;
use std::time::Duration;

use rune::Latest;

use crate::fleet_source::{FleetSnapshot, FleetSource, SourceFailure};

/// Gap after each answer before the next read. A slow fleet read is not followed immediately by another.
pub const INTERVAL: Duration = Duration::from_secs(2);

pub struct Update {
    pub result: Result<FleetSnapshot, SourceFailure>,
    /// Local time of day, `HH:MM:SS`, when this read finished.
    pub refreshed_at: String,
}

pub fn start(feed: Arc<Latest<Update>>, source: Arc<dyn FleetSource>) {
    let _detached = thread::Builder::new()
        .name("elsecall-ui-fleet".into())
        .spawn(move || poll_loop(feed, source))
        .expect("start the fleet refresh thread");
}

fn poll_loop(feed: Arc<Latest<Update>>, source: Arc<dyn FleetSource>) {
    loop {
        let result = source.fleet();
        let refreshed_at = local_hms();
        feed.request((result, refreshed_at), |(result, refreshed_at)| Update {
            result,
            refreshed_at,
        });
        thread::sleep(INTERVAL);
    }
}

fn local_hms() -> String {
    let Ok(since) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) else {
        return "00:00:00".into();
    };
    let secs = since.as_secs() as libc::time_t;
    // `localtime_r` fills `tm` and is safe to call from more than one thread.
    let mut tm = unsafe { std::mem::zeroed::<libc::tm>() };
    let filled = unsafe { !libc::localtime_r(&secs, &mut tm).is_null() };
    if !filled {
        return "00:00:00".into();
    }
    format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
}
