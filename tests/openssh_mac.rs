//! The SSH adapter against a real Mac, such as a Tart VM (docs/e2e-vm.md). It
//! changes that Mac, so it runs only when `ELSECALL_E2E_SSH` names one as
//! `account@host` and `ELSECALL_E2E_SSH_PASSWORD_FILE` holds the password.
#![cfg(target_os = "macos")]
use elsecall_fleet::remote_mac::{Build, RemoteMac};
use elsecall_macos::openssh_mac::OpensshMac;
use std::path::Path;
use zeroize::Zeroizing;

#[tokio::test(flavor = "multi_thread")]
async fn the_adapter_reads_installs_and_authorizes_on_a_real_mac() {
    let (Ok(target), Ok(password_file)) = (
        std::env::var("ELSECALL_E2E_SSH"),
        std::env::var("ELSECALL_E2E_SSH_PASSWORD_FILE"),
    ) else {
        eprintln!("skipping: ELSECALL_E2E_SSH and ELSECALL_E2E_SSH_PASSWORD_FILE are not set");
        return;
    };
    let (account, host) = target.split_once('@').unwrap();
    let password = std::fs::read_to_string(password_file).unwrap();
    let elsecall = Path::new(env!("CARGO_BIN_EXE_elsecall"));

    let mut public_key = None;
    let refused = OpensshMac::connect(
        host,
        account,
        Zeroizing::new("wrong".into()),
        elsecall,
        &mut public_key,
    )
    .await
    .err()
    .unwrap();
    assert_eq!(refused.code, "ssh_password_refused");
    assert!(
        public_key.is_some(),
        "a rejected sign-in still learns the host key"
    );

    let other = tempfile::tempdir().unwrap();
    let key_path = other.path().join("other");
    assert!(
        std::process::Command::new("/usr/bin/ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-f"])
            .arg(&key_path)
            .status()
            .unwrap()
            .success()
    );
    let other_key = std::fs::read_to_string(key_path.with_extension("pub")).unwrap();
    let other_key = other_key
        .splitn(3, ' ')
        .take(2)
        .collect::<Vec<_>>()
        .join(" ");
    let changed = OpensshMac::connect(
        host,
        account,
        Zeroizing::new("wrong".into()),
        elsecall,
        &mut Some(other_key),
    )
    .await
    .err()
    .unwrap();
    assert_eq!(
        changed.code, "host_key_changed",
        "the password is never tried against a different key"
    );

    let mut mac = OpensshMac::connect(
        host,
        account,
        Zeroizing::new(password.trim().into()),
        elsecall,
        &mut public_key,
    )
    .await
    .unwrap();
    assert!(mac.host_key().starts_with("SHA256:"), "{}", mac.host_key());
    let facts = mac.facts().await.unwrap();
    eprintln!("{facts:?}");
    assert_eq!(facts.architecture, "arm64");
    assert_eq!(facts.account, account);
    assert!(facts.administrator);
    assert!(mac.sudo_accepted().await.unwrap());
    // Whatever Tailscale the Mac has, it is read without failing.
    let tailnet = mac.tailnet().await.unwrap();
    eprintln!("{tailnet:?}");

    let build = Build {
        executable: elsecall.to_owned(),
        window: None,
    };
    let installed = mac.install(&build).await.unwrap();
    eprintln!("installed: {installed}");
    let wanted = elsecall_contract::build_identity::Digest::of_file(elsecall)
        .unwrap()
        .build_id()
        .to_string();
    assert_eq!(
        mac.installed_build().await.unwrap().as_deref(),
        Some(wanted.as_str())
    );
    let status = mac.elsecall(&["status"]).await.unwrap();
    assert_eq!(status["status"], "ready", "{status}");

    // Xcode's privileges, as Add Mac installs them: the master's script
    // through the session's sudo, then the Mac's own Elsecall reads them.
    let script = elsecall_tools::privileges_cli::install_script(account).unwrap();
    mac.install_xcode_privileges(&script).await.unwrap();
    let privileges = mac.elsecall(&["tools", "privileges"]).await.unwrap();
    assert_eq!(privileges["ready"], true, "{privileges}");
    assert_eq!(privileges["helper"], "current", "{privileges}");
    mac.install_xcode_privileges(&script)
        .await
        .expect("installing them again changes nothing");

    // The boot starter, installed once; the Mac's doctor then finds it.
    assert!(facts.uid > 0);
    let agent = elsecall_host::paths::Paths::for_home(Path::new(&facts.home))
        .unwrap()
        .agent;
    mac.start_at_boot(&facts.account, facts.uid, &agent)
        .await
        .unwrap();
    assert!(
        !mac.start_at_boot(&facts.account, facts.uid, &agent)
            .await
            .unwrap(),
        "installed once"
    );
    let doctor = mac.elsecall(&["doctor"]).await.unwrap();
    assert_eq!(doctor["boot_start"], "installed", "{doctor}");

    let executable = format!(
        "{}/Library/Application Support/Elsecall/elsecall",
        facts.home
    );
    let before = mac.incoming(&executable).await.unwrap();
    eprintln!("firewall: {before:?}");
    assert_ne!(before, elsecall_fleet::inbound_firewall::Rule::Unknown);
    mac.allow_incoming(std::slice::from_ref(&executable))
        .await
        .unwrap();
    let line = elsecall_fleet::authorized_keys::line(
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIExampleMasterKey me@example.com",
        "mac-mini",
    )
    .unwrap();
    assert!(mac.authorize_key(&line).await.unwrap());
    assert!(!mac.authorize_key(&line).await.unwrap(), "added once");

    // Tailscale, when asked to: it stays installed and waiting for a login.
    if std::env::var_os("ELSECALL_E2E_TAILSCALE").is_some() {
        assert_eq!(tailnet, elsecall_fleet::remote_mac::RemoteTailnet::Absent);
        let bottle =
            elsecall_peers::tailscale_bottle::for_macos(facts.macos_major().unwrap()).unwrap();
        mac.install_tailscale(&bottle).await.unwrap();
        assert_eq!(
            mac.tailnet().await.unwrap(),
            elsecall_fleet::remote_mac::RemoteTailnet::LoggedOut {
                variant: elsecall_fleet::remote_mac::TailscaleVariant::AddMacDaemon,
                domain: None,
            }
        );
        let login = mac
            .start_tailscale_login(Some("macbook-pro"))
            .await
            .unwrap();
        assert!(
            matches!(&login, elsecall_fleet::remote_mac::Login::Approve(link) if link.as_str().starts_with("https://login.tailscale.com/")),
            "{login:?}"
        );
        // Asking again offers a link again rather than failing.
        assert!(matches!(
            mac.start_tailscale_login(None).await.unwrap(),
            elsecall_fleet::remote_mac::Login::Approve(_)
        ));
        // Installing again repairs rather than failing.
        mac.install_tailscale(&bottle).await.unwrap();
    }

    let uninstalled = mac.elsecall(&["uninstall", "--force"]).await.unwrap();
    assert_eq!(uninstalled["status"], "uninstalled", "{uninstalled}");
    // The starter needs root to remove, so it stays and is named.
    assert_eq!(
        uninstalled["boot_start_kept"]["commands"][0],
        format!(
            "sudo launchctl bootout system/dev.elsecall.boot.{}",
            facts.account
        ),
        "{uninstalled}"
    );
    assert_eq!(mac.installed_build().await.unwrap(), None);
}
