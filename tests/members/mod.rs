//! Real member services in separate temporary homes on this machine. External
//! machine observations and tailnet discovery are controlled: each member
//! "discovers" loopback and its own port, with scripted load and speed.
#![allow(dead_code)]
use elsecall::{member_key::MemberKey, paths::Paths, roster::Member};
use serde_json::Value;
use std::{
    io::{Read, Write},
    net::TcpStream,
    path::Path,
    process::{Child, Output},
    thread::sleep,
    time::{Duration, Instant},
};

pub struct TestMember {
    pub name: &'static str,
    pub port: u16,
    pub home: tempfile::TempDir,
    service: Option<Child>,
    /// Extra environment for the service, such as a PATH for placed work.
    environment: Vec<(String, String)>,
    /// Installed with `elsecall install` from this executable, running as a plain
    /// background process (`ELSECALL_SERVICE_MANAGER=process`) instead of a child.
    installed_from: Option<std::path::PathBuf>,
    /// When false, the service is not given a configured tailnet presence.
    tailnet: bool,
}

/// The environment every installed test member's install, service and CLI share.
fn installed_environment(member: &TestMember) -> Vec<(String, String)> {
    let mut environment = vec![
        ("ELSECALL_SERVICE_MANAGER".into(), "process".into()),
        ("ELSECALL_TAILNET_NAME".into(), member.name.into()),
        ("ELSECALL_TAILNET_ADDRESSES".into(), "127.0.0.1".into()),
        ("ELSECALL_FLEET_PORT".into(), member.port.to_string()),
        ("ELSECALL_MACHINE_LOAD".into(), "idle".into()),
        ("ELSECALL_MACHINE_SPEED".into(), "1".into()),
    ];
    environment.extend(member.environment.iter().cloned());
    environment
}

/// A free port below the ephemeral range, so a stopped member's port is not taken by
/// another connection's ephemeral source port before the member restarts.
pub fn free_port() -> u16 {
    use std::sync::atomic::{AtomicU16, Ordering};
    static NEXT: AtomicU16 = AtomicU16::new(0);
    let base = 20_000 + (std::process::id() % 100) as u16 * 200;
    loop {
        let port = base + NEXT.fetch_add(1, Ordering::Relaxed) % 20_000;
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return port;
        }
    }
}

pub fn cli(home: &Path, args: &[&str]) -> Output {
    crate::processes::command(env!("CARGO_BIN_EXE_elsecall"))
        .env("HOME", home)
        .current_dir(home)
        .args(args)
        .output()
        .unwrap()
}

pub fn json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|_| panic!("not JSON: {output:?}"))
}

impl TestMember {
    pub fn start(name: &'static str) -> Self {
        Self::start_with(name, "", &[])
    }

    /// Starts a member whose service has `configuration` as its config.toml and
    /// `environment` added to its own.
    pub fn start_with(
        name: &'static str,
        configuration: &str,
        environment: &[(&str, &str)],
    ) -> Self {
        Self::launch(name, configuration, environment, true)
    }

    /// Starts a solo service with no `ELSECALL_TAILNET_NAME` or
    /// `ELSECALL_TAILNET_ADDRESSES`. Readiness is the local status route,
    /// because `fleet identity` needs a tailnet presence.
    pub fn start_without_tailnet(name: &'static str, environment: &[(&str, &str)]) -> Self {
        Self::launch(name, "", environment, false)
    }

    fn launch(
        name: &'static str,
        configuration: &str,
        environment: &[(&str, &str)],
        tailnet: bool,
    ) -> Self {
        // This process talks to members over loopback too.
        elsecall::tailnet::allow_loopback_members();
        let home = tempfile::Builder::new()
            .prefix("ec-")
            .tempdir_in("/tmp")
            .unwrap();
        let data = Paths::for_home(home.path()).unwrap().data;
        elsecall::private_files::directory(&data).unwrap();
        std::fs::write(data.join("config.toml"), configuration).unwrap();
        let mut member = Self {
            name,
            port: free_port(),
            home,
            service: None,
            environment: environment
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            installed_from: None,
            tailnet,
        };
        // Other processes on this machine may take a port between choosing and
        // binding it; a member that has not enrolled yet can simply move.
        for _ in 0..5 {
            if member.try_restart() {
                return member;
            }
            member.port = free_port();
        }
        panic!("{name} did not start: {}", member.log());
    }

    /// Installs a member with `executable`'s own `install` command, as an operator
    /// would, so its service runs the installed copy and can be upgraded.
    pub fn install(name: &'static str, executable: &Path) -> Self {
        Self::install_with(name, executable, "", &[])
    }

    /// Installs a member as `install` does, whose service has `configuration` as its
    /// config.toml and `environment` added to its own.
    pub fn install_with(
        name: &'static str,
        executable: &Path,
        configuration: &str,
        environment: &[(&str, &str)],
    ) -> Self {
        elsecall::tailnet::allow_loopback_members();
        let home = tempfile::Builder::new()
            .prefix("ec-")
            .tempdir_in("/tmp")
            .unwrap();
        if !configuration.is_empty() {
            let data = Paths::for_home(home.path()).unwrap().data;
            elsecall::private_files::directory(&data).unwrap();
            std::fs::write(data.join("config.toml"), configuration).unwrap();
        }
        let mut member = Self {
            name,
            port: free_port(),
            home,
            service: None,
            environment: environment
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            installed_from: Some(executable.to_owned()),
            tailnet: true,
        };
        for _ in 0..5 {
            if member.try_restart() {
                return member;
            }
            member.port = free_port();
        }
        panic!("{name} did not install: {}", member.log());
    }

    /// The installed service's state directory.
    pub fn data(&self) -> std::path::PathBuf {
        Paths::for_home(self.home.path()).unwrap().data
    }

    /// The installed service's process ID, from the process service manager.
    pub fn installed_pid(&self) -> Option<i32> {
        std::fs::read_to_string(self.data().join("process-service.pid"))
            .ok()?
            .trim()
            .parse()
            .ok()
    }

    pub fn restart(&mut self) {
        assert!(
            self.try_restart(),
            "{} did not start: {}",
            self.name,
            self.log()
        );
    }

    /// Starts the service; false if its port was taken.
    fn try_restart(&mut self) -> bool {
        self.spawn();
        // A service reads its whole executable at start to identify its build; dozens
        // of debug builds starting at once on a loaded machine take seconds.
        let deadline = Instant::now() + crate::processes::WAIT;
        while !self.ready() {
            if self.log().contains("bind_failed") {
                self.stop();
                return false;
            }
            if let Some(child) = self.service.as_mut()
                && let Some(status) = child.try_wait().unwrap()
            {
                panic!("{} exited with {status}: {}", self.name, self.log());
            }
            assert!(
                Instant::now() < deadline,
                "{} did not start: {}",
                self.name,
                self.log()
            );
            sleep(Duration::from_millis(20));
        }
        true
    }

    fn ready(&self) -> bool {
        let args = if self.tailnet {
            ["fleet", "identity"].as_slice()
        } else {
            ["status"].as_slice()
        };
        self.run(args).status.success()
    }

    /// Restarts a member whose fleet state is broken: only the local API comes up.
    pub fn restart_without_fleet(&mut self) {
        self.spawn();
        let deadline = Instant::now() + crate::processes::WAIT;
        while !self.run(&["status"]).status.success() {
            if let Some(child) = self.service.as_mut()
                && let Some(status) = child.try_wait().unwrap()
            {
                panic!("{} exited with {status}: {}", self.name, self.log());
            }
            assert!(
                Instant::now() < deadline,
                "{} did not start: {}",
                self.name,
                self.log()
            );
            sleep(Duration::from_millis(20));
        }
    }

    fn spawn(&mut self) {
        self.stop();
        if let Some(executable) = &self.installed_from {
            let output = crate::processes::command(executable)
                .env("HOME", self.home.path())
                .current_dir(self.home.path())
                .envs(installed_environment(self))
                .arg("install")
                .output()
                .unwrap();
            if !output.status.success() {
                eprintln!("{} install: {output:?}", self.name);
            }
            return;
        }
        // Foreground fixtures historically expose only this launch's records.
        // Do not let an earlier port collision poison a later startup retry.
        elsecall::private_files::write(&self.data().join("service.log"), b"", 0o600).unwrap();
        // A service that cannot start prints its error envelope on stdout.
        let log = std::fs::File::create(self.home.path().join("serve.log")).unwrap();
        let mut command = crate::processes::command(
            self.environment
                .iter()
                .find(|(name, _)| name == "EC_TEST_BINARY")
                .map(|(_, path)| path.as_str())
                .unwrap_or(env!("CARGO_BIN_EXE_elsecall")),
        );
        command
            .env("HOME", self.home.path())
            .env("ELSECALL_FLEET_PORT", self.port.to_string())
            // The machine's real load comes from unrelated work.
            .env("ELSECALL_MACHINE_LOAD", "idle")
            // Calibration has its own real-provider test. Starting many
            // high-priority benchmarks competes with peer I/O, not the
            // membership behavior these fixtures test.
            .env("ELSECALL_MACHINE_SPEED", "1")
            .envs(self.environment.iter().map(|(k, v)| (k, v)));
        if self.tailnet {
            command
                .env("ELSECALL_TAILNET_NAME", self.name)
                .env("ELSECALL_TAILNET_ADDRESSES", "127.0.0.1");
        }
        self.service = Some(
            command
                .arg("serve")
                .stdout(log.try_clone().unwrap())
                .stderr(log)
                .spawn()
                .unwrap(),
        );
    }

    /// The service's process ID.
    pub fn service_pid(&self) -> u32 {
        self.service.as_ref().expect("running").id()
    }

    /// Asks the service to stop, as launchd does, and says whether it exited
    /// within `limit`.
    pub fn terminate_within(&mut self, limit: Duration) -> bool {
        let child = self.service.as_mut().expect("running");
        // SAFETY: kill has no memory-safety preconditions.
        unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
        let deadline = Instant::now() + limit;
        while Instant::now() < deadline {
            if child.try_wait().unwrap().is_some() {
                self.service = None;
                return true;
            }
            sleep(Duration::from_millis(20));
        }
        false
    }

    /// Kills the service abruptly, as a crash or a powered-off machine would.
    pub fn stop(&mut self) {
        if let Some(mut child) = self.service.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if self.installed_from.is_some()
            && let Some(pid) = self.installed_pid()
        {
            // SAFETY: kill has no memory-safety preconditions.
            unsafe { libc::kill(pid, libc::SIGKILL) };
            let _ = std::fs::remove_file(self.data().join("process-service.pid"));
        }
    }

    /// The service's lifecycle log, for diagnosing a failed start.
    pub fn log(&self) -> String {
        let mut log = std::fs::read_to_string(self.data().join("service.log")).unwrap_or_default();
        // Early failures can precede initialization of the service-owned log.
        log.push_str(
            &std::fs::read_to_string(self.home.path().join("serve.log")).unwrap_or_default(),
        );
        log
    }

    pub fn run(&self, args: &[&str]) -> Output {
        if self.installed_from.is_none() {
            return cli(self.home.path(), args);
        }
        crate::processes::command(env!("CARGO_BIN_EXE_elsecall"))
            .env("HOME", self.home.path())
            .current_dir(self.home.path())
            .envs(installed_environment(self))
            .args(args)
            .output()
            .unwrap()
    }

    /// Runs a command that must succeed and returns its data.
    pub fn ok(&self, args: &[&str]) -> Value {
        let output = self.run(args);
        let value = json(&output);
        assert!(output.status.success(), "{} {args:?}: {value}", self.name);
        value["data"].clone()
    }

    /// Runs a command that must fail and returns its error code.
    pub fn fails(&self, args: &[&str]) -> String {
        let output = self.run(args);
        let value = json(&output);
        assert_eq!(
            output.status.code(),
            Some(1),
            "{} {args:?}: {value}",
            self.name
        );
        value["error"]["code"]
            .as_str()
            .unwrap_or("<none>")
            .to_owned()
    }

    pub fn code(&self) -> String {
        self.ok(&["fleet", "identity"])["code"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    /// The member's private key, read the way only its own account could.
    pub fn key(&self) -> MemberKey {
        MemberKey::load_or_create(&Paths::for_home(self.home.path()).unwrap().member_key()).unwrap()
    }

    pub fn as_member(&self) -> Member {
        elsecall::member_code::decode(&self.code()).unwrap()
    }

    /// The fleet as this member sees it, keyed by member name.
    pub fn fleet(&self) -> Value {
        self.ok(&["fleet"])
    }

    pub fn view_of(&self, other: &str) -> Value {
        self.fleet()["members"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["name"] == other)
            .cloned()
            .unwrap_or(Value::Null)
    }

    /// Sends raw HTTP to the member listener and returns the status code and body.
    pub fn raw(&self, request: &str) -> (u16, String) {
        let mut stream = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        let status = response[9..12].parse().unwrap();
        let body = response.split("\r\n\r\n").nth(1).unwrap_or("").to_owned();
        (status, body)
    }
}
impl Drop for TestMember {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Enrols `joining` on `sponsor` and completes the join on `joining`.
pub fn enrol(sponsor: &TestMember, joining: &TestMember) {
    let report = sponsor.ok(&["fleet", "enroll", &joining.code()]);
    let next = report["next_step"].as_str().unwrap();
    let code = next.rsplit(' ').next().unwrap();
    joining.ok(&["fleet", "join", code]);
}
