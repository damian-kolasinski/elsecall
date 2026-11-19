//! Where the Add Mac sheet gets nearby Macs and runs an add: the installed
//! `elsecall` CLI, run as this app's child, so macOS asks once whether Elsecall
//! may use the local network. The CLI is the one implementation agents use too.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use elsecall_add_mac::add_mac::{AddReport, Event};
use elsecall_add_mac::nearby_macs::NearbyMac;
use elsecall_contract::failure::Failure;
use rune::Waker;

use crate::add_mac_flow::{AddRequest, AddUpdate};

const OUTPUT_LIMIT: u64 = 16 * 1024 * 1024;

pub(crate) trait AddMacSource: Send + Sync {
    /// The Macs on the local network.
    fn nearby(&self) -> Result<Vec<NearbyMac>, Failure>;
    /// Runs add `request.run` to its end, handing each update to `updates` as it
    /// happens; the last is always `Finished`.
    fn add(&self, request: AddRequest, updates: &(dyn Fn(AddUpdate) + Sync));
    /// Stops add `run` if it still runs; it ends its SSH session and reports
    /// `add_stopped`. Stopping before it started stops it as it starts.
    fn stop(&self, run: u64);
}

/// Updates from a background thread, in order, waking the window for each.
pub(crate) struct Inbox<T> {
    items: Mutex<Vec<T>>,
    waker: Mutex<Option<Waker>>,
}

impl<T> Default for Inbox<T> {
    fn default() -> Self {
        Self {
            items: Mutex::new(Vec::new()),
            waker: Mutex::new(None),
        }
    }
}

impl<T> Inbox<T> {
    pub(crate) fn set_waker(&self, waker: Waker) {
        *self.waker.lock().expect("inbox waker") = Some(waker);
    }

    pub(crate) fn push(&self, item: T) {
        self.items.lock().expect("inbox").push(item);
        if let Some(wake) = self.waker.lock().expect("inbox waker").as_ref() {
            wake();
        }
    }

    pub(crate) fn drain(&self) -> Vec<T> {
        std::mem::take(&mut *self.items.lock().expect("inbox"))
    }
}

/// The add that runs now: which one, its process while it has one, and whether
/// it was asked to stop.
#[derive(Default)]
struct Running {
    run: u64,
    pid: Option<u32>,
    stop: bool,
}

/// The installed CLI, run once per look and per add.
pub(crate) struct CliSource {
    /// `None` for the installed CLI, found when needed.
    executable: Option<PathBuf>,
    running: Mutex<Running>,
}

pub(crate) fn production() -> Arc<dyn AddMacSource> {
    Arc::new(CliSource::new(None))
}

impl CliSource {
    pub(crate) fn new(executable: Option<PathBuf>) -> Self {
        Self {
            executable,
            running: Mutex::new(Running::default()),
        }
    }

    fn executable(&self) -> Result<PathBuf, Failure> {
        if let Some(executable) = &self.executable {
            return Ok(executable.clone());
        }
        let executable = elsecall_host::paths::Paths::current()?.executable();
        if executable.is_file() {
            Ok(executable)
        } else {
            Err(Failure::new(
                "not_installed",
                "Elsecall is not installed on this Mac.",
                "Install it with elsecall install, then add a Mac.",
            ))
        }
    }

    fn run(
        &self,
        request: AddRequest,
        updates: &(dyn Fn(AddUpdate) + Sync),
    ) -> Result<AddReport, Failure> {
        let AddRequest {
            run,
            host,
            account,
            password,
        } = request;
        let mut command = Command::new(self.executable()?);
        command
            .args(["fleet", "add", "--progress", "json"])
            .arg(format!("--user={account}"))
            // The host is never read as an option.
            .arg("--")
            .arg(&host)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = {
            let mut running = self.running.lock().expect("running add");
            if running.run == run && running.stop {
                return Err(stopped());
            }
            let child = command
                .spawn()
                .map_err(|error| Failure::io("Start elsecall fleet add", error))?;
            *running = Running {
                run,
                pid: Some(child.id()),
                stop: false,
            };
            child
        };
        // The password, then standard input stays open: when this app goes away,
        // it closes and the add stops.
        let mut stdin = child.stdin.take().expect("piped stdin");
        {
            let mut line = zeroize::Zeroizing::new(password.as_bytes().to_vec());
            line.push(b'\n');
            let _ = stdin.write_all(&line);
            let _ = stdin.flush();
        }
        drop(password);
        let mut stdout = child.stdout.take().expect("piped stdout");
        let reader = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = (&mut stdout).take(OUTPUT_LIMIT).read_to_end(&mut bytes);
            // Keep draining, so the CLI never blocks on a full pipe.
            let _ = std::io::copy(&mut stdout, &mut std::io::sink());
            bytes
        });
        // Each progress event is a JSON line; anything else is not for the sheet.
        let mut stderr = BufReader::new(child.stderr.take().expect("piped stderr"));
        let mut line = Vec::new();
        loop {
            line.clear();
            match stderr.read_until(b'\n', &mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if let Ok(event) = serde_json::from_slice::<Event>(&line) {
                        updates(AddUpdate::Event(event));
                    }
                }
            }
        }
        let stdout = reader.join().unwrap_or_default();
        // Forgotten before the wait, while the exited process still holds its pid.
        self.running.lock().expect("running add").pid = None;
        drop(stdin);
        let _ = child.wait();
        let data = envelope(&stdout)?;
        serde_json::from_value(data).map_err(|_| {
            Failure::new(
                "unexpected_response",
                "Elsecall's report on the added Mac was unreadable.",
                "Run elsecall fleet to see the new member.",
            )
        })
    }
}

fn stopped() -> Failure {
    Failure::new(
        "add_stopped",
        "Adding the Mac was stopped before it finished.",
        "Add it again; it continues where it stopped.",
    )
}

/// An Elsecall JSON document: its data, or its error.
fn envelope(stdout: &[u8]) -> Result<serde_json::Value, Failure> {
    let document: serde_json::Value = serde_json::from_slice(stdout).map_err(|_| {
        Failure::new(
            "unexpected_response",
            "Elsecall gave no answer.",
            "Run the same elsecall fleet command in Terminal to see why.",
        )
    })?;
    match document.get("error") {
        Some(error) => Err(serde_json::from_value(error.clone()).unwrap_or_else(|_| {
            Failure::new(
                "unexpected_response",
                "Elsecall failed without saying why.",
                "Run the same elsecall fleet command in Terminal to see why.",
            )
        })),
        None => Ok(document["data"].clone()),
    }
}

impl AddMacSource for CliSource {
    fn nearby(&self) -> Result<Vec<NearbyMac>, Failure> {
        let output = Command::new(self.executable()?)
            .args(["fleet", "nearby", "--browse", "2s"])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .map_err(|error| Failure::io("Run elsecall fleet nearby", error))?;
        let data = envelope(&output.stdout)?;
        serde_json::from_value(data["macs"].clone()).map_err(|_| {
            Failure::new(
                "unexpected_response",
                "Elsecall's list of nearby Macs was unreadable.",
                "Run elsecall fleet nearby in Terminal to see why.",
            )
        })
    }

    fn add(&self, request: AddRequest, updates: &(dyn Fn(AddUpdate) + Sync)) {
        updates(AddUpdate::Finished(self.run(request, updates)));
    }

    fn stop(&self, run: u64) {
        let mut running = self.running.lock().expect("running add");
        if running.run != run {
            // Not started yet: it stops as it starts.
            *running = Running {
                run,
                pid: None,
                stop: true,
            };
            return;
        }
        running.stop = true;
        if let Some(pid) = running.pid {
            // The CLI ends its SSH session and reports add_stopped.
            // SAFETY: kill has no memory-safety preconditions; the pid is this
            // add's child, which has not been waited for, so it is not reused.
            unsafe { libc::kill(pid as i32, libc::SIGTERM) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use zeroize::Zeroizing;

    #[test]
    fn documents_give_their_data_or_their_error() {
        let ok = envelope(br#"{"schema_version":1,"data":{"macs":[]}}"#).unwrap();
        assert_eq!(ok["macs"], serde_json::json!([]));
        let failed =
            envelope(br#"{"schema_version":1,"error":{"code":"x","message":"m","hint":"h"}}"#)
                .unwrap_err();
        assert_eq!(failed.code, "x");
        assert_eq!(envelope(b"").unwrap_err().code, "unexpected_response");
    }

    #[test]
    fn the_inbox_keeps_order_and_wakes() {
        let inbox = Inbox::default();
        let woke = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = Arc::clone(&woke);
        inbox.set_waker(Arc::new(move || {
            count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }));
        inbox.push(1);
        inbox.push(2);
        assert_eq!(inbox.drain(), [1, 2]);
        assert!(inbox.drain().is_empty());
        assert_eq!(woke.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    /// A stand-in CLI, in Perl for its dependable signal handling: records its
    /// arguments and password, prints two events and a line that is not one, then
    /// ends with a report, or waits until SIGTERM and reports the stop.
    fn fake_cli(directory: &std::path::Path, wait: bool) -> PathBuf {
        let script = directory.join("elsecall");
        let ending = if wait {
            r#"while (1) { select(undef, undef, undef, 0.05); }"#
        } else {
            r#"print '{"schema_version":1,"data":{"member":"macbook-pro","host":"macbook-pro.local","mac":{"model":"Mac16,10","serial_number":null,"macos_version":"26.1","ssh_host_key":"SHA256:x (ED25519)"},"readiness":"awaiting_provisioning","tailscale_key_expiry":null,"steps":[],"next_step":""}}';"#
        };
        std::fs::write(
            &script,
            format!(
                r#"#!/usr/bin/perl
$| = 1;
$SIG{{TERM}} = sub {{ print '{{"schema_version":1,"error":{{"code":"add_stopped","message":"m","hint":"h"}}}}'; exit 1; }};
open(my $args, '>', '{dir}/args'); print $args join("\n", @ARGV), "\n"; close($args);
my $password = <STDIN>; chomp($password);
open(my $saved, '>', '{dir}/password'); print $saved $password; close($saved);
print STDERR '{{"event":"step","step":"check"}}', "\n";
print STDERR "not an event\n";
print STDERR '{{"event":"approve","link":"https://login.tailscale.com/a/example","approval":"login"}}', "\n";
{ending}
"#,
                dir = directory.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        script
    }

    fn request(run: u64) -> AddRequest {
        AddRequest {
            run,
            host: "macbook-pro.local".into(),
            account: "devbox".into(),
            password: Zeroizing::new("s3cret pass".into()),
        }
    }

    #[test]
    fn an_add_runs_the_cli_with_the_password_on_stdin_and_reads_its_events() {
        let directory = tempfile::tempdir().unwrap();
        let source = CliSource::new(Some(fake_cli(directory.path(), false)));
        let updates = Mutex::new(Vec::new());
        source.add(request(1), &|update| updates.lock().unwrap().push(update));
        let updates = updates.into_inner().unwrap();
        assert!(matches!(updates[0], AddUpdate::Event(Event::Step { .. })));
        assert!(matches!(
            updates[1],
            AddUpdate::Event(Event::Approve { .. })
        ));
        assert!(
            matches!(&updates[2], AddUpdate::Finished(Ok(report)) if report.member == "macbook-pro")
        );
        assert_eq!(updates.len(), 3, "a line that is not an event is skipped");
        assert_eq!(
            std::fs::read_to_string(directory.path().join("args")).unwrap(),
            "fleet\nadd\n--progress\njson\n--user=devbox\n--\nmacbook-pro.local\n"
        );
        assert_eq!(
            std::fs::read_to_string(directory.path().join("password")).unwrap(),
            "s3cret pass"
        );
    }

    #[test]
    fn stopping_an_add_ends_it_even_before_it_starts() {
        let directory = tempfile::tempdir().unwrap();
        let source = Arc::new(CliSource::new(Some(fake_cli(directory.path(), true))));
        // Stopped before it started: it never runs.
        source.stop(7);
        let updates = Mutex::new(Vec::new());
        source.add(request(7), &|update| updates.lock().unwrap().push(update));
        assert!(matches!(
            updates.into_inner().unwrap().as_slice(),
            [AddUpdate::Finished(Err(failure))] if failure.code == "add_stopped"
        ));
        // Stopped while it runs: SIGTERM, and the CLI's own answer.
        let running = Arc::clone(&source);
        let (done, finished) = std::sync::mpsc::channel();
        let (started, running_now) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            running.add(request(8), &|update| {
                match update {
                    // The stand-in prints it once it handles SIGTERM.
                    AddUpdate::Event(Event::Approve { .. }) => started.send(()).unwrap(),
                    AddUpdate::Finished(result) => {
                        done.send(result.map(|_| ()).map_err(|f| f.code)).unwrap()
                    }
                    AddUpdate::Event(_) => (),
                }
            });
        });
        running_now
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the add started");
        source.stop(8);
        let result = finished
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap();
        assert_eq!(result, Err("add_stopped".into()));
    }
}
