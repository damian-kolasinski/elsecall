use crate::{
    failure::{Failure, Result},
    installation::{ServiceManager, Session},
    local_access::user_id,
    paths::{LABEL, Paths},
};
use std::{
    path::Path,
    process::{Command, Stdio},
};
/// A per-user LaunchAgent. The desktop session uses the `gui/<uid>` domain, which
/// exists while the user is logged in to the desktop. The background session uses the
/// `user/<uid>` domain with `LimitLoadToSessionType` `Background`, which runs without a
/// desktop login and survives ssh disconnects.
pub struct LaunchAgent;
const SESSIONS: [Session; 2] = [Session::Desktop, Session::Background];
fn domain(session: Session) -> String {
    match session {
        Session::Desktop => format!("gui/{}", user_id()),
        Session::Background => format!("user/{}", user_id()),
    }
}
fn target(session: Session) -> String {
    format!("{}/{LABEL}", domain(session))
}
impl LaunchAgent {
    fn loaded_in(&self, session: Session) -> Result<bool> {
        match self.launchctl(&["print".as_ref(), target(session).as_ref()])? {
            Some(0) => Ok(true),
            // 113: no such service. 112 and 125: no such domain, e.g. no desktop login.
            Some(112 | 113 | 125) => Ok(false),
            _ => Err(Failure::new(
                "service_manager_error",
                "Cannot inspect the Elsecall LaunchAgent.",
                "Run elsecall doctor; see README diagnostics.",
            )),
        }
    }
    fn launchctl(&self, args: &[&std::ffi::OsStr]) -> Result<Option<i32>> {
        let status = Command::new("/bin/launchctl")
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|e| Failure::io("Run launchctl", e))?;
        Ok(status.code())
    }
}

fn launchctl_failure(code: Option<i32>) -> Failure {
    Failure::new(
        "service_manager_error",
        format!("launchctl failed with exit code {}.", code.unwrap_or(-1)),
        "Run elsecall doctor; see README diagnostics for inspecting the registration.",
    )
}

/// launchctl's exit code when the job is still being torn down (EINPROGRESS).
const IN_PROGRESS: i32 = 36;

impl ServiceManager for LaunchAgent {
    fn configuration(&self, paths: &Paths, session: Session) -> Result<Vec<u8>> {
        configuration(paths, session)
    }
    fn configured_session(&self, configuration: &[u8]) -> Option<Session> {
        let value = plist::Value::from_reader_xml(configuration).ok()?;
        let config = value.as_dictionary()?;
        match config.get("LimitLoadToSessionType") {
            None => Some(Session::Desktop),
            Some(kind) => match kind.as_string()? {
                "Aqua" => Some(Session::Desktop),
                "Background" => Some(Session::Background),
                _ => None,
            },
        }
    }
    fn desktop_available(&self) -> Result<bool> {
        Ok(self.launchctl(&["print".as_ref(), domain(Session::Desktop).as_ref()])? == Some(0))
    }
    fn loaded(&self) -> Result<bool> {
        for session in SESSIONS {
            if self.loaded_in(session)? {
                return Ok(true);
            }
        }
        Ok(false)
    }
    fn start(&self, plist: &Path, session: Session) -> Result<()> {
        match self.launchctl(&[
            "bootstrap".as_ref(),
            domain(session).as_ref(),
            plist.as_os_str(),
        ])? {
            Some(0) => Ok(()),
            code => Err(launchctl_failure(code)),
        }
    }
    fn stop(&self) -> Result<()> {
        for session in SESSIONS {
            if !self.loaded_in(session)? {
                continue;
            }
            match self.launchctl(&["bootout".as_ref(), target(session).as_ref()])? {
                Some(0 | IN_PROGRESS) => (),
                code => return Err(launchctl_failure(code)),
            }
        }
        // bootout can return before launchd deregisters the job, and bootstrap fails
        // until it has. Callers restart right after stopping, so wait here.
        for _ in 0..100 {
            if !self.loaded()? {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        Err(Failure::new(
            "service_manager_error",
            "launchd did not unload the Elsecall LaunchAgent within ten seconds.",
            "Run elsecall doctor and inspect the service log, then retry.",
        ))
    }
}
fn configuration(paths: &Paths, session: Session) -> Result<Vec<u8>> {
    let mut values = plist::Dictionary::new();
    values.insert("Label".into(), LABEL.into());
    if session == Session::Background {
        values.insert("LimitLoadToSessionType".into(), "Background".into());
    }
    values.insert(
        "ProgramArguments".into(),
        plist::Value::Array(vec![
            paths.executable().to_string_lossy().into_owned().into(),
            "serve".into(),
        ]),
    );
    values.insert("RunAtLoad".into(), true.into());
    values.insert("KeepAlive".into(), true.into());
    values.insert("ThrottleInterval".into(), 10i64.into());
    // Without a process type, launchd throttles the job's CPU and I/O: the service
    // and the commands it runs for other members, which inherit its class, would
    // run below the work they coordinate and stall behind its disk traffic.
    values.insert("ProcessType".into(), "Interactive".into());
    values.insert("Umask".into(), 0o077i64.into());
    values.insert("StandardOutPath".into(), "/dev/null".into());
    values.insert("StandardErrorPath".into(), "/dev/null".into());
    let mut environment = plist::Dictionary::new();
    environment.insert(
        "HOME".into(),
        paths.home.to_string_lossy().into_owned().into(),
    );
    values.insert(
        "EnvironmentVariables".into(),
        plist::Value::Dictionary(environment),
    );
    let mut bytes = Vec::new();
    plist::Value::Dictionary(values)
        .to_writer_xml(&mut bytes)
        .map_err(|_| {
            Failure::new(
                "configuration_error",
                "Could not encode LaunchAgent configuration.",
                "Rebuild Elsecall and retry.",
            )
        })?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn configuration_preserves_paths_and_defines_login_lifecycle() {
        let paths = Paths::for_home(Path::new("/Users/a & b")).unwrap();
        let bytes = configuration(&paths, Session::Desktop).unwrap();
        assert_eq!(
            LaunchAgent.configured_session(&bytes),
            Some(Session::Desktop)
        );
        let value = plist::Value::from_reader_xml(bytes.as_slice()).unwrap();
        let config = value.as_dictionary().unwrap();
        assert_eq!(config["RunAtLoad"].as_boolean(), Some(true));
        assert_eq!(config["KeepAlive"].as_boolean(), Some(true));
        assert_eq!(config["ProcessType"].as_string(), Some("Interactive"));
        assert_eq!(config["Umask"].as_unsigned_integer(), Some(0o077));
        let args = config["ProgramArguments"].as_array().unwrap();
        assert_eq!(args[0].as_string(), paths.executable().to_str());
        assert_eq!(args[1].as_string(), Some("serve"));
        let env = config["EnvironmentVariables"].as_dictionary().unwrap();
        assert_eq!(env.len(), 1);
        assert_eq!(env["HOME"].as_string(), paths.home.to_str());
        assert!(
            !config.contains_key("LimitLoadToSessionType"),
            "unchanged desktop configuration"
        );
    }

    #[test]
    fn background_configuration_runs_without_a_desktop_login() {
        let paths = Paths::for_home(Path::new("/Users/devbox")).unwrap();
        let bytes = configuration(&paths, Session::Background).unwrap();
        assert_eq!(
            LaunchAgent.configured_session(&bytes),
            Some(Session::Background)
        );
        let value = plist::Value::from_reader_xml(bytes.as_slice()).unwrap();
        let config = value.as_dictionary().unwrap();
        assert_eq!(
            config["LimitLoadToSessionType"].as_string(),
            Some("Background")
        );
        assert_eq!(config["KeepAlive"].as_boolean(), Some(true));
        assert_eq!(config["ProcessType"].as_string(), Some("Interactive"));
        assert_eq!(LaunchAgent.configured_session(b"not a plist"), None);
    }
}
