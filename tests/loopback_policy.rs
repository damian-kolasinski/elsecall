#![cfg(target_os = "macos")]
//! Without a configured presence, loopback never stands in for the tailnet. This test
//! binary is its own process, which never enables the override.
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use elsecall::{member_code, member_key::MemberKey, roster::Member};

#[test]
fn production_members_refuse_loopback_endpoints() {
    let key = MemberKey::generate().unwrap().public();
    let local = Member::new(
        "a".parse().unwrap(),
        key,
        vec![([127, 0, 0, 1], 47821).into()],
    );
    assert_eq!(local.unwrap_err().code, "invalid_member_endpoints");
    let tailnet = Member::new(
        "a".parse().unwrap(),
        key,
        vec![([100, 64, 0, 9], 47821).into()],
    )
    .unwrap();
    assert_eq!(
        member_code::decode(&member_code::encode(&tailnet)).unwrap(),
        tailnet
    );
    // A code naming a loopback endpoint, as another member might send, is refused.
    let json = format!(r#"{{"name":"a","key":"{key}","endpoints":["127.0.0.1:47821"]}}"#);
    let code = format!("elsecall-member-v1.{}", URL_SAFE_NO_PAD.encode(json));
    assert_eq!(
        member_code::decode(&code).unwrap_err().code,
        "invalid_member_code"
    );
}
