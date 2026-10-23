use super::*;
use crate::services::discord::gateway::GatewayFuture;
use crate::services::discord::serenity::MessageId;
use std::sync::atomic::{AtomicBool, Ordering};

pub(in crate::services::discord) struct Isolation {
    _guards: [crate::config::TestEnvVarGuard; 2],
    _live: crate::config_live_reload::TestLiveGuard,
    _root: tempfile::TempDir,
}

impl Isolation {
    pub(in crate::services::discord) fn new() -> Self {
        let live = crate::config_live_reload::test_live_guard(
            crate::config::test_env_lock::acquire_shared_test_env_lock(),
        );
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("config.yaml");
        std::fs::write(&config, "server: {}\n").unwrap();
        let set = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock;
        let guards = [
            set("AGENTDESK_ROOT_DIR", root.path()),
            set("AGENTDESK_CONFIG", &config),
        ];
        crate::config_live_reload::install(crate::config::Config::default());
        Self {
            _guards: guards,
            _live: live,
            _root: root,
        }
    }
}

#[derive(Default)]
pub(in crate::services::discord) struct Gateway {
    pub(in crate::services::discord) sent: Mutex<Vec<String>>,
    pub(in crate::services::discord) edited: Mutex<Vec<String>>,
    fail: AtomicBool,
    pending: AtomicBool,
}

impl Gateway {
    pub(in crate::services::discord) fn posts(&self) -> Vec<String> {
        self.sent.lock().unwrap().clone()
    }
}

impl TurnGateway for Gateway {
    fn send_message<'a>(
        &'a self,
        _: ChannelId,
        content: &'a str,
    ) -> GatewayFuture<'a, Result<MessageId, String>> {
        Box::pin(async move {
            self.sent.lock().unwrap().push(content.into());
            if self.pending.load(Ordering::SeqCst) {
                std::future::pending::<()>().await;
            }
            if self.fail.load(Ordering::SeqCst) {
                Err("uncertain response".into())
            } else {
                Ok(MessageId::new(100))
            }
        })
    }
    fn edit_message<'a>(
        &'a self,
        _: ChannelId,
        _: MessageId,
        text: &'a str,
    ) -> GatewayFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.edited.lock().unwrap().push(text.into());
            Ok(())
        })
    }
    fn replace_message_with_outcome<'a>(
        &'a self,
        _: ChannelId,
        _: MessageId,
        _: &'a str,
    ) -> GatewayFuture<'a, Result<super::super::formatting::ReplaceLongMessageOutcome, String>>
    {
        Box::pin(async { panic!("summary cannot replace an assistant body") })
    }
    fn schedule_retry_with_history<'a>(
        &'a self,
        _: ChannelId,
        _: MessageId,
        _: &'a str,
    ) -> GatewayFuture<'a, ()> {
        Box::pin(async { panic!("summary cannot schedule a provider retry") })
    }
    fn dispatch_queued_turn<'a>(
        &'a self,
        _: ChannelId,
        _: &'a super::super::Intervention,
        _: &'a str,
        _: bool,
        _: Option<Arc<crate::services::turn_orchestrator::DispatchLease>>,
    ) -> GatewayFuture<'a, Result<(), String>> {
        Box::pin(async { panic!("summary cannot dispatch a turn") })
    }
    fn validate_live_routing<'a>(&'a self, _: ChannelId) -> GatewayFuture<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
    fn requester_mention(&self) -> Option<String> {
        None
    }
    fn can_chain_locally(&self) -> bool {
        false
    }
    fn bot_owner_provider(&self) -> Option<ProviderKind> {
        Some(ProviderKind::Claude)
    }
}

pub(in crate::services::discord) fn row(channel: u64, user: u64, nonce: &str) -> InflightTurnState {
    serde_json::from_value(serde_json::json!({
        "version":9,"provider":"claude","channel_id":channel,"channel_name":"summary-test",
        "request_owner_user_id":1,"user_msg_id":user,"current_msg_id":100,"current_msg_len":6,
        "user_text":"question","source":"text","session_id":null,"tmux_session_name":null,
        "output_path":null,"input_fifo_path":null,"last_offset":20,"turn_start_offset":10,
        "full_response":"answer","response_sent_offset":6,"started_at":"unknown","updated_at":"unknown",
        "turn_nonce":nonce,"runtime_kind":"claude_tui"
    })).unwrap()
}

fn summary(channel: u64, user: u64, nonce: &str) -> LiveSummary {
    LiveSummary::bridge(
        ChannelId::new(channel),
        &row(channel, user, nonce),
        0,
        Outcome::Completed,
        None,
    )
}

#[tokio::test]
async fn selected_profile_usage_reaches_the_legacy_summary_without_a_quota_query() {
    use futures::FutureExt;
    let _isolation = Isolation::new();
    let _boot = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let channel = 7_271_002;
    let overlay =
        super::super::org_schema::spawn_auth_overlay(ProviderKind::Codex, Some(channel)).unwrap();
    let now = chrono::Utc::now();
    let data = serde_json::json!({"buckets":[
        {"name":"5h","used":15,"limit":100,"reset":now.timestamp()+18000},
        {"name":"7d","used":38,"limit":100,"reset":now.timestamp()+604800}
    ]})
    .to_string();
    quota::publish("codex", &overlay.profile_id, &data, now.timestamp());
    struct Cleanup(String);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            quota::publish("codex", &self.0, "{}", chrono::Utc::now().timestamp());
        }
    }
    let _cleanup = Cleanup(overlay.profile_id);
    tokio::time::sleep(Duration::from_secs(1)).await;
    let mut state = row(channel, 1, "quota-boundary");
    state.started_at = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let summary = LiveSummary::bridge(
        ChannelId::new(channel),
        &state,
        0,
        Outcome::Completed,
        Some(1000),
    );
    let gateway = Gateway::default();
    post_live(&gateway, &ProviderKind::Codex, summary).await;
    let posts = gateway.posts();
    assert_eq!(posts.len(), 1);
    assert!(posts[0].starts_with("-# ⏱️ 1초 · 5h 15% ("), "{posts:?}");
    assert!(posts[0].contains(" · 7d 38% ("), "{posts:?}");
    let state = row(channel, 2, "quota-contended");
    let summary = LiveSummary::bridge(
        ChannelId::new(channel),
        &state,
        0,
        Outcome::Completed,
        Some(1000),
    );
    let delivered = quota::tests::with_locked_quota_cache(|| {
        post_live(&gateway, &ProviderKind::Codex, summary).now_or_never()
    });
    assert!(
        delivered.is_some(),
        "Legacy summary waited for the quota writer"
    );
    assert_eq!(gateway.posts()[1], "-# ⏱️ 1초");
}

#[test]
fn latch_retains_pending_attempts_and_bounds_finished_entries_by_ttl_and_capacity() {
    let mut latch = Latch::default();
    let now = Instant::now();
    let pending = summary(586_001, 1, "pending").key;
    assert!(latch.take(pending, now));
    for id in 2..=CAP as u64 {
        assert!(latch.take(summary(586_001, id, "done").key, now));
    }
    assert!(!latch.take(summary(586_001, CAP as u64 + 1, "full").key, now + TTL));
    assert_eq!(latch.0.len(), CAP);
    assert!(!latch.take(pending, now + TTL));
    let done = summary(586_001, 2, "done").key;
    latch.finish(done, now);
    assert!(latch.take(summary(586_001, CAP as u64 + 1, "full").key, now));
    assert!(latch.0.contains_key(&pending.exact_key()));
    let mut expired = Latch::default();
    assert!(expired.take(done, now));
    expired.finish(done, now);
    assert!(!expired.take(done, now + TTL - Duration::from_nanos(1)));
    assert!(expired.take(done, now + TTL));
}

#[test]
fn latch_keeps_distinct_zero_id_sources_and_episode_nonces_separate() {
    let mut first = row(586_002, 0, "first");
    first.output_path = Some("/fixture/a.jsonl".into());
    let mut second = first.clone();
    second.output_path = Some("/fixture/b.jsonl".into());
    let mut third = first.clone();
    third.turn_nonce = Some("third".into());
    let mut latch = Latch::default();
    for state in [&first, &second, &third] {
        let key =
            LiveSummary::bridge(ChannelId::new(586_002), state, 0, Outcome::Completed, None).key;
        assert!(latch.take(key, Instant::now()));
        assert!(!latch.take(key, Instant::now()));
    }
    assert_eq!(latch.0.len(), 3);
}

#[tokio::test(flavor = "current_thread")]
async fn uncertain_or_cancelled_sends_are_consumed_without_a_second_producer_retry() {
    let _boot = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    for pending in [false, true] {
        let gateway = Gateway::default();
        gateway.fail.store(!pending, Ordering::SeqCst);
        gateway.pending.store(pending, Ordering::SeqCst);
        let summary = summary(586_003 + u64::from(pending), 1, "attempt");
        let mut attempt = Box::pin(post_live(&gateway, &ProviderKind::Claude, summary));
        if pending {
            assert!(futures::poll!(&mut attempt).is_pending());
            drop(attempt);
        } else {
            attempt.await;
        }
        gateway.fail.store(false, Ordering::SeqCst);
        gateway.pending.store(false, Ordering::SeqCst);
        post_live(&gateway, &ProviderKind::Claude, summary).await;
        assert_eq!(gateway.posts(), ["-# ✅ 완료"]);
        assert!(gateway.edited.lock().unwrap().is_empty());
    }
}

#[tokio::test(flavor = "current_thread")]
async fn o_ownership_and_unknown_identity_suppress_summary_without_consuming_legacy_latch() {
    use crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui;
    let gateway = Gateway::default();
    let summary = summary(586_005, 1, "ownership");
    {
        let _boot =
            crate::services::tui_o::cutover::test_override::force_channels(&[(586_005, ClaudeTui)]);
        post_live(&gateway, &ProviderKind::Claude, summary).await;
        let mut unknown = summary;
        unknown.runtime_kind = None;
        post_live(&gateway, &ProviderKind::Claude, unknown).await;
        assert!(gateway.posts().is_empty());
    }
    let _boot = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    post_live(&gateway, &ProviderKind::Claude, summary).await;
    assert_eq!(gateway.posts(), ["-# ✅ 완료"]);
}

#[test]
fn native_terminal_outcome_uses_cancel_then_failure_without_treating_tool_failure_as_turn_failure()
{
    for (native, cancelled, expected) in [
        (
            r#"{"type":"result","is_error":true}"#,
            false,
            Outcome::Failed,
        ),
        (
            r#"{"type":"assistant","isApiErrorMessage":true}"#,
            false,
            Outcome::Failed,
        ),
        (
            "{\"type\":\"assistant\",\"isApiErrorMessage\":true}\n{\"type\":\"assistant\"}",
            false,
            Outcome::Completed,
        ),
        (
            r#"{"type":"user","message":{"content":[{"type":"tool_result","is_error":true}]}}"#,
            false,
            Outcome::Completed,
        ),
        (
            r#"{"type":"result","is_error":true}"#,
            true,
            Outcome::Cancelled,
        ),
    ] {
        let mut facts = WatcherOutcome::default();
        for line in native.lines() {
            facts.observe(&serde_json::from_str(line).unwrap());
        }
        assert_eq!(
            if cancelled {
                Outcome::Cancelled
            } else {
                facts.outcome()
            },
            expected
        );
    }
}

#[cfg(unix)]
#[test]
fn watcher_snapshot_uses_the_captured_episode_and_rejects_a_successor_row() {
    let _isolation = Isolation::new();
    let shared = super::super::make_shared_data_for_tests();
    let mut first = row(586_006, 0, "captured");
    first.tmux_session_name = Some("fixture-summary".into());
    let key =
        crate::services::discord::tmux::tmux_watcher::completion_producer::watcher_completion_key(
            &shared,
            ChannelId::new(586_006),
            Some(&first),
            "fixture-summary",
            30,
        )
        .unwrap();
    assert!(
        LiveSummary::watcher(&shared, Some(key), Some(&first), false, Outcome::Completed).is_some()
    );
    let mut successor = first.clone();
    successor.turn_nonce = Some("successor".into());
    crate::services::discord::inflight::save_inflight_state(&successor).unwrap();
    assert!(
        LiveSummary::watcher(&shared, Some(key), Some(&first), false, Outcome::Completed).is_some()
    );
    assert!(
        LiveSummary::watcher(
            &shared,
            Some(key),
            Some(&successor),
            false,
            Outcome::Completed
        )
        .is_none()
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn real_watcher_producer_posts_body_then_one_summary_for_retired_flags_and_terminal_outcomes()
{
    let _isolation = Isolation::new();
    let _boot = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    assert!(crate::services::discord::footer_retirement::retired());
    let mut channel = 586_201;
    for v2 in [false, true] {
        for single in [false, true] {
            for (native, expected) in [
                (
                    "{\"type\":\"result\",\"is_error\":false}\n",
                    "-# ✅ · 📦 45%",
                ),
                (
                    "{\"type\":\"user\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"[Request interrupted by user]\"}]}}\n",
                    "-# ⏹ 중단 · 📦 45%",
                ),
                (
                    "{\"type\":\"result\",\"is_error\":true}\n",
                    "-# ⚠️ 오류 · 📦 45%",
                ),
            ] {
                let mut shared = super::super::make_shared_data_for_tests();
                Arc::get_mut(&mut shared)
                    .unwrap()
                    .ui
                    .status_panel_v2_enabled = v2;
                let channel_id = ChannelId::new(channel);
                let _mailbox = shared.mailbox(channel_id);
                shared
                    .ui
                    .placeholder_live_events
                    .set_context_panel_usage(channel_id, None, 100, 200, 150, 1000, 80);
                let mut state = row(channel, 0, "watcher");
                state.tmux_session_name = Some("fixture-summary".into());
                let key = crate::services::discord::tmux::tmux_watcher::completion_producer::watcher_completion_key(
                    &shared,
                    channel_id,
                    Some(&state),
                    "fixture-summary",
                    30,
                )
                .unwrap();
                let summary = LiveSummary::watcher(
                    &shared,
                    Some(key),
                    Some(&state),
                    false,
                    parsed_watcher_outcome(native).0.outcome(),
                )
                .unwrap();
                let recorder = super::super::recovery_engine::o_cut_recorder::start(channel).await;
                let gateway = DiscordGateway::new(
                    recorder.http.clone(),
                    shared.clone(),
                    ProviderKind::Claude,
                    None,
                );
                let body = super::super::formatting::build_streaming_placeholder_text(
                    "answer",
                    "⠸ 계속 처리 중",
                );
                gateway.send_message(channel_id, &body).await.unwrap();
                let mode = super::super::single_message_panel::footer_mode_enabled(single, v2);
                assert!(!mode);
                let mut spin = 0;
                let mut last = String::new();
                for _ in 0..2 {
                    crate::services::discord::tmux::tmux_watcher::completion_producer::complete_watcher_terminal_footer_or_status_panel_with_sniffer(
                &recorder.http,&shared,channel_id,&ProviderKind::Claude,0,mode,&mut spin,None,None,"answer",None,&mut last,None,None,|_|async{false},None,false,false,false,Some(summary),None,
            ).await;
                }
                assert_eq!(
                    recorder.contents(),
                    ["answer", expected],
                    "v2={v2} single={single}"
                );
                channel += 1;
            }
        }
    }
}

#[cfg(unix)]
fn parsed_watcher_outcome(native: &str) -> (WatcherOutcome, String) {
    let mut buffer = native.to_string();
    let mut state = crate::services::session_backend::StreamLineState::new();
    let mut response = "answer".to_string();
    let mut tools = super::super::tmux::WatcherToolState::new();
    super::super::tmux::process_watcher_lines_for_turn(
        &mut buffer,
        &mut state,
        &mut response,
        &mut tools,
        Some(100),
        Some(100),
    );
    (tools.summary_outcome, buffer)
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn watcher_native_outcome_stops_at_the_consumed_turn_before_followup_errors_or_aborts() {
    let _isolation = Isolation::new();
    let _boot = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let followup = "{\"type\":\"user\",\"message\":{\"content\":\"next question\"}}\n{\"type\":\"result\",\"is_error\":true}\n{\"type\":\"user\",\"message\":{\"content\":\"[Request interrupted by user]\"}}\n";
    for (index, (own, outcome, expected)) in [
        ("{\"type\":\"result\",\"is_error\":false}\n", Outcome::Completed, "-# ✅ 완료"),
        ("{\"type\":\"result\",\"is_error\":true}\n", Outcome::Failed, "-# ⚠️ 오류"),
        ("{\"type\":\"assistant\",\"isApiErrorMessage\":true}\n{\"type\":\"result\",\"is_error\":false}\n", Outcome::Failed, "-# ⚠️ 오류"),
        ("{\"type\":\"user\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"[Request interrupted by user]\"}]}}\n", Outcome::Cancelled, "-# ⏹ 중단"),
    ].into_iter().enumerate() {
        let native = format!("{own}{followup}");
        let (facts, remaining) = parsed_watcher_outcome(&native);
        assert_eq!(facts.outcome(), outcome);
        // User-boundary evidence is put back for the next turn; it still cancels this turn.
        assert_eq!(remaining, if outcome == Outcome::Cancelled { native.as_str() } else { followup });
        let channel = 586_300 + index as u64;
        let shared = super::super::make_shared_data_for_tests();
        let _mailbox = shared.mailbox(ChannelId::new(channel));
        let state = row(channel, 1, "parsed");
        let key = summary(channel, 1, "parsed").key;
        let snapshot = LiveSummary::watcher(&shared, Some(key), Some(&state), false, facts.outcome()).unwrap();
        let recorder = super::super::recovery_engine::o_cut_recorder::start(channel).await;
        let gateway = DiscordGateway::new(recorder.http.clone(), shared.clone(), ProviderKind::Claude, None);
        gateway.send_message(ChannelId::new(channel), "answer").await.unwrap();
        let mut spin = 0;
        let mut last = String::new();
        crate::services::discord::tmux::tmux_watcher::completion_producer::complete_watcher_terminal_footer_or_status_panel_with_sniffer(
            &recorder.http, &shared, ChannelId::new(channel), &ProviderKind::Claude, 0, false, &mut spin,
            None, None, "answer", None, &mut last, None, None, |_| async { false }, None,
            false, false, false, Some(snapshot), None,
        ).await;
        assert_eq!(recorder.contents(), ["answer", expected]);
    }
}

#[test]
fn future_watcher_start_is_omitted_before_submillisecond_duration_truncation() {
    let at = chrono::DateTime::parse_from_rfc3339("2026-10-19T00:00:00Z")
        .unwrap()
        .naive_utc();
    assert_eq!(
        elapsed_since(at + chrono::Duration::nanoseconds(1), at),
        None
    );
    assert_eq!(
        elapsed_since(at, at + chrono::Duration::nanoseconds(1)),
        Some(0)
    );
    assert_eq!(
        elapsed_since(at, at + chrono::Duration::milliseconds(1234)),
        Some(1234)
    );
}
