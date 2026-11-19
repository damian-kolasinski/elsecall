//! What a failed integration test prints about the processes it drove.
//!
//! A fixture registers each log it owns, such as a member's service log or a
//! rehearsal controller's output, and notes the commands it runs. When the test
//! thread panics, for any assertion, a panic hook prints the recent commands
//! with their times and outcomes, then every registered log. A CI failure then
//! explains itself without a rerun. Fixture logs carry temporary paths, loopback
//! addresses and event names only.
#![allow(dead_code)]
use std::{
    cell::RefCell,
    collections::VecDeque,
    fmt::Write as _,
    path::PathBuf,
    sync::{
        Once,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

/// Commands kept for the report; polling repeats collapse into one line.
const JOURNAL: usize = 200;
/// The tail of each log that is printed.
const LOG_TAIL: usize = 32 * 1024;

struct Note {
    at: f64,
    text: String,
    repeats: u32,
    last: f64,
    /// The shortest and longest duration among the collapsed repeats.
    took: Option<(Duration, Duration)>,
}

struct Registered {
    id: u64,
    label: String,
    paths: Vec<PathBuf>,
}

struct Context {
    started: Instant,
    logs: Vec<Registered>,
    journal: VecDeque<Note>,
}

thread_local! {
    static CONTEXT: RefCell<Context> = RefCell::new(Context {
        started: Instant::now(),
        logs: Vec::new(),
        journal: VecDeque::new(),
    });
}

/// Removes its log from the report when dropped.
pub(crate) struct Log(u64);
impl Drop for Log {
    fn drop(&mut self) {
        let id = self.0;
        let _ = CONTEXT.try_with(|c| c.borrow_mut().logs.retain(|l| l.id != id));
    }
}

/// Prints `paths`, in order, under `label` if this thread's test fails while
/// the returned guard lives.
pub(crate) fn register(label: impl Into<String>, paths: Vec<PathBuf>) -> Log {
    // Process-wide, so a guard dropped on another thread removes nothing else.
    static NEXT: AtomicU64 = AtomicU64::new(0);
    install();
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    CONTEXT.with(|c| {
        c.borrow_mut().logs.push(Registered {
            id,
            label: label.into(),
            paths,
        })
    });
    Log(id)
}

/// Records something the test did, with the time since the thread's first
/// note or registration. An identical consecutive note only counts a repeat.
pub(crate) fn note(text: String) {
    record(text, None);
}

/// Records a command that took `took`. Consecutive notes with the same text,
/// such as a polling loop's, collapse into one line with their duration range.
pub(crate) fn note_timed(text: String, took: Duration) {
    record(text, Some(took));
}

fn record(text: String, took: Option<Duration>) {
    install();
    CONTEXT.with(|c| {
        let mut c = c.borrow_mut();
        let at = c.started.elapsed().as_secs_f64();
        if let Some(last) = c.journal.back_mut()
            && last.text == text
        {
            last.repeats += 1;
            last.last = at;
            last.took = match (last.took, took) {
                (Some((low, high)), Some(took)) => Some((low.min(took), high.max(took))),
                (range, None) => range,
                (None, Some(took)) => Some((took, took)),
            };
            return;
        }
        if c.journal.len() == JOURNAL {
            c.journal.pop_front();
        }
        c.journal.push_back(Note {
            at,
            text,
            repeats: 1,
            last: at,
            took: took.map(|t| (t, t)),
        });
    });
}

fn install() {
    static HOOK: Once = Once::new();
    HOOK.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            previous(info);
            // Printed through the test harness's capture of this thread.
            if let Some(report) = report() {
                eprintln!("{report}");
            }
        }));
    });
}

fn report() -> Option<String> {
    CONTEXT
        .try_with(|c| {
            let c = c.try_borrow().ok()?;
            if c.logs.is_empty() && c.journal.is_empty() {
                return None;
            }
            let mut out = String::new();
            let mut load = [0.0f64; 3];
            // SAFETY: getloadavg writes at most the three elements it is given.
            unsafe { libc::getloadavg(load.as_mut_ptr(), 3) };
            let _ = writeln!(
                out,
                "\n=== test diagnostics: t+{:.3}s at {}, load {:.2} {:.2} {:.2}",
                c.started.elapsed().as_secs_f64(),
                elsecall_scheduling_model::timestamp::Timestamp::now(),
                load[0],
                load[1],
                load[2]
            );
            let _ = writeln!(out, "--- commands (latest {JOURNAL})");
            for note in &c.journal {
                let took = match note.took {
                    Some((low, high)) if low == high => format!(" in {:.3}s", low.as_secs_f64()),
                    Some((low, high)) => {
                        format!(" in {:.3}-{:.3}s", low.as_secs_f64(), high.as_secs_f64())
                    }
                    None => String::new(),
                };
                if note.repeats == 1 {
                    let _ = writeln!(out, "t+{:.3}s {}{took}", note.at, note.text);
                } else {
                    let _ = writeln!(
                        out,
                        "t+{:.3}s..{:.3}s {} (x{}){took}",
                        note.at, note.last, note.text, note.repeats
                    );
                }
            }
            for log in &c.logs {
                for path in &log.paths {
                    let _ = writeln!(out, "--- {} ({})", log.label, path.display());
                    match std::fs::read(path) {
                        Ok(bytes) => {
                            let tail = &bytes[bytes.len().saturating_sub(LOG_TAIL)..];
                            if tail.len() < bytes.len() {
                                let _ =
                                    writeln!(out, "[{} earlier bytes]", bytes.len() - tail.len());
                            }
                            out.push_str(&String::from_utf8_lossy(tail));
                            if !out.ends_with('\n') {
                                out.push('\n');
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                            out.push_str("[absent]\n");
                        }
                        Err(error) => {
                            let _ = writeln!(out, "[unreadable: {error}]");
                        }
                    }
                }
            }
            let _ = write!(out, "=== end of test diagnostics");
            Some(out)
        })
        .ok()
        .flatten()
}
