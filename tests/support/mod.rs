//! A real service on a real socket, driven by the real CLI binary. Only the
//! machine's load is faked, as an external observation boundary.
#![allow(dead_code)]
use elsecall::{
    failure::Result,
    inspection::{
        CoreCounts, Inspection, LoadAverage, LoadSample, Machine, MachineInspector, Memory,
        MemoryPressure, ThermalState,
    },
    macos::processes::MacProcesses,
    paths::Paths,
    peer_listener, private_files,
    scheduling::service::Adapters,
    server,
    tailnet::{Presence, Tailnet},
};
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::process::Child;

/// These tests never join a fleet, so the member listener has no tailnet to bind.
struct NoTailnet;
impl Tailnet for NoTailnet {
    fn discover(&self) -> Result<Presence> {
        Err(elsecall::tailnet::unavailable("not connected"))
    }
}

pub struct FakeMachine(pub Mutex<LoadSample>, pub std::sync::atomic::AtomicUsize);
impl MachineInspector for FakeMachine {
    fn inspect(&self) -> Result<Machine> {
        Ok(Machine {
            hostname: Some("test-member.local".into()),
            os: "macos".into(),
            os_version: None,
            architecture: "aarch64".into(),
            logical_cpus: 2,
            load_average: LoadAverage {
                one_minute: 0.0,
                five_minutes: 0.0,
                fifteen_minutes: 0.0,
            },
            memory: Memory {
                total_bytes: 1,
                used_bytes: 0,
                available_bytes: 1,
            },
            uptime_seconds: 1,
            load: self.sample_load()?,
        })
    }
    fn sample_load(&self) -> Result<LoadSample> {
        let load = self.0.lock().unwrap();
        self.1.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(load.clone())
    }
}

pub fn quiet(performance_cores: u32) -> LoadSample {
    LoadSample {
        cores: CoreCounts {
            performance: performance_cores,
            efficiency: 0,
        },
        cpu_utilization: Some(0.1),
        memory_pressure: MemoryPressure::Normal,
        thermal_state: ThermalState::Nominal,
    }
}

pub struct Fixture {
    pub home: tempfile::TempDir,
    pub paths: Paths,
    pub machine: Arc<FakeMachine>,
    server: Option<tokio::task::JoinHandle<Result<()>>>,
    children: Vec<Child>,
}

impl Fixture {
    /// A service whose capacity is `cores` weighted cores.
    pub async fn start(cores: u32) -> Self {
        let home = tempfile::Builder::new()
            .prefix("ec-")
            .tempdir_in("/tmp")
            .unwrap();
        let paths = Paths::for_home(home.path()).unwrap();
        private_files::directory(&paths.data).unwrap();
        let mut fixture = Self {
            home,
            paths,
            machine: Arc::new(FakeMachine(
                Mutex::new(quiet(cores)),
                std::sync::atomic::AtomicUsize::new(0),
            )),
            server: None,
            children: Vec::new(),
        };
        fixture.serve().await;
        fixture
    }

    pub async fn serve(&mut self) {
        let (processes, exits) = MacProcesses::start().unwrap();
        self.server = Some(tokio::spawn(server::serve(
            self.paths.clone(),
            Arc::new(Inspection::new(self.machine.clone())),
            Adapters {
                machine: self.machine.clone(),
                speed: Arc::new(elsecall::machine_script::FixedSpeed(1.0)),
                processes,
                exits,
            },
            Arc::new(elsecall::macos::process_sampler::MacProcessSampler),
            peer_listener::Settings {
                tailnet: Arc::new(NoTailnet),
                port: 1,
            },
        )));
        let deadline = Instant::now() + crate::processes::WAIT;
        loop {
            let server = self.server.as_mut().unwrap();
            if server.is_finished() {
                panic!("the service stopped: {:?}", server.await);
            }
            let (code, json) = self.cli(&["runs", "--limit", "1"]).await;
            if code == 0 && json["data"]["member"]["load"].is_object() {
                return;
            }
            assert!(Instant::now() < deadline, "service not ready: {json}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Stops the service abruptly, as a crash or an update would.
    pub async fn stop(&mut self) {
        let server = self.server.take().unwrap();
        server.abort();
        let _ = server.await;
        // A concurrent fork can hold the old service's close-on-exec lock until
        // it execs. Wait for release before starting a successor, just as restart
        // does; never retry a failed service start.
        let deadline = Instant::now() + crate::processes::WAIT;
        loop {
            if let Some(lock) =
                private_files::try_lock(&self.paths.data.join("service.lock")).unwrap()
            {
                drop(lock);
                break;
            }
            assert!(
                Instant::now() < deadline,
                "stopped service still owns its lock"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Whether the service returned on its own within `limit`.
    pub async fn stopped_within(&mut self, limit: Duration) -> bool {
        let server = self.server.as_mut().unwrap();
        let stopped = tokio::time::timeout(limit, server).await.is_ok();
        if stopped {
            self.server = None;
        }
        stopped
    }

    pub fn set_load(&self, change: impl FnOnce(&mut LoadSample)) {
        let mut load = self.machine.0.lock().unwrap();
        change(&mut load);
        self.machine.1.store(0, std::sync::atomic::Ordering::SeqCst);
    }

    pub async fn wait_for_load_window(&self) {
        let deadline = Instant::now() + crate::processes::WAIT;
        while self.machine.1.load(std::sync::atomic::Ordering::SeqCst) < 3 {
            assert!(Instant::now() < deadline, "load samples did not arrive");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    pub fn path(&self, name: &str) -> PathBuf {
        self.home.path().join(name)
    }

    /// Runs the CLI and returns its exit code and JSON output.
    pub async fn cli(&self, args: &[&str]) -> (i32, Value) {
        let output = crate::processes::async_command(env!("CARGO_BIN_EXE_elsecall"))
            .env("HOME", self.home.path())
            .current_dir(self.home.path())
            .args(args)
            .stdin(Stdio::null())
            .output()
            .await
            .unwrap();
        let json = serde_json::from_slice(&output.stdout)
            .unwrap_or_else(|_| panic!("not JSON: {output:?}"));
        (output.status.code().unwrap_or(-1), json)
    }

    /// Starts `sh -c body` with `$EC` naming the CLI and `$DIR` the test home.
    /// Its own process group lets cleanup stop everything it started.
    pub fn script(&mut self, body: &str) -> u32 {
        let child = crate::processes::async_command("/bin/sh")
            .arg("-c")
            .arg(body)
            .env("HOME", self.home.path())
            .current_dir(self.home.path())
            .env("EC", env!("CARGO_BIN_EXE_elsecall"))
            .env("DIR", self.home.path())
            .stdin(Stdio::null())
            .process_group(0)
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let pid = child.id().unwrap();
        self.children.push(child);
        pid
    }

    /// Waits for the script started with `pid` and returns its exit code.
    pub async fn exit_code(&mut self, pid: u32) -> i32 {
        let child = self
            .children
            .iter_mut()
            .find(|c| c.id() == Some(pid))
            .unwrap();
        tokio::time::timeout(crate::processes::WAIT, child.wait())
            .await
            .expect("script finished")
            .unwrap()
            .code()
            .unwrap_or(-1)
    }

    pub async fn runs(&self) -> Value {
        let (code, json) = self.cli(&["runs", "--limit", "100"]).await;
        assert_eq!(code, 0, "{json}");
        json["data"].clone()
    }

    /// Polls `elsecall runs` until the predicate holds.
    pub async fn until(&self, predicate: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + crate::processes::WAIT;
        loop {
            let (code, json) = self.cli(&["runs", "--limit", "100"]).await;
            if code == 0 && predicate(&json["data"]) {
                return json["data"].clone();
            }
            assert!(Instant::now() < deadline, "condition not reached: {json}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    pub async fn wait_for_file(&self, name: &str) -> String {
        let path = self.path(name);
        let deadline = Instant::now() + crate::processes::WAIT;
        loop {
            if let Ok(text) = std::fs::read_to_string(&path)
                && text.ends_with('\n')
            {
                return text;
            }
            assert!(Instant::now() < deadline, "{name} never appeared");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    pub fn touch(&self, name: &str) {
        std::fs::write(self.path(name), "go\n").unwrap();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for child in &self.children {
            if let Some(pid) = child.id() {
                // SAFETY: kill has no memory-safety preconditions.
                unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
            }
        }
        if let Some(server) = &self.server {
            server.abort();
        }
    }
}

pub fn in_use(runs: &Value) -> f64 {
    runs["member"]["in_use"].as_f64().unwrap()
}
pub fn queued(runs: &Value, class: &str) -> u64 {
    runs["member"]["queued"][class].as_u64().unwrap()
}
pub fn record<'a>(runs: &'a Value, label: &str) -> &'a Value {
    runs["runs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["label"] == label)
        .unwrap_or_else(|| panic!("no record labelled {label}: {runs}"))
}

pub fn read_all(paths: &[&Path]) -> Vec<u8> {
    paths
        .iter()
        .filter_map(|p| std::fs::read(p).ok())
        .flatten()
        .collect()
}
