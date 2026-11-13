//! Measures one core's speed with a small compile-like workload: sorting, filling a
//! hash table and scanning bytes, which exercise branches, caches and memory the
//! way builds and tests do, rather than one instruction a single unit executes.
//!
//! Each trial is timed by the measuring thread's CPU clock, so time the scheduler
//! gives to other processes does not count. The thread asks for user-interactive
//! quality of service, which macOS places on performance cores when one is free.
//! The best of several trials is the measurement.
use crate::{
    failure::{Failure, Result},
    scheduling::speed::SpeedMeter,
};
use std::{collections::HashMap, hint::black_box, time::Duration};

/// Rounds per trial: about 27 ms of CPU on an M4 Pro performance core.
const ROUNDS: u32 = 200;
const TRIALS: usize = 5;
/// Scales rounds per CPU second so that an M4 Pro performance core measures about 1.
const UNIT: f64 = 7500.0;

/// `QOS_CLASS_USER_INTERACTIVE` from sys/qos.h.
const USER_INTERACTIVE: u32 = 0x21;
unsafe extern "C" {
    fn pthread_set_qos_class_self_np(class: u32, relative_priority: i32) -> i32;
}

#[derive(Default)]
pub struct CoreBenchmark;

impl SpeedMeter for CoreBenchmark {
    fn measure(&self) -> Result<f64> {
        std::thread::Builder::new()
            .name("speed".into())
            .spawn(|| {
                // SAFETY: changes only the calling thread's scheduling class.
                unsafe { pthread_set_qos_class_self_np(USER_INTERACTIVE, 0) };
                let mut best: Option<f64> = None;
                for trial in 0..TRIALS {
                    let started = thread_cpu()?;
                    let mut checksum = 0;
                    for round in 0..ROUNDS {
                        checksum ^= workload(u64::from(round) + trial as u64 * 1000);
                    }
                    black_box(checksum);
                    let spent = thread_cpu()?.saturating_sub(started).as_secs_f64();
                    if spent > 0.0 {
                        let speed = f64::from(ROUNDS) / spent / UNIT;
                        best = Some(best.map_or(speed, |b: f64| b.max(speed)));
                    }
                }
                best.ok_or_else(|| unavailable("the CPU clock did not advance"))
            })
            .map_err(|e| Failure::io("Start the speed measurement", e))?
            .join()
            .unwrap_or_else(|_| Err(unavailable("the measurement panicked")))
    }
}

fn unavailable(why: &str) -> Failure {
    Failure::new(
        "speed_unavailable",
        format!("This member's speed could not be measured: {why}."),
        "Placement treats the member as of typical speed until a measurement succeeds.",
    )
}

/// CPU time the calling thread has used.
fn thread_cpu() -> Result<Duration> {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `now` is a valid timespec for the call to fill.
    if unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut now) } != 0 {
        return Err(Failure::io(
            "Read the thread's CPU clock",
            std::io::Error::last_os_error(),
        ));
    }
    Ok(Duration::new(now.tv_sec as u64, now.tv_nsec as u32))
}

/// One round of work, seeded so no two rounds can be folded together.
fn workload(seed: u64) -> u64 {
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let mut values: Vec<u64> = (0..8192).map(|_| next()).collect();
    values.sort_unstable();
    let mut table: HashMap<u64, u32> = HashMap::with_capacity(2048);
    for value in values.iter().step_by(2) {
        *table.entry(value % 2039).or_default() += 1;
    }
    let bytes: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
    let rising = bytes.windows(2).filter(|w| w[0] < w[1]).count() as u64;
    values[values.len() / 2] ^ table.len() as u64 ^ rising
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measures_a_positive_repeatable_speed() {
        let meter = CoreBenchmark;
        let first = meter.measure().unwrap();
        let second = meter.measure().unwrap();
        assert!(first > 0.01 && first < 20.0, "{first}");
        // The best of five trials, on the thread's own clock, is stable even on a
        // busy machine; allow for a trial that never reached a performance core.
        let ratio = first.max(second) / first.min(second);
        assert!(ratio < 4.0, "{first} vs {second}");
    }
}
