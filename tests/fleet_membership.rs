#![cfg(target_os = "macos")]
//! Enrolment, removal and authentication between real member services.
mod members;
mod processes;
use elsecall::{fleet::PeerStatus, member_key::MemberKey, peer_auth, peer_client, roster::Member};
use hyper::Method;
use members::{TestMember, enrol};
use std::time::Duration;

fn names(view: &serde_json::Value) -> Vec<String> {
    let mut names: Vec<String> = view["members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["name"].as_str().unwrap().to_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn enrolment_and_removal_take_a_step_on_each_machine() {
    let a = TestMember::start("alpha");
    let b = TestMember::start("bravo");
    let c = TestMember::start("charlie");

    // A machine that has joined nothing is the master of a fleet of one.
    let solo = a.fleet();
    assert_eq!(
        (solo["enrolled"].as_bool(), solo["master"].as_str()),
        (Some(false), Some("alpha"))
    );
    assert_eq!(a.view_of("alpha")["reachability"], "reachable");

    // Enrolling records the member on the sponsor; joining records the fleet on it.
    let report = a.ok(&["fleet", "enroll", &b.code()]);
    assert_eq!(report["members"], serde_json::json!(["alpha", "bravo"]));
    assert!(
        report["next_step"]
            .as_str()
            .unwrap()
            .starts_with("On bravo, run: elsecall fleet join ")
    );
    assert_eq!(
        b.fleet()["enrolled"],
        false,
        "nothing changes on bravo until it joins"
    );
    let code = report["next_step"]
        .as_str()
        .unwrap()
        .rsplit(' ')
        .next()
        .unwrap()
        .to_owned();
    b.ok(&["fleet", "join", &code]);

    for member in [&a, &b] {
        let view = member.fleet();
        assert_eq!(view["master"], "alpha", "this Mac starts as master");
        assert_eq!(names(&view), ["alpha", "bravo"]);
        for other in ["alpha", "bravo"] {
            let entry = member.view_of(other);
            assert_eq!(entry["reachability"], "reachable", "{entry}");
            assert_eq!(entry["in_sync"], true);
            assert_eq!(entry["status"]["machine"]["os"], "macos");
        }
    }
    assert_eq!(b.view_of("alpha")["role"], "master");
    assert_eq!(b.view_of("bravo")["role"], "member");

    // A third member, enrolled from a non-master member, reaches every roster.
    enrol(&b, &c);
    for member in [&a, &b, &c] {
        assert_eq!(names(&member.fleet()), ["alpha", "bravo", "charlie"]);
    }
    // Enrolling again re-delivers the same roster without a new epoch.
    let again = a.ok(&["fleet", "enroll", &c.code()]);
    assert_eq!(again["epoch"], 2);
    assert!(again["complete"].as_bool().unwrap());

    // Removing charlie updates every remaining member; charlie then gets nothing.
    assert_eq!(a.fails(&["fleet", "remove", "alpha"]), "cannot_remove_self");
    assert_eq!(b.fails(&["fleet", "remove", "alpha"]), "member_is_master");
    assert_eq!(a.fails(&["fleet", "remove", "nobody"]), "not_a_member");
    let removed = a.ok(&["fleet", "remove", "charlie"]);
    assert_eq!(
        removed["next_step"],
        "On charlie, run: elsecall fleet leave"
    );
    assert_eq!(removed["deliveries"][0]["outcome"], "updated");
    for member in [&a, &b] {
        assert_eq!(names(&member.fleet()), ["alpha", "bravo"]);
    }
    for other in ["alpha", "bravo"] {
        let entry = c.view_of(other);
        assert_eq!(entry["reachability"], "unreachable");
        assert_eq!(entry["error"]["code"], "member_rejected", "{entry}");
    }
    let left = c.ok(&["fleet", "leave"]);
    assert_eq!(left["status"], "solo");
    assert_eq!(names(&c.fleet()), ["charlie"]);

    // Membership survives a restart.
    let mut b = b;
    b.restart();
    assert_eq!(b.view_of("alpha")["reachability"], "reachable");
}

#[test]
fn joining_requires_enrolment_on_the_other_side() {
    let a = TestMember::start("alpha");
    let b = TestMember::start("bravo");
    assert_eq!(b.fails(&["fleet", "join", &a.code()]), "member_rejected");
    assert_eq!(b.fails(&["fleet", "join", &b.code()]), "cannot_join_self");
    assert_eq!(
        a.fails(&["fleet", "enroll", &a.code()]),
        "cannot_enrol_self"
    );
    assert_eq!(
        a.fails(&["fleet", "enroll", "elsecall-member-v1.garbage"]),
        "invalid_member_code"
    );
    // A different machine reusing an enrolled member's name is refused.
    enrol(&a, &b);
    let impostor = TestMember::start("bravo");
    assert_eq!(
        a.fails(&["fleet", "enroll", &impostor.code()]),
        "member_conflict"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tailnet_requests_need_an_enrolled_unforged_unreplayed_signature() {
    let a = TestMember::start("alpha");
    let b = TestMember::start("bravo");
    let outsider = TestMember::start("delta");
    enrol(&a, &b);
    let alpha: Member = a.as_member();
    let status = |key: MemberKey| {
        let alpha = alpha.clone();
        async move {
            peer_client::call::<PeerStatus>(
                &key,
                &alpha,
                Method::GET,
                "/peer/v1/status",
                None::<&()>,
                Duration::from_secs(2),
            )
            .await
        }
    };

    // An enrolled member's signed request is answered with a verified response.
    let answer = status(b.key()).await.unwrap();
    assert_eq!(
        (answer.member.as_str(), answer.roster.master().name.as_str()),
        ("alpha", "alpha")
    );
    assert_eq!(answer.machine.unwrap()["os"], "macos");

    // Unenrolled and freshly generated keys get nothing.
    assert_eq!(
        status(outsider.key()).await.unwrap_err().code,
        "member_rejected"
    );
    assert_eq!(
        status(MemberKey::generate().unwrap())
            .await
            .unwrap_err()
            .code,
        "member_rejected"
    );

    // Unsigned requests and unknown routes get an empty 401, never data or errors.
    let (code, body) =
        a.raw("GET /peer/v1/status HTTP/1.1\r\nHost: a\r\nConnection: close\r\n\r\n");
    assert_eq!((code, body.as_str()), (401, ""));
    let (code, body) =
        a.raw("GET /peer/v1/roster/secret HTTP/1.1\r\nHost: a\r\nConnection: close\r\n\r\n");
    assert_eq!((code, body.as_str()), (401, ""));
    // A request without a proof is refused before its body is sent.
    let (code, _) = a.raw(
        "PUT /peer/v1/roster HTTP/1.1\r\nHost: a\r\nContent-Length: 60000\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(code, 401);

    // A forged request claims bravo's enrolled key but is signed by another key.
    let bravo = b.key().public();
    let rogue = MemberKey::generate().unwrap();
    let signed = |key: &MemberKey, nonce: [u8; 16]| {
        peer_auth::Request {
            recipient: &alpha.key,
            method: "GET",
            path: "/peer/v1/status",
            body: b"",
        }
        .sign(key, peer_auth::now(), nonce)
    };
    let to_http = |proof: &peer_auth::RequestProof| {
        let headers: String = proof
            .headers()
            .iter()
            .map(|(n, v)| format!("{n}: {v}\r\n"))
            .collect();
        format!("GET /peer/v1/status HTTP/1.1\r\nHost: a\r\n{headers}Connection: close\r\n\r\n")
    };
    let mut forged = signed(&rogue, [1; 16]);
    forged.sender = bravo;
    assert_eq!(a.raw(&to_http(&forged)).0, 401);

    // A genuine request works once; replaying it, or redirecting it to another
    // member, gets nothing.
    let genuine = signed(&b.key(), [2; 16]);
    let (code, body) = a.raw(&to_http(&genuine));
    assert_eq!(code, 200, "{body}");
    assert_eq!(a.raw(&to_http(&genuine)).0, 401, "replayed");
    let (code, _) = b.raw(&to_http(&signed(&a.key(), [3; 16])));
    assert_eq!(code, 401, "signed for alpha, sent to bravo");

    // A removed member's key stops working at once.
    a.ok(&["fleet", "remove", "bravo"]);
    assert_eq!(status(b.key()).await.unwrap_err().code, "member_rejected");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn responses_not_signed_by_the_enrolled_key_are_rejected() {
    // Something else answers at an enrolled member's address with valid-looking data.
    let impostor = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = impostor.local_addr().unwrap().port();
    let app = axum::Router::new().fallback(|| async {
        axum::Json(serde_json::json!({"schema_version": 1, "data": {"outcome": "accepted"}}))
    });
    tokio::spawn(async move { axum::serve(impostor, app).await });
    let claimed = Member::new(
        "echo".parse().unwrap(),
        MemberKey::generate().unwrap().public(),
        vec![([127, 0, 0, 1], port).into()],
    )
    .unwrap();
    let a = TestMember::start("alpha");
    let b = TestMember::start("bravo");
    let code = elsecall::member_code::encode(&claimed);
    assert_eq!(b.fails(&["fleet", "join", &code]), "member_unverified");
    // Enrolling it succeeds locally, but the fleet view never trusts its answers.
    a.ok(&["fleet", "enroll", &code]);
    let view = a.view_of("echo");
    assert_eq!(view["reachability"], "unreachable");
    assert_eq!(view["error"]["code"], "member_unverified");
}

#[test]
fn changes_refuse_unreachable_members_unless_forced_and_fleet_shows_them_down() {
    let a = TestMember::start("alpha");
    let b = TestMember::start("bravo");
    let c = TestMember::start("charlie");
    let mut d = TestMember::start("delta");
    enrol(&a, &b);
    enrol(&a, &d);
    d.stop();

    let down = a.view_of("delta");
    assert_eq!(down["reachability"], "unreachable");
    assert_eq!(down["error"]["code"], "member_unavailable");
    assert_eq!(a.view_of("bravo")["reachability"], "reachable");

    // Enrolling charlie while delta is down is refused and changes nothing.
    assert_eq!(
        a.fails(&["fleet", "enroll", &c.code()]),
        "member_unavailable"
    );
    assert_eq!(names(&b.fleet()), ["alpha", "bravo", "delta"]);
    // Forced, the reachable members record it; delta keeps its old roster.
    let output = a.run(&["fleet", "enroll", "--force", &c.code()]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "an incomplete change exits 1"
    );
    let report = members::json(&output)["data"].clone();
    assert_eq!(report["complete"], false);
    let failed: Vec<_> = report["deliveries"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|d| d["outcome"] == "failed")
        .map(|d| d["member"].clone())
        .collect();
    assert_eq!(failed, ["delta"]);
    assert_eq!(names(&b.fleet()), ["alpha", "bravo", "charlie", "delta"]);

    assert!(
        report["next_step"]
            .as_str()
            .unwrap()
            .contains("elsecall fleet sync")
    );

    // Once delta is back, syncing brings it up to date.
    c.ok(&["fleet", "join", &a.code()]);
    d.restart();
    assert_eq!(a.view_of("delta")["in_sync"], false);
    let synced = a.ok(&["fleet", "sync"]);
    assert_eq!(synced["complete"], true);
    assert_eq!(a.view_of("delta")["in_sync"], true);
    assert_eq!(names(&d.fleet()), ["alpha", "bravo", "charlie", "delta"]);
}

#[test]
fn a_forced_removal_is_completed_by_sync_and_the_removed_member_loses_trust() {
    let a = TestMember::start("alpha");
    let mut b = TestMember::start("bravo");
    let x = TestMember::start("xray");
    enrol(&a, &b);
    enrol(&a, &x);
    b.stop();
    let output = a.run(&["fleet", "remove", "--force", "xray"]);
    assert_eq!(output.status.code(), Some(1));
    b.restart();
    // bravo missed the removal and still trusts xray.
    assert_eq!(b.view_of("xray")["reachability"], "reachable");
    assert_eq!(a.view_of("bravo")["in_sync"], false);
    // Syncing from the stale member would resurrect xray, so it is refused.
    assert_eq!(b.fails(&["fleet", "sync"]), "stale_roster");
    assert_eq!(names(&a.fleet()), ["alpha", "bravo"]);
    // Syncing from alpha completes the removal everywhere.
    assert_eq!(a.ok(&["fleet", "sync"])["complete"], true);
    assert_eq!(names(&b.fleet()), ["alpha", "bravo"]);
    for other in ["alpha", "bravo"] {
        assert_eq!(x.view_of(other)["error"]["code"], "member_rejected");
    }
}

#[test]
fn sync_resolves_independent_changes_in_favour_of_the_syncing_member() {
    let a = TestMember::start("alpha");
    let b = TestMember::start("bravo");
    let mut c = TestMember::start("charlie");
    let d = TestMember::start("delta");
    let e = TestMember::start("echo");
    enrol(&a, &b);
    enrol(&a, &c);
    // While partitioned, alpha enrols delta and charlie enrols echo at the same epoch.
    c.stop();
    a.run(&["fleet", "enroll", "--force", &d.code()]);
    d.ok(&["fleet", "join", &a.code()]);
    c.restart();
    let mut a_side = a;
    a_side.stop();
    let mut b_side = b;
    b_side.stop();
    c.run(&["fleet", "enroll", "--force", &e.code()]);
    a_side.restart();
    b_side.restart();
    let conflict = a_side.view_of("charlie");
    assert_eq!(conflict["in_sync"], false, "same epoch, different members");
    assert_eq!(
        conflict["status"]["roster"]["epoch"],
        a_side.fleet()["epoch"]
    );
    // Delivering alpha's roster again cannot win; syncing re-issues it above both.
    let synced = a_side.ok(&["fleet", "sync"]);
    assert_eq!(synced["complete"], true);
    for member in [&a_side, &b_side, &c] {
        assert_eq!(
            names(&member.fleet()),
            ["alpha", "bravo", "charlie", "delta"]
        );
    }
}

#[test]
fn broken_fleet_state_keeps_the_local_service_and_explains_the_repair() {
    let a = TestMember::start("alpha");
    let mut b = TestMember::start("bravo");
    enrol(&a, &b);
    let paths = elsecall::paths::Paths::for_home(b.home.path()).unwrap();
    // The key changes after enrolment: the roster no longer lists this member.
    std::fs::remove_file(paths.member_key()).unwrap();
    b.restart_without_fleet();
    assert!(b.run(&["status"]).status.success());
    assert!(b.run(&["machine"]).status.success());
    assert_eq!(b.fails(&["fleet"]), "invalid_fleet_state");
    assert_eq!(b.fails(&["fleet", "leave"]), "invalid_fleet_state");
    // Following the hint makes it a solo member with a new identity.
    std::fs::remove_file(paths.fleet_membership()).unwrap();
    b.restart();
    assert_eq!(b.fleet()["enrolled"], false);
    assert_eq!(a.view_of("bravo")["error"]["code"], "member_rejected");
}

#[test]
fn re_enrolling_a_member_with_new_endpoints_reaches_it_there() {
    let a = TestMember::start("alpha");
    let mut b = TestMember::start("bravo");
    enrol(&a, &b);
    // bravo's tailnet address changed: it now listens elsewhere.
    b.port = members::free_port();
    b.restart();
    assert_eq!(a.view_of("bravo")["reachability"], "unreachable");
    let report = a.ok(&["fleet", "enroll", &b.code()]);
    assert_eq!(report["complete"], true);
    assert_eq!(a.view_of("bravo")["in_sync"], true);
    assert_eq!(b.view_of("alpha")["in_sync"], true);
}

#[test]
fn enrolling_again_before_the_member_joined_repeats_the_join_step() {
    let a = TestMember::start("alpha");
    let b = TestMember::start("bravo");
    let first = a.ok(&["fleet", "enroll", &b.code()]);
    // The join code was lost: enrolling again works and prints it again, although
    // bravo, not having joined, refuses alpha's requests.
    let again = a.ok(&["fleet", "enroll", &b.code()]);
    assert_eq!(again["next_step"], first["next_step"]);
    assert_eq!(again["complete"], true);
    assert_eq!(again["deliveries"][0]["outcome"], "awaiting_join");
    let code = again["next_step"]
        .as_str()
        .unwrap()
        .rsplit(' ')
        .next()
        .unwrap()
        .to_owned();
    b.ok(&["fleet", "join", &code]);
    assert_eq!(a.view_of("bravo")["in_sync"], true);
}
