#![cfg(target_os = "macos")]
//! The master role between real member services: who holds placement authority, and
//! moving the role.
mod members;
mod processes;
use members::{TestMember, enrol};

fn master_seen_by(member: &TestMember) -> String {
    member.fleet()["master"].as_str().unwrap().to_owned()
}

#[test]
fn only_the_recorded_master_holds_placement_authority() {
    let a = TestMember::start("alpha");
    let b = TestMember::start("bravo");
    let c = TestMember::start("charlie");
    let outsider = TestMember::start("delta");
    enrol(&a, &b);
    enrol(&a, &c);

    let accepted = a.ok(&["fleet", "check-authority", "bravo"]);
    assert_eq!(accepted["member"], "bravo");
    assert_eq!(accepted["accepted_from"], "alpha");
    a.ok(&["fleet", "check-authority", "alpha"]);
    a.ok(&["fleet", "check-authority", "charlie"]);

    // Enrolled members that are not master are authenticated but not authorized.
    assert_eq!(
        b.fails(&["fleet", "check-authority", "charlie"]),
        "not_master"
    );
    assert_eq!(
        c.fails(&["fleet", "check-authority", "alpha"]),
        "not_master"
    );
    assert_eq!(
        a.fails(&["fleet", "check-authority", "nobody"]),
        "not_a_member"
    );
    // An unenrolled machine that knows alpha's code still gets nothing.
    outsider.ok(&["fleet", "enroll", &a.code()]);
    assert_eq!(
        outsider.fails(&["fleet", "check-authority", "alpha"]),
        "member_rejected"
    );

    // The master cannot simply walk away from the fleet.
    assert_eq!(a.fails(&["fleet", "leave"]), "member_is_master");
}

#[test]
fn moving_the_master_updates_every_member_and_refuses_unreachable_unless_forced() {
    let a = TestMember::start("alpha");
    let b = TestMember::start("bravo");
    let mut c = TestMember::start("charlie");
    enrol(&a, &b);
    enrol(&a, &c);

    // An explicit move updates every member, and authority follows it.
    let moved = a.ok(&["fleet", "move-master", "bravo"]);
    assert_eq!(
        (moved["master"].as_str(), moved["complete"].as_bool()),
        (Some("bravo"), Some(true))
    );
    for member in [&a, &b, &c] {
        assert_eq!(master_seen_by(member), "bravo");
    }
    assert_eq!(
        a.fails(&["fleet", "check-authority", "charlie"]),
        "not_master"
    );
    b.ok(&["fleet", "check-authority", "charlie"]);
    assert_eq!(b.view_of("bravo")["role"], "master");
    assert_eq!(a.fails(&["fleet", "move-master", "nobody"]), "not_a_member");

    // With charlie down, the move is refused and nothing changes.
    c.stop();
    assert_eq!(
        b.fails(&["fleet", "move-master", "alpha"]),
        "member_unavailable"
    );
    for member in [&a, &b] {
        assert_eq!(master_seen_by(member), "bravo");
    }

    // Forced, the reachable members move; charlie keeps trusting the old master.
    let forced = b.run(&["fleet", "move-master", "--force", "alpha"]);
    assert_eq!(
        forced.status.code(),
        Some(1),
        "incomplete: charlie missed it"
    );
    assert_eq!(members::json(&forced)["data"]["master"], "alpha");
    c.restart();
    assert_eq!(master_seen_by(&a), "alpha");
    assert_eq!(master_seen_by(&c), "bravo");
    let stale = a.view_of("charlie");
    assert_eq!(stale["in_sync"], false);
    assert_eq!(stale["status"]["roster"]["master"], "bravo");
    assert_eq!(
        a.fails(&["fleet", "check-authority", "charlie"]),
        "not_master"
    );
    b.ok(&["fleet", "check-authority", "charlie"]);

    // Repeating the move once charlie is back brings it up to date.
    let repeated = a.ok(&["fleet", "move-master", "alpha"]);
    assert_eq!(repeated["complete"], true);
    assert_eq!(master_seen_by(&c), "alpha");
    a.ok(&["fleet", "check-authority", "charlie"]);
    assert_eq!(
        b.fails(&["fleet", "check-authority", "charlie"]),
        "not_master"
    );
}

#[test]
fn any_enrolled_member_may_move_the_role_but_only_the_master_holds_authority() {
    let a = TestMember::start("alpha");
    let b = TestMember::start("bravo");
    enrol(&a, &b);
    // The operator can move the role from a member that is not master, e.g. when the
    // master is gone; authority follows the recorded master, not who ran the command.
    b.ok(&["fleet", "move-master", "bravo"]);
    assert_eq!(master_seen_by(&a), "bravo");
    assert_eq!(
        a.fails(&["fleet", "check-authority", "bravo"]),
        "not_master"
    );
    b.ok(&["fleet", "check-authority", "alpha"]);
}
