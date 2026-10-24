use super::*;
use crate::services::turn_summary_line::quota::tests::{accounts, now};
use crate::services::turn_summary_line::{Outcome, render};
use serde_json::json;

fn record(root: &Path, session: &str, value: &Value, at: DateTime<Utc>) {
    let path = record_path(root, session);
    std::fs::write(&path, value.to_string()).unwrap();
    File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(at.into()))
        .unwrap();
}

fn value(session: &str) -> Value {
    json!({"session_id": session, "rate_limits": {
        "five_hour": {"used_percentage": 23.6, "resets_at": now().timestamp() + 9000},
        "seven_day": {"used_percentage": 61, "resets_at": 1793084400_i64}
    }})
}

fn fallback(root: &Path, session: Option<&str>, at: DateTime<Utc>) -> Option<Quota> {
    let mut response = accounts();
    response.fetched_at = at;
    response.accounts[1].usage_fetched_at = Some(at.to_rfc3339());
    with_fallback(root, session, at, || super::super::claude(&response, at))
}

#[test]
fn stdin_epoch_limits_render_percentages_and_kst_resets() {
    let root = tempfile::tempdir().unwrap();
    record(root.path(), "session-a", &value("session-a"), now());
    assert_eq!(
        render(
            Outcome::Completed,
            Some(1000),
            None,
            fallback(root.path(), Some("session-a"), now())
        ),
        "-# ⏱️ 1초 · 5h 24% (11:30) · 7d 61% (10/27 16:00)"
    );
}

#[test]
fn mismatched_session_is_rejected_before_cswap_fallback() {
    let root = tempfile::tempdir().unwrap();
    record(root.path(), "session-a", &value("session-b"), now());
    assert_eq!(
        fallback(root.path(), Some("session-a"), now())
            .unwrap()
            .five_hour
            .unwrap()
            .percent,
        15,
        "foreign session must use cswap, never its 24 percent"
    );
    assert_eq!(read(root.path(), "session-a", now()), None);
}

#[test]
fn fifteen_minutes_is_inclusive_and_stale_records_use_cswap() {
    let root = tempfile::tempdir().unwrap();
    record(root.path(), "session-a", &value("session-a"), now());
    let boundary = now() + chrono::Duration::seconds(900);
    assert_eq!(
        fallback(root.path(), Some("session-a"), boundary)
            .unwrap()
            .five_hour
            .unwrap()
            .percent,
        24
    );
    assert_eq!(
        fallback(
            root.path(),
            Some("session-a"),
            boundary + chrono::Duration::milliseconds(1)
        )
        .unwrap()
        .five_hour
        .unwrap()
        .percent,
        15,
        "expired record must use cswap's 15 percent"
    );
    assert_eq!(
        read(
            root.path(),
            "session-a",
            now() - chrono::Duration::milliseconds(1)
        ),
        None
    );
}

#[test]
fn missing_legacy_malformed_and_empty_records_preserve_cswap_or_omit_quota() {
    let root = tempfile::tempdir().unwrap();
    for session in [None, Some(""), Some("not-recorded")] {
        assert_eq!(
            fallback(root.path(), session, now())
                .unwrap()
                .five_hour
                .unwrap()
                .percent,
            15
        );
        assert_eq!(with_fallback(root.path(), session, now(), || None), None);
    }
    for malformed in [
        json!({}),
        json!({"session_id":"session-a"}),
        json!({"session_id":"session-a","rate_limits":null}),
        json!({"session_id":"session-a","rate_limits":{"five_hour":{
            "used_percentage":101,"resets_at":now().timestamp()+9000}}}),
    ] {
        record(root.path(), "session-a", &malformed, now());
        assert_eq!(read(root.path(), "session-a", now()), None, "{malformed}");
        assert_eq!(
            fallback(root.path(), Some("session-a"), now())
                .unwrap()
                .five_hour
                .unwrap()
                .percent,
            15
        );
    }
    std::fs::write(record_path(root.path(), "session-a"), b"{incomplete").unwrap();
    assert_eq!(
        with_fallback(root.path(), Some("session-a"), Utc::now(), || None),
        None
    );
}

#[test]
fn zero_is_real_only_with_a_valid_future_window_and_one_window_can_survive() {
    let root = tempfile::tempdir().unwrap();
    let mut data = value("session-a");
    data["rate_limits"]["five_hour"]["used_percentage"] = json!(0);
    data["rate_limits"]["seven_day"]["resets_at"] = json!(now().timestamp());
    record(root.path(), "session-a", &data, now());
    let quota = read(root.path(), "session-a", now()).unwrap();
    assert_eq!(quota.five_hour.unwrap().percent, 0);
    assert_eq!(quota.seven_day, None);
    data["rate_limits"]["five_hour"]
        .as_object_mut()
        .unwrap()
        .remove("used_percentage");
    record(root.path(), "session-a", &data, now());
    assert_eq!(read(root.path(), "session-a", now()), None);
}
