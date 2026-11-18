//! `elsecall fleet nearby` through the system's DNS-SD against devices registered
//! on this Mac only (DNS-SD's local-only interface), so nothing reaches the network.
#![cfg(target_os = "macos")]
mod diagnostics;
mod members;
mod processes;

use members::TestMember;
use serde_json::Value;
use std::{
    process::{Child, Stdio},
    thread::sleep,
    time::{Duration, Instant},
};

/// Local-only DNS-SD registrations, withdrawn when dropped.
#[derive(Default)]
struct Advertised(Vec<Child>);

impl Advertised {
    fn register(&mut self, args: &[&str]) {
        let child = processes::command("/usr/bin/dns-sd")
            .arg("-lo")
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        self.0.push(child);
    }

    /// A device named `name` on host `host`, offering `service` and reporting
    /// `model`, as a Mac's `_device-info._tcp` record does.
    fn device(
        &mut self,
        name: &str,
        host: &str,
        address: &str,
        service: &str,
        port: &str,
        model: &str,
    ) {
        self.register(&["-P", name, service, "local", port, host, address]);
        // A zero port only reserves the name, so the record gets a placeholder.
        self.register(&[
            "-R",
            name,
            "_device-info._tcp",
            "local",
            "9",
            &format!("model={model}"),
        ]);
    }
}

impl Drop for Advertised {
    fn drop(&mut self) {
        for child in &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn ours<'a>(macs: &'a Value, tag: &str) -> Vec<&'a Value> {
    macs.as_array()
        .unwrap()
        .iter()
        .filter(|mac| mac["name"].as_str().unwrap().contains(tag))
        .collect()
}

#[test]
fn nearby_lists_macs_whether_remote_login_is_on_and_which_are_members() {
    let member = TestMember::start("nearby-member");
    let tag = format!("ec{}", std::process::id());
    let mut advertised = Advertised::default();
    advertised.device(
        &format!("{tag} New Mac"),
        &format!("{tag}-new.local"),
        "127.0.0.1",
        "_ssh._tcp",
        "2222",
        "Mac16,10",
    );
    advertised.device(
        &format!("{tag} Studio"),
        &format!("{tag}-studio.local"),
        "127.0.0.2",
        "_companion-link._tcp",
        "49152",
        "Mac14,13",
    );
    advertised.device(
        &format!("{tag} Phone"),
        &format!("{tag}-phone.local"),
        "127.0.0.3",
        "_companion-link._tcp",
        "49153",
        "iPhone17,1",
    );
    // A member is recognised by its host name and an address it reports: this
    // Mac's own, which the test member's machine inspection reports.
    let own = elsecall::macos::lan_addresses::lan_addresses()
        .into_iter()
        .find(std::net::IpAddr::is_ipv4)
        .expect("this Mac has a local network address");
    advertised.device(
        &format!("{tag} Member"),
        "nearby-member.local",
        &own.to_string(),
        "_ssh._tcp",
        "22",
        "Mac16,10",
    );

    let new = format!("{tag} New Mac");
    let new_host = format!("{tag}-new.local");
    let studio = format!("{tag} Studio");
    let studio_host = format!("{tag}-studio.local");
    let member_name = format!("{tag} Member");
    let expected = serde_json::json!([
        [new, new_host, "on", "not_member", null, "Mac16,10"],
        [studio, studio_host, "off", "not_member", null, "Mac14,13"],
        [
            member_name,
            "nearby-member.local",
            "on",
            "member",
            "nearby-member",
            "Mac16,10"
        ],
    ]);
    let summary = |data: &Value| -> Value {
        ours(&data["macs"], &tag)
            .iter()
            .map(|mac| {
                serde_json::json!([
                    mac["name"],
                    mac["host"],
                    mac["remote_login"],
                    mac["membership"],
                    mac["member"],
                    mac["model"]
                ])
            })
            .collect()
    };
    // Registrations take a moment to appear; wait for all of them, models included.
    let deadline = Instant::now() + processes::WAIT;
    let data = loop {
        let data = member.ok(&["fleet", "nearby", "--browse", "1s"]);
        if summary(&data) == expected || Instant::now() > deadline {
            break data;
        }
        sleep(Duration::from_millis(200));
    };
    assert_eq!(summary(&data), expected, "{data}");
    let found = ours(&data["macs"], &tag);
    assert_eq!(found[0]["ssh_port"], 2222);
    assert_eq!(found[0]["addresses"], serde_json::json!(["127.0.0.1"]));
    assert!(found[1].get("ssh_port").is_none());
    assert!(
        data["next_step"]
            .as_str()
            .unwrap()
            .contains("elsecall fleet add"),
        "{data}"
    );
}
