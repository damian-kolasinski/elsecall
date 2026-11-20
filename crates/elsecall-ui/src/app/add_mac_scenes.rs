//! Standalone scenes for rendering and checking the Add Mac sheet.

use super::{AppState, scene_members};
use crate::add_mac_flow::{AddUpdate, mac};
use elsecall_contract::failure::Failure;
use elsecall_fleet::{
    add_mac::{Approval, Event, Step},
    remote_mac::LoginLink,
};

pub(super) fn scene_add_mac_prepare(state: &mut AppState) {
    scene_members(state);
    state.add_mac.open();
}

pub(super) fn scene_add_mac_looking(state: &mut AppState) {
    scene_add_mac_network(state);
    state.add_mac.show_choose();
}

pub(super) fn scene_add_mac_network(state: &mut AppState) {
    scene_add_mac_prepare(state);
    state.add_mac.show_network();
}

pub(super) fn scene_add_mac_choose(state: &mut AppState) {
    scene_add_mac_prepare(state);
    let mut macbook = mac("MacBook Pro", true, None);
    macbook.model = Some("MacBookPro18,4".into());
    state.add_mac.set_nearby(Ok(vec![
        macbook,
        mac("Studio", false, None),
        mac("Mac mini", true, Some("mac-mini")),
    ]));
    state.add_mac.show_choose();
}

pub(super) fn scene_add_mac_sign_in(state: &mut AppState) {
    scene_add_mac_choose(state);
    state.add_mac.show_sign_in();
    state.add_mac.account.replace_text("devbox");
    state.add_mac.password.replace_text("example");
}

pub(super) fn scene_add_mac_adding(state: &mut AppState) {
    scene_add_mac_sign_in(state);
    state.add_mac.start();
    for step in [Step::Check, Step::Tailnet, Step::Tools, Step::Install] {
        state
            .add_mac
            .apply_current(AddUpdate::Event(Event::Step { step }));
    }
}

pub(super) fn scene_add_mac_approve(state: &mut AppState) {
    scene_add_mac_sign_in(state);
    state.add_mac.start();
    for event in [
        Event::Step { step: Step::Check },
        Event::Step {
            step: Step::Tailnet,
        },
        Event::Approve {
            link: LoginLink::parse("https://login.tailscale.com/a/example")
                .expect("a Tailscale link"),
            approval: Approval::Login,
        },
    ] {
        state.add_mac.apply_current(AddUpdate::Event(event));
    }
}

pub(super) fn scene_add_mac_firewall(state: &mut AppState) {
    scene_add_mac_adding(state);
    for event in [
        Event::Step {
            step: Step::Firewall,
        },
        Event::FirewallApproval,
    ] {
        state.add_mac.apply_current(AddUpdate::Event(event));
    }
}

pub(super) fn scene_add_mac_firewall_failed(state: &mut AppState) {
    scene_add_mac_firewall(state);
    state.add_mac.apply_current(AddUpdate::Finished(Err(Failure::new(
        "firewall_unchanged",
        "The firewall on macbook-pro.local still does not allow /Users/devbox/Library/Application Support/Elsecall/elsecall.",
        "Click Allow on the new Mac's incoming connections prompt. If there is no prompt, allow the program in System Settings > Network > Firewall > Options, then try again.",
    ))));
}

pub(super) fn scene_add_mac_done(state: &mut AppState) {
    scene_add_mac_sign_in(state);
    state.add_mac.start();
    let report = serde_json::from_value(serde_json::json!({
        "member": "macbook-pro",
        "host": "macbook-pro.local",
        "mac": {
            "model": "Mac16,10",
            "serial_number": null,
            "macos_version": "26.1",
            "ssh_host_key": "SHA256:exampleHostKeyFingerprint (ED25519)"
        },
        "readiness": "awaiting_provisioning",
        "tailscale_key_expiry": null,
        "steps": [],
        "next_step": ""
    }))
    .expect("a report");
    state.add_mac.apply_current(AddUpdate::Finished(Ok(report)));
}

pub(super) fn scene_add_mac_failed(state: &mut AppState) {
    scene_add_mac_sign_in(state);
    state.add_mac.start();
    state.add_mac.apply_current(AddUpdate::Finished(Err(Failure::new(
        "ssh_password_refused",
        "macbook-pro.local refused the password for devbox.",
        "Check the account's short name and password on that Mac, and that Remote Login allows that account.",
    ))));
}
