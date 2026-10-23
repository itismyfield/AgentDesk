use super::*;
use crate::services::turn_summary_line::{Outcome, render};
use serde_json::json;

pub(crate) fn with_locked_quota_cache<T>(run: impl FnOnce() -> T) -> T {
    let _guard = CACHE.write().unwrap();
    run()
}

fn now() -> DateTime<Utc> {
    "2026-10-23T00:00:00Z".parse().unwrap()
}

fn accounts() -> ClaudeAccountsResponse {
    let account = |number, active, pct| {
        serde_json::from_value(json!({
            "number":number,"active":active,"usageStatus":"ok",
            "usageFetchedAt":"2026-10-22T23:59:00Z",
            "usage":{"fiveHour":{"pct":pct,"resetsAt":"2026-10-23T02:30:00Z"},
                     "sevenDay":{"pct":38,"resetsAt":"2026-10-27T07:00:00Z"}}
        }))
        .unwrap()
    };
    ClaudeAccountsResponse {
        schema_version: 1,
        status: ClaudeAccountsStatus::Ok,
        hostname: "isolated".into(),
        instance_id: None,
        fetched_at: now(),
        served_at: now(),
        cache_ttl_seconds: 60,
        usage_data_stale: false,
        stale_reason: None,
        active_account_number: Some(2),
        accounts: vec![account(1, false, 90), account(2, true, 15)],
    }
}

fn data(percent: u8) -> String {
    json!({"buckets":[
        {"name":"5h","used":percent,"limit":100,"reset":now().timestamp()+9000},
        {"name":"7d","used":38,"limit":100,"reset":1793084400_i64}
    ]})
    .to_string()
}

#[test]
fn active_claude_quota_renders_one_line_in_order_with_kst_resets() {
    let quota = claude(&accounts(), now()).unwrap();
    let line = render(Outcome::Completed, Some(150_000), Some(45), Some(quota));
    assert_eq!(
        line,
        "-# ⏱️ 2분 30초 · 📦 45% · 5h 15% (11:30) · 7d 38% (10/27 16:00)"
    );
    assert!(!line.chars().any(char::is_control));
    assert_eq!(line.lines().count(), 1);
}

#[test]
fn claude_missing_stale_and_ambiguous_accounts_never_supply_a_quota() {
    let base = accounts();
    let mut cases = Vec::new();
    let mut absent = base.clone();
    absent.accounts.clear();
    cases.push(absent);
    let mut wrong = base.clone();
    wrong.active_account_number = Some(1);
    cases.push(wrong);
    let mut ambiguous = base.clone();
    ambiguous.accounts[0].active = true;
    cases.push(ambiguous);
    let mut inactive = base.clone();
    inactive.accounts[1].active = false;
    cases.push(inactive);
    let mut old_cache = base.clone();
    old_cache.fetched_at -= chrono::Duration::seconds(901);
    cases.push(old_cache);
    let mut future = base.clone();
    future.fetched_at += chrono::Duration::milliseconds(1);
    cases.push(future);
    let mut old_usage = base.clone();
    old_usage.accounts[1].usage_fetched_at =
        Some((now() - chrono::Duration::seconds(901)).to_rfc3339());
    cases.push(old_usage);
    let mut stale = base.clone();
    stale.usage_data_stale = true;
    cases.push(stale);
    let mut failed = base.clone();
    failed.accounts[1].usage_status = Some("error".into());
    cases.push(failed);
    let mut missing = base;
    missing.accounts[1].usage = None;
    cases.push(missing);
    for response in cases {
        assert_eq!(claude(&response, now()), None, "{response:?}");
    }
}

#[test]
fn claude_usage_age_includes_cache_residence_and_expires_after_fifteen_minutes() {
    let mut response = accounts();
    response.fetched_at -= chrono::Duration::seconds(10);
    response.accounts[1].usage_fetched_at = None;
    response.accounts[1].usage_age_seconds = Some(890.0);
    assert!(claude(&response, now()).is_some());
    assert_eq!(
        claude(&response, now() + chrono::Duration::milliseconds(1)),
        None
    );
    for age in [None, Some(f64::NAN), Some(f64::INFINITY), Some(-11.0)] {
        response.accounts[1].usage_age_seconds = age;
        assert_eq!(claude(&response, now()), None);
    }
    response.accounts[1].usage_fetched_at =
        Some((now() - chrono::Duration::seconds(900)).to_rfc3339());
    assert!(claude(&response, now()).is_some());
    assert_eq!(
        claude(&response, now() + chrono::Duration::milliseconds(1)),
        None
    );
}

#[test]
fn quota_windows_need_valid_percent_and_a_future_reset() {
    for percent in [f64::NAN, f64::INFINITY, -1.0, 101.0] {
        assert_eq!(
            Window::new(percent, now() + chrono::Duration::hours(1), now()),
            None
        );
    }
    assert_eq!(Window::new(15.0, now(), now()), None);
    assert_eq!(
        Window::new(15.0, now() - chrono::Duration::seconds(1), now()),
        None
    );
    let mut response = accounts();
    response.accounts[1]
        .usage
        .as_mut()
        .unwrap()
        .five_hour
        .as_mut()
        .unwrap()
        .pct = None;
    let line = render(
        Outcome::Completed,
        Some(1000),
        None,
        claude(&response, now()),
    );
    assert_eq!(line, "-# ⏱️ 1초 · 7d 38% (10/27 16:00)");
}

fn cache() -> RwLock<Cache> {
    RwLock::new(Cache {
        quotas: BTreeMap::from([
            ("default".into(), (now(), codex(&data(90), now()).unwrap())),
            ("named".into(), (now(), codex(&data(15), now()).unwrap())),
        ]),
        profiles: BTreeMap::from([(("codex".into(), 42), (now(), "named".into()))]),
    })
}

#[test]
fn codex_requires_the_selected_profile_and_a_fresh_cache() {
    let cache = cache();
    let quota = cached_codex(&cache, 42, now(), now()).unwrap();
    assert_eq!(quota.five_hour.unwrap().percent, 15);
    assert_eq!(cached_codex(&cache, 43, now(), now()), None);
    assert_eq!(
        cached_codex(&cache, 42, now() - chrono::Duration::milliseconds(1), now()),
        None
    );
    assert!(cached_codex(&cache, 42, now(), now() + chrono::Duration::seconds(900)).is_some());
    assert_eq!(
        cached_codex(&cache, 42, now(), now() + chrono::Duration::seconds(901)),
        None
    );
    cache.write().unwrap().quotas.remove("named");
    assert_eq!(cached_codex(&cache, 42, now(), now()), None);
    assert_eq!(snapshot("unsupported", 42, Some(now())), None);
}

#[test]
fn codex_incomplete_duplicate_and_unrelated_windows_are_not_zero_percent() {
    for buckets in [
        json!([]),
        json!([{"name":"5h","limit":100,"reset":1793084400_i64}]),
        json!([{"name":"5h","used":0,"limit":0,"reset":1793084400_i64}]),
        json!([{"name":"5h","used":0,"limit":100,"reset":0}]),
        json!([{"name":"1d","used":0,"limit":100,"reset":1793084400_i64}]),
        json!([{"name":"5h","used":1,"limit":100,"reset":1793084400_i64},
               {"name":"5h","used":2,"limit":100,"reset":1793084400_i64}]),
    ] {
        assert_eq!(codex(&json!({"buckets":buckets}).to_string(), now()), None);
    }
    let quota = codex(
        &json!({"buckets":[
            {"name":"5h","used":0,"limit":100,"reset":now().timestamp()+9000}
        ]})
        .to_string(),
        now(),
    );
    assert_eq!(
        render(Outcome::Completed, Some(1000), None, quota),
        "-# ⏱️ 1초 · 5h 0% (11:30)"
    );
}

#[test]
fn contended_quota_cache_still_renders_without_waiting_for_its_writer() {
    let cache = std::sync::Arc::new(cache());
    let guard = cache.write().unwrap();
    let (sent, received) = std::sync::mpsc::channel();
    let reader = cache.clone();
    let thread = std::thread::spawn(move || {
        let quota = cached_codex(&reader, 42, now(), now());
        sent.send(render(Outcome::Completed, Some(1000), None, quota))
            .unwrap();
    });
    let delivered_while_locked = received.recv_timeout(std::time::Duration::from_secs(1));
    drop(guard);
    thread.join().unwrap();
    assert!(
        delivered_while_locked.is_ok(),
        "summary waited for the quota writer"
    );
    assert_eq!(delivered_while_locked.unwrap(), "-# ⏱️ 1초");
}

#[test]
fn quota_does_not_change_untimed_completion_cancellation_or_failure() {
    let quota = claude(&accounts(), now());
    for (outcome, elapsed, context, expected) in [
        (Outcome::Completed, None, None, "-# ✅ 완료"),
        (Outcome::Completed, None, Some(45), "-# ✅ · 📦 45%"),
        (
            Outcome::Cancelled,
            Some(1000),
            Some(45),
            "-# ⏹ 중단 · 1초 · 📦 45%",
        ),
        (Outcome::Failed, Some(1000), None, "-# ⚠️ 오류 · 1초"),
    ] {
        assert_eq!(render(outcome, elapsed, context, quota), expected);
    }
}

#[test]
fn recorded_profile_and_collected_quota_reach_the_o_summary_boundary() {
    use crate::services::provider::ProviderKind;
    use crate::services::tui_o::shadow::{CapturedRecord, ShadowProvider};
    use crate::services::tui_o::writer::pieces::{Derived, SummaryInputs, UnitDeriver};
    let channel = 7_271_001;
    let mut overlay = ProviderAuthOverlay::default_for(ProviderKind::Codex);
    overlay.profile_id = "summary-boundary-isolated".into();
    assert_eq!(
        record_launch(Some(channel), overlay.clone()),
        Ok(overlay.clone())
    );
    let started = Utc::now();
    let collected = json!({"buckets":[
        {"name":"5h","used":15,"limit":100,"reset":started.timestamp()+18000},
        {"name":"7d","used":38,"limit":100,"reset":started.timestamp()+604800}
    ]})
    .to_string();
    publish(
        "codex",
        &overlay.profile_id,
        &collected,
        started.timestamp(),
    );
    let mut derive =
        UnitDeriver::new(channel, ShadowProvider::Codex).with_turn_summary(SummaryInputs {
            turn_marks: Default::default(),
            disposed: Default::default(),
            context_window: None,
        });
    let row = |value: Value| CapturedRecord {
        start: 0,
        end: 1,
        line: serde_json::to_vec(&value).unwrap(),
    };
    derive.derive(&row(
        json!({"type":"event_msg","timestamp":started.to_rfc3339(),
        "payload":{"type":"task_started","turn_id":"isolated"}}),
    ));
    derive.derive(&row(json!({"type":"response_item","payload":{
        "type":"message","role":"assistant","id":"answer","content":[{"type":"output_text","text":"body"}]}})));
    let work = derive.derive(&row(json!({"type":"event_msg",
        "timestamp":(started+chrono::Duration::seconds(2)).to_rfc3339(),
        "payload":{"type":"task_complete","turn_id":"isolated"}})));
    let lines = work
        .iter()
        .filter_map(|work| match work {
            Derived::Piece(piece) => Some(piece.payload.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(lines.len(), 1);
    assert!(lines[0].starts_with("-# ⏱️ 2초 · 5h 15% ("), "{lines:?}");
    assert!(lines[0].contains(" · 7d 38% ("), "{lines:?}");
    publish("codex", &overlay.profile_id, "{}", started.timestamp());
    assert_eq!(snapshot("codex", channel, Some(started)), None);
}
