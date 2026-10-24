use super::*;
use crate::services::turn_summary_line::{Outcome, render};
use serde_json::json;

#[cfg(unix)]
#[test]
fn statusline_quota_reaches_o_summary_when_cswap_usage_is_429() {
    use crate::services::tui_o::shadow::{CapturedRecord, ShadowProvider};
    use crate::services::tui_o::writer::pieces::{Derived, SummaryInputs, UnitDeriver};
    use sha2::{Digest, Sha256};

    if isolated_cswap_429_fixture(concat!(
        module_path!(),
        "::statusline_quota_reaches_o_summary_when_cswap_usage_is_429"
    )) {
        return;
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime
        .block_on(CswapService::global().list_accounts("fixture".into(), None))
        .unwrap();
    let usage_429 = CswapService::global()
        .cached_accounts_for_summary()
        .unwrap();
    assert_eq!(
        usage_429.accounts[0].extra["usageError"],
        "429 usage-endpoint budget reached; backing off"
    );
    assert!(usage_429.accounts[0].usage_age_seconds.unwrap() >= 51.0 * 60.0);
    assert_eq!(claude(&usage_429, Utc::now()), None);

    let session = uuid::Uuid::new_v4().to_string();
    let root = std::env::temp_dir().join("agentdesk-claude-rate-limits");
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join(format!("{:x}.json", Sha256::digest(session.as_bytes())));
    let started = Utc::now();
    let five_reset = started + chrono::Duration::hours(5);
    let seven_reset = started + chrono::Duration::days(7);
    std::fs::write(
        &path,
        json!({
            "session_id": session,
            "rate_limits": {
                "five_hour": {"used_percentage": 23.6, "resets_at": five_reset.timestamp()},
                "seven_day": {"used_percentage": 61, "resets_at": seven_reset.timestamp()}
            }
        })
        .to_string(),
    )
    .unwrap();
    let mut deriver =
        UnitDeriver::new(7_319_001, ShadowProvider::Claude).with_turn_summary(SummaryInputs {
            turn_marks: Default::default(),
            disposed: Default::default(),
            context_window: None,
        });
    let row = |value: Value| CapturedRecord {
        start: 0,
        end: 1,
        line: serde_json::to_vec(&value).unwrap(),
    };
    deriver.derive(&row(json!({"type":"user", "sessionId":session,
        "timestamp":started.to_rfc3339(), "message":{"content":"request"}})));
    deriver.derive(&row(json!({"type":"assistant", "sessionId":session,
        "timestamp":started.to_rfc3339(), "uuid":"statusline-answer", "apiBlockIndex":0,
        "message":{"id":"statusline-answer", "content":[{"type":"text","text":"answer"}]}})));
    let work = deriver.derive(&row(json!({"type":"system", "subtype":"stop_hook_summary",
        "sessionId":session, "timestamp":(started+chrono::Duration::seconds(2)).to_rfc3339()})));
    let lines = work
        .iter()
        .filter_map(|work| match work {
            Derived::Piece(piece) => Some(piece.payload.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    let expected = format!(
        "-# ⏱️ 2초 · 5h 24% ({}) · 7d 61% ({})",
        five_reset
            .with_timezone(&chrono_tz::Asia::Seoul)
            .format("%H:%M"),
        seven_reset
            .with_timezone(&chrono_tz::Asia::Seoul)
            .format("%m/%d %H:%M")
    );
    let _ = std::fs::remove_file(&path);
    assert_eq!(
        lines,
        vec![expected.as_str()],
        "fresh session statusline must survive cswap 429"
    );
}

#[cfg(unix)]
fn isolated_cswap_429_fixture(name: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let name = name
        .strip_prefix(concat!(env!("CARGO_CRATE_NAME"), "::"))
        .unwrap();
    const CHILD: &str = "ADK_STATUSLINE_CSWAP_429_TEST";
    if std::env::var(CHILD).as_deref() == Ok(name) {
        return false;
    }
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("tmp")).unwrap();
    std::fs::create_dir(root.path().join(".claude")).unwrap();
    std::fs::write(root.path().join("config.yaml"), "server: {}\n").unwrap();
    let executable = root.path().join("cswap");
    std::fs::write(&executable, "#!/bin/sh\ncat \"$0.json\"\n").unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    let at = Utc::now();
    std::fs::write(executable.with_extension("json"), json!({
        "activeAccountNumber":2,"accounts":[{
            "number":2,"active":true,"usageStatus":"ok",
            "usageFetchedAt":(at-chrono::Duration::minutes(51)).to_rfc3339(),
            "usageError":"429 usage-endpoint budget reached; backing off",
            "usage":{"fiveHour":{"pct":15,"resetsAt":(at+chrono::Duration::hours(5)).to_rfc3339()}}
        }]
    }).to_string()).unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env(CHILD, name)
        .env("AGENTDESK_CSWAP_PATH", executable)
        .env("HOME", root.path())
        .env("CLAUDE_CONFIG_DIR", root.path().join(".claude"))
        .env("TMPDIR", root.path().join("tmp"))
        .env("AGENTDESK_ROOT_DIR", root.path())
        .env("AGENTDESK_CONFIG", root.path().join("config.yaml"))
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success()
            && stdout.contains("test result: ok. 1 passed; 0 failed; 0 ignored;"),
        "isolated cached cswap 429 test failed:\n{stdout}\n{stderr}"
    );
    true
}

pub(crate) fn with_locked_quota_cache<T>(run: impl FnOnce() -> T) -> T {
    let _guard = CACHE.write().unwrap();
    run()
}

pub(super) fn now() -> DateTime<Utc> {
    "2026-10-23T00:00:00Z".parse().unwrap()
}

pub(super) fn accounts() -> ClaudeAccountsResponse {
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
    let mut rollover = accounts();
    rollover.accounts[1]
        .usage
        .as_mut()
        .unwrap()
        .seven_day
        .as_mut()
        .unwrap()
        .resets_at = Some("2026-10-26T23:45:00Z".into());
    assert_eq!(
        render(
            Outcome::Completed,
            Some(150_000),
            Some(45),
            claude(&rollover, now())
        ),
        "-# ⏱️ 2분 30초 · 📦 45% · 5h 15% (11:30) · 7d 38% (10/27 08:45)"
    );
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
        profiles: BTreeMap::from([(
            ("codex".into(), 42),
            Selection {
                at: now(),
                profile: "named".into(),
            },
        )]),
    })
}

#[test]
fn codex_requires_the_selected_profile_and_a_fresh_cache() {
    let cache = cache();
    let quota = cached_codex(&cache, 42, Selected::Before(now()), now()).unwrap();
    assert_eq!(quota.five_hour.unwrap().percent, 15);
    assert_eq!(
        cached_codex(&cache, 43, Selected::Before(now()), now()),
        None
    );
    assert_eq!(
        cached_codex(
            &cache,
            42,
            Selected::Before(now() - chrono::Duration::milliseconds(1)),
            now()
        ),
        None
    );
    assert!(
        cached_codex(
            &cache,
            42,
            Selected::Before(now()),
            now() + chrono::Duration::seconds(900)
        )
        .is_some()
    );
    assert_eq!(
        cached_codex(
            &cache,
            42,
            Selected::Before(now()),
            now() + chrono::Duration::seconds(901)
        ),
        None
    );
    assert!(cached_codex(&cache, 42, Selected::Within(now()), now()).is_some());
    assert!(
        cached_codex(
            &cache,
            42,
            Selected::Within(now() - chrono::Duration::milliseconds(1)),
            now()
        )
        .is_some()
    );
    let key = ("codex".to_string(), 42);
    cache.write().unwrap().profiles.get_mut(&key).unwrap().at =
        now() - chrono::Duration::milliseconds(1);
    assert_eq!(
        cached_codex(&cache, 42, Selected::Within(now()), now()),
        None
    );
    cache.write().unwrap().profiles.get_mut(&key).unwrap().at =
        now() + chrono::Duration::milliseconds(1);
    assert_eq!(
        cached_codex(&cache, 42, Selected::Within(now()), now()),
        None
    );
    cache.write().unwrap().profiles.get_mut(&key).unwrap().at = now();
    cache.write().unwrap().quotas.remove("named");
    assert_eq!(
        cached_codex(&cache, 42, Selected::Before(now()), now()),
        None
    );
    assert_eq!(
        snapshot("unsupported", 42, Some(Selected::Before(now())), None),
        None
    );
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
        let quota = cached_codex(&reader, 42, Selected::Before(now()), now());
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
    assert_eq!(
        snapshot("codex", channel, Some(Selected::Before(started)), None),
        None
    );
}
