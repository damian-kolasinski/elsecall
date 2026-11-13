//! Fixed CPU-service budgets with fractional-core pacing. Calibration converts
//! frozen work units to CPU seconds, never to a required instruction count. This
//! keeps H1 CPU demand stable across core types and frequency changes. Scheduler
//! stalls still extend completion; deadline exhaustion fails rather than truncates.
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Calibration {
    pub iterations: u64,
    pub elapsed_seconds: f64,
    pub cpu_seconds: f64,
    pub iterations_per_second: f64,
    pub logical_cores: u32,
}
impl Calibration {
    pub fn valid(&self) -> bool {
        self.iterations > 0
            && self.elapsed_seconds.is_finite()
            && self.elapsed_seconds > 0.0
            && self.cpu_seconds.is_finite()
            && self.cpu_seconds > 0.0
            && self.iterations_per_second.is_finite()
            && self.iterations_per_second > 0.0
            && self.logical_cores > 0
            && self.logical_cores <= 256
            && (self.iterations as f64 / self.cpu_seconds / self.iterations_per_second - 1.0).abs()
                < 0.000001
    }
}
pub async fn calibrate(
    cores: crate::inspection::CoreCounts,
) -> crate::failure::Result<Calibration> {
    use crate::failure::Failure;
    let script = r#"use Time::HiRes qw(clock_gettime CLOCK_MONOTONIC CLOCK_PROCESS_CPUTIME_ID);
my $start = clock_gettime(CLOCK_MONOTONIC); my $cpu_start = clock_gettime(CLOCK_PROCESS_CPUTIME_ID); my $x = 17; my $n = 0;
while (clock_gettime(CLOCK_MONOTONIC) - $start < 0.25) {
    for (1..8192) { $x = ($x * 1664525 + 1013904223) % 4294967296; } $n += 8192;
}
print "$n ", clock_gettime(CLOCK_MONOTONIC)-$start, " ", clock_gettime(CLOCK_PROCESS_CPUTIME_ID)-$cpu_start, "\n";"#;
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tokio::process::Command::new("/usr/bin/perl")
            .args(["-e", script])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| super::invalid("Worker calibration timed out."))?
    .map_err(|e| Failure::io("Calibrate rehearsal work", e))?;
    let text = String::from_utf8_lossy(&output.stdout);
    let values: Vec<_> = text.split_whitespace().collect();
    if !output.status.success() || values.len() != 3 {
        return Err(super::invalid("Worker calibration failed."));
    }
    let iterations: u64 = values[0]
        .parse()
        .map_err(|_| super::invalid("Invalid calibration work."))?;
    let elapsed_seconds: f64 = values[1]
        .parse()
        .map_err(|_| super::invalid("Invalid calibration duration."))?;
    let cpu_seconds: f64 = values[2]
        .parse()
        .map_err(|_| super::invalid("Invalid calibration CPU time."))?;
    let result = Calibration {
        iterations,
        elapsed_seconds,
        cpu_seconds,
        iterations_per_second: iterations as f64 / cpu_seconds,
        logical_cores: cores.performance + cores.efficiency,
    };
    if !result.valid() {
        return Err(super::invalid("Invalid worker calibration."));
    }
    Ok(result)
}

pub const SCRIPT: &str = r#"use strict;
use warnings;
use Time::HiRes qw(clock_gettime CLOCK_MONOTONIC CLOCK_PROCESS_CPUTIME_ID sleep time);
my ($seconds, $seed, $exit, $until, $work, @targets) = @ARGV;
my ($cores, $rate);
while (@targets) {
    my ($member, $demand, $speed) = splice(@targets, 0, 3);
    ($cores, $rate) = ($demand, $speed) if $member eq ($ENV{ELSECALL_MEMBER} // '');
}
die "missing member calibration\n" unless defined($cores) && $rate > 0;
my $workers = int($cores); $workers++ if $cores > $workers;
my @children;
$SIG{TERM} = $SIG{INT} = sub { kill 'TERM', @children if @children; waitpid($_, 0) for @children; exit 143; };
for (my $i = 0; $i < $workers; $i++) {
    my $pid = fork();
    unless (defined $pid) { kill 'TERM', @children if @children; waitpid($_, 0) for @children; die "fork failed\n"; }
    if (!$pid) {
        $SIG{TERM} = $SIG{INT} = 'DEFAULT';
        my $fraction = $cores / $workers; $fraction = 1 if $fraction > 1;
        # Work units are frozen across arms; each member converts them using its
        # own calibration. Spend that CPU budget even if the OS changes cores.
        my $budget = $work / ($rate * $workers);
        my $x = $seed + $i;
        my $start = clock_gettime(CLOCK_MONOTONIC);
        my $cpu_start = clock_gettime(CLOCK_PROCESS_CPUTIME_ID);
        my $used = 0;
        while ($used < $budget) {
            exit 124 if time() >= $until;
            # Small batches bound CPU-budget overshoot independently of the
            # calibration rate. The process CPU clock excludes scheduler stalls.
            my $batch = int(($budget - $used) * $rate / 8);
            $batch = 8192 if $batch > 8192; $batch = 1 if $batch < 1;
            for (1..$batch) { $x = ($x * 1664525 + 1013904223) % 4294967296; }
            $used = clock_gettime(CLOCK_PROCESS_CPUTIME_ID) - $cpu_start;
            if ($fraction < 1) {
                my $rest = $start + $used / $fraction - clock_gettime(CLOCK_MONOTONIC);
                sleep($rest) if $rest > 0;
            }
        }
        exit 0;
    }
    push @children, $pid;
}
# Zero-CPU inputs represent a timed holder rather than CPU work.
if (!$workers) {
    my $remaining = $until - time();
    exit 124 if $remaining < $seconds;
    sleep($seconds);
}
my $failed = 0;
for (@children) { waitpid($_, 0); $failed ||= $?; }
exit($failed ? 124 : $exit);
"#;

/// H1 fits machine CPU share times weighted capacity. OS CPU time is measured
/// against all logical CPUs, so fractional busy workers need this conversion.
pub fn logical_demand(effective: f64, cores: crate::inspection::CoreCounts) -> f64 {
    let weighted =
        crate::scheduling::capacity::Capacity::of(cores, crate::scheduling::capacity::Cores::ZERO)
            .weighted_cores
            .as_f64();
    effective * f64::from(cores.performance + cores.efficiency) / weighted
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inspection::CoreCounts;
    #[test]
    fn converts_h1_weighted_demand_to_the_os_cpu_denominator() {
        assert_eq!(
            logical_demand(
                1.0,
                CoreCounts {
                    performance: 4,
                    efficiency: 4
                }
            ),
            8.0 / 6.0
        );
        assert_eq!(
            logical_demand(
                2.0,
                CoreCounts {
                    performance: 8,
                    efficiency: 0
                }
            ),
            2.0
        );
        assert_eq!(
            logical_demand(
                6.0,
                CoreCounts {
                    performance: 10,
                    efficiency: 4
                }
            ),
            7.0
        );
    }
}

#[cfg(all(test, target_os = "macos"))]
mod progress_tests {
    use super::*;
    use std::os::unix::process::CommandExt;
    use std::{
        fs,
        process::Command,
        time::{Duration, Instant},
    };

    // Reproducible contention: withhold CPU for 400 ms by descheduling only the
    // test's private process group. Host CPU count/load cannot erase this stall.
    fn elapsed(
        script: &std::path::Path,
        work: u64,
        rate: f64,
        stall: bool,
    ) -> (Duration, Duration) {
        let started = Instant::now();
        let mut child = Command::new("/usr/bin/perl")
            .arg(script)
            .args(["0.7", "17", "0"])
            .arg(
                (crate::scheduling::timestamp::Timestamp::now().millis() as f64 / 1000.0 + 20.0)
                    .to_string(),
            )
            .arg(work.to_string())
            .args(["fixture", "1"])
            .arg(rate.to_string())
            .env("ELSECALL_MEMBER", "fixture")
            .process_group(0)
            .spawn()
            .unwrap();
        if stall {
            std::thread::sleep(Duration::from_millis(100));
            assert_eq!(
                unsafe { libc::kill(-(child.id() as i32), libc::SIGSTOP) },
                0
            );
            std::thread::sleep(Duration::from_millis(400));
            assert_eq!(
                unsafe { libc::kill(-(child.id() as i32), libc::SIGCONT) },
                0
            );
        }
        crate::macos::process_usage::wait_exited(child.id()).unwrap();
        let cpu = crate::macos::process_usage::cpu_time(child.id()).unwrap();
        assert!(child.wait().unwrap().success());
        (started.elapsed(), cpu)
    }
    #[tokio::test]
    async fn fixed_work_stretches_when_cpu_is_withheld_but_wall_lifetime_does_not() {
        let c = calibrate(crate::inspection::CoreCounts {
            performance: 1,
            efficiency: 0,
        })
        .await
        .unwrap();
        let temp = tempfile::tempdir().unwrap();
        let fixed = temp.path().join("fixed.pl");
        fs::write(&fixed, SCRIPT).unwrap();
        let work = (c.iterations_per_second * 0.7) as u64;
        let normal = elapsed(&fixed, work, c.iterations_per_second, false);
        let stalled = elapsed(&fixed, work, c.iterations_per_second, true);
        // Other parallel tests can supply additional contention to either
        // sample. CPU work remains fixed; wall duration owes the withheld CPU
        // service even when a noisy unstalled sample also suffered preemption.
        assert!(
            (normal.1.as_secs_f64() - stalled.1.as_secs_f64()).abs() / normal.1.as_secs_f64()
                < 0.35,
            "normal {normal:?}; stalled {stalled:?}"
        );
        assert!(
            stalled.0.as_secs_f64() >= stalled.1.as_secs_f64() + 0.35,
            "stalled {stalled:?}"
        );
        let lifetime = temp.path().join("lifetime.pl");
        fs::write(&lifetime,"use Time::HiRes qw(time); my $end=time()+$ARGV[0]; my $x=17; while(time()<$end){$x=($x*1664525+1013904223)%4294967296;}").unwrap();
        let normal = elapsed(&lifetime, work, c.iterations_per_second, false);
        let stalled = elapsed(&lifetime, work, c.iterations_per_second, true);
        assert!(
            stalled.0.abs_diff(normal.0) < Duration::from_millis(400),
            "normal {normal:?}; stalled {stalled:?}"
        );
    }
}

#[cfg(all(test, target_os = "macos"))]
mod cpu_tests {
    use super::*;
    #[tokio::test]
    async fn slower_instruction_service_does_not_inflate_the_cpu_budget() {
        use crate::macos::process_usage;
        use std::{fs, process::Command};
        let c = calibrate(crate::inspection::CoreCounts {
            performance: 1,
            efficiency: 0,
        })
        .await
        .unwrap();
        let temp = tempfile::tempdir().unwrap();
        // Emulate a slower core/kernel without changing budget or pacing inputs.
        let kernel = "$x = ($x * 1664525 + 1013904223) % 4294967296;";
        for (name, script) in [
            ("normal", SCRIPT.to_owned()),
            ("slow", SCRIPT.replace(kernel, &kernel.repeat(4))),
        ] {
            let path = temp.path().join(format!("{name}.pl"));
            fs::write(&path, script).unwrap();
            let mut child = Command::new("/usr/bin/perl")
                .arg(path)
                .args(["0.5", "17", "0"])
                .arg(
                    (crate::scheduling::timestamp::Timestamp::now().millis() as f64 / 1000.0
                        + 20.0)
                        .to_string(),
                )
                .arg((c.iterations_per_second * 0.25).round().to_string())
                .args(["fixture", "0.5"])
                .arg(c.iterations_per_second.to_string())
                .env("ELSECALL_MEMBER", "fixture")
                .spawn()
                .unwrap();
            process_usage::wait_exited(child.id()).unwrap();
            let cpu = process_usage::cpu_time(child.id()).unwrap().as_secs_f64();
            assert!(child.wait().unwrap().success());
            assert!((cpu - 0.25).abs() < 0.04, "{name}: {cpu} CPU seconds");
        }
    }

    #[tokio::test]
    async fn fractional_and_multicore_fixed_work_consumes_the_calibrated_cpu_budget() {
        use crate::macos::process_usage;
        use std::{fs, process::Command};
        let c = calibrate(crate::inspection::CoreCounts {
            performance: 4,
            efficiency: 4,
        })
        .await
        .unwrap();
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("worker.pl");
        fs::write(&path, SCRIPT).unwrap();
        for target in [0.25, 1.25] {
            let logical = logical_demand(
                target,
                crate::inspection::CoreCounts {
                    performance: 4,
                    efficiency: 4,
                },
            );
            let expected_cpu = logical * 1.5;
            let work = (c.iterations_per_second * expected_cpu).round() as u64;
            let mut worker = Command::new("/usr/bin/perl")
                .arg(&path)
                .args(["1.5", "17", "0"])
                .arg(
                    (crate::scheduling::timestamp::Timestamp::now().millis() as f64 / 1000.0
                        + 30.0)
                        .to_string(),
                )
                .arg(work.to_string())
                .arg("fixture")
                .arg(logical.to_string())
                .arg(c.iterations_per_second.to_string())
                .env("ELSECALL_MEMBER", "fixture")
                .spawn()
                .unwrap();
            process_usage::wait_exited(worker.id()).unwrap();
            let cpu = process_usage::cpu_time(worker.id()).unwrap().as_secs_f64();
            assert!(worker.wait().unwrap().success());
            // CPU work, unlike the old CPU/wall assertion, is invariant under
            // scheduler stalls. Frequency/core heterogeneity retains a 35% band.
            assert!(
                (cpu - expected_cpu).abs() / expected_cpu <= 0.35,
                "target {target}, CPU {cpu}, budget {expected_cpu}"
            );
        }
    }
}
