//! The SSH adapter against a real Mac, such as a Tart VM (docs/e2e-vm.md). It
//! changes that Mac, so it runs only when `ELSECALL_E2E_SSH` names one as
//! `account@host` and `ELSECALL_E2E_SSH_PASSWORD_FILE` holds the password.
#![cfg(target_os = "macos")]
use elsecall_add_mac::remote_mac::{Build, RemoteMac};
use elsecall_openssh_mac::OpensshMac;
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

    let refused = OpensshMac::connect(host, account, Zeroizing::new("wrong".into()), elsecall)
        .await
        .err()
        .unwrap();
    assert_eq!(refused.code, "ssh_password_refused");

    let mut mac = OpensshMac::connect(
        host,
        account,
        Zeroizing::new(password.trim().into()),
        elsecall,
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

    let executable = format!(
        "{}/Library/Application Support/Elsecall/elsecall",
        facts.home
    );
    let before = mac.incoming(&executable).await.unwrap();
    eprintln!("firewall: {before:?}");
    assert_ne!(before, elsecall_add_mac::inbound_firewall::Rule::Unknown);
    mac.allow_incoming(std::slice::from_ref(&executable))
        .await
        .unwrap();
    let line = elsecall_add_mac::authorized_keys::line(
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIExampleMasterKey me@example.com",
        "mac-mini",
    )
    .unwrap();
    assert!(mac.authorize_key(&line).await.unwrap());
    assert!(!mac.authorize_key(&line).await.unwrap(), "added once");

    // Tailscale, when asked to: it stays installed and waiting for a login.
    if std::env::var_os("ELSECALL_E2E_TAILSCALE").is_some() {
        assert_eq!(tailnet, elsecall_add_mac::remote_mac::RemoteTailnet::Absent);
        let bottle =
            elsecall_tailnet::tailscale_bottle::for_macos(facts.macos_major().unwrap()).unwrap();
        mac.install_tailscale(&bottle).await.unwrap();
        assert_eq!(
            mac.tailnet().await.unwrap(),
            elsecall_add_mac::remote_mac::RemoteTailnet::LoggedOut {
                variant: elsecall_add_mac::remote_mac::TailscaleVariant::AddMacDaemon,
                domain: None,
            }
        );
        let login = mac
            .start_tailscale_login(Some("macbook-pro"))
            .await
            .unwrap();
        assert!(
            matches!(&login, elsecall_add_mac::remote_mac::Login::Approve(link) if link.as_str().starts_with("https://login.tailscale.com/")),
            "{login:?}"
        );
        // Asking again offers a link again rather than failing.
        assert!(matches!(
            mac.start_tailscale_login(None).await.unwrap(),
            elsecall_add_mac::remote_mac::Login::Approve(_)
        ));
        // Installing again repairs rather than failing.
        mac.install_tailscale(&bottle).await.unwrap();
    }

    let uninstalled = mac.elsecall(&["uninstall", "--force"]).await.unwrap();
    assert_eq!(uninstalled["status"], "uninstalled", "{uninstalled}");
    assert_eq!(mac.installed_build().await.unwrap(), None);
}
