//! Host guard on the auto-retry reset: the session-died recovery handler, and a resume
//! failure driven through terminal delivery and the completion postlude.

use std::sync::atomic::{AtomicUsize, Ordering};

use super::super::recovery_retry::{
    RecoveryRetryContext, RecoveryRetryMessage, RecoveryRetryOutcome, RecoveryRetryState,
    handle_recovery_retry,
};
use super::*;
use crate::db::dispatched_sessions::hosted_execution::HostedState;
use crate::db::dispatched_sessions::hosted_execution::tests::{owner, record, wire};
use crate::services::discord::inflight::seed_session_row;
use crate::services::provider::CancelToken;

/// Counts retry-with-history scheduling and records the placeholder edits.
struct RetryCounter(Arc<AtomicUsize>, Arc<Mutex<Vec<String>>>);

impl TurnGateway for RetryCounter {
    fn send_message<'a>(
        &'a self,
        _channel_id: ChannelId,
        _content: &'a str,
    ) -> GatewayFuture<'a, Result<MessageId, String>> {
        panic!("recovery retry must not send a message")
    }

    fn edit_message<'a>(
        &'a self,
        _channel_id: ChannelId,
        _message_id: MessageId,
        content: &'a str,
    ) -> GatewayFuture<'a, Result<(), String>> {
        self.1.lock().unwrap().push(content.to_string());
        Box::pin(async { Ok(()) })
    }

    fn replace_message_with_outcome<'a>(
        &'a self,
        _channel_id: ChannelId,
        _message_id: MessageId,
        _content: &'a str,
    ) -> GatewayFuture<'a, Result<ReplaceLongMessageOutcome, String>> {
        panic!("recovery retry must not replace a message")
    }

    fn schedule_retry_with_history<'a>(
        &'a self,
        _channel_id: ChannelId,
        _user_message_id: MessageId,
        _user_text: &'a str,
    ) -> GatewayFuture<'a, ()> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {})
    }

    fn dispatch_queued_turn<'a>(
        &'a self,
        _channel_id: ChannelId,
        _intervention: &'a Intervention,
        _request_owner_name: &'a str,
        _has_more_queued_turns: bool,
        _dispatch_lease: Option<Arc<crate::services::turn_orchestrator::DispatchLease>>,
    ) -> GatewayFuture<'a, Result<(), String>> {
        panic!("recovery retry must not dispatch a queued turn")
    }

    fn validate_live_routing<'a>(
        &'a self,
        _channel_id: ChannelId,
    ) -> GatewayFuture<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }

    fn requester_mention(&self) -> Option<String> {
        None
    }

    fn can_chain_locally(&self) -> bool {
        false
    }

    fn can_deliver_directly(&self) -> bool {
        true
    }

    fn bot_owner_provider(&self) -> Option<ProviderKind> {
        Some(ProviderKind::Claude)
    }
}

fn resumable_session() -> crate::services::discord::DiscordSession {
    crate::services::discord::DiscordSession {
        session_id: Some("sid-core".to_string()),
        memento_context_loaded: false,
        memento_reflected: false,
        current_path: None,
        history: Vec::new(),
        pending_uploads: Vec::new(),
        cleared: false,
        remote_profile_name: None,
        channel_id: None,
        channel_name: None,
        category_name: None,
        last_active: tokio::time::Instant::now(),
        worktree: None,
        born_generation: 0,
    }
}

// A session that died during restart recovery, with a user message to retry: only a
// turn whose own key finds a legacy row loses its resume ids, is killed and is requeued.
#[tokio::test]
async fn session_died_recovery_retries_only_a_session_the_host_guard_admits_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let bound = wire(&record(
        &owner("1479671301387059502"),
        "n1",
        HostedState::Bound,
    ));
    let legacy = seed_session_row(&pool, "p4c3w1-retry-legacy", 1479671301387059501, None);
    let legacy = legacy.await;
    let bound = seed_session_row(
        &pool,
        "p4c3w1-retry-bound",
        1479671301387059502,
        Some(bound),
    );
    let bound = bound.await;
    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let cases = [
        (Some(legacy), "p4c3w1-retry-legacy", 501, true),
        (Some(bound), "p4c3w1-retry-bound", 502, false),
        (None, "p4c3w1-retry-no-key", 503, false),
    ];
    for (key, name, channel, admitted) in cases {
        let channel_id = ChannelId::new(1479671301387059000 + channel);
        let session = resumable_session();
        shared
            .core
            .lock()
            .await
            .sessions
            .insert(channel_id, session);
        let token = Arc::new(CancelToken::new());
        token.bind_unmanaged_session_name(name);
        let scheduled = Arc::new(AtomicUsize::new(0));
        let gateway: Arc<dyn TurnGateway> =
            Arc::new(RetryCounter(scheduled.clone(), Arc::default()));
        let (mut sid, mut raw) = (Some("sid".to_string()), Some("raw".to_string()));
        let mut full_response = "partial".to_string();
        let mut inflight = InflightTurnState::new(
            ProviderKind::Claude,
            channel_id.get(),
            None,
            1,
            7,
            8,
            "retry me".to_string(),
            Some("sid".to_string()),
            Some(name.to_string()),
            None,
            None,
            0,
        );
        let text = "retry me".to_string();
        let outcome = handle_recovery_retry(
            RecoveryRetryMessage::SessionDiedDuringRecovery,
            RecoveryRetryContext {
                shared_owned: &shared,
                gateway: &gateway,
                cancel_token: &token,
                channel_id,
                user_msg_id: Some(MessageId::new(7)),
                current_msg_id: MessageId::new(8),
                adk_session_key: &key,
                user_text_owned: &text,
            },
            RecoveryRetryState {
                full_response: &mut full_response,
                new_session_id: &mut sid,
                new_raw_provider_session_id: &mut raw,
                inflight_state: &mut inflight,
                auto_retry: &mut AutoRetry::default(),
            },
        )
        .await;
        assert_eq!(outcome, RecoveryRetryOutcome::Continue, "{name}");
        // Scheduling runs on a spawned task; a refused turn never spawns one.
        for _ in 0..200 {
            if scheduled.load(Ordering::SeqCst) > 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let kept = |id: &str| (!admitted).then(|| id.to_string());
        assert_eq!((sid, raw), (kept("sid"), kept("raw")), "{name}");
        assert_eq!(inflight.session_id, kept("sid"), "{name}");
        let core_sid = shared.core.lock().await.sessions[&channel_id]
            .session_id
            .clone();
        assert_eq!(core_sid, kept("sid-core"), "{name}");
        let requeued = scheduled.load(Ordering::SeqCst);
        assert_eq!(requeued, usize::from(admitted), "{name}");
        let exit_reason = crate::services::tmux_common::session_temp_path(name, "exit_reason");
        let killed = std::path::Path::new(&exit_reason).exists();
        assert_eq!(killed, admitted, "{name}");
    }
    pool.close().await;
    db.drop().await;
}

/// The stored row behind the turn's own session key.
#[derive(Clone, Copy, Debug)]
enum StoredRow {
    Legacy,
    Bound,
    Missing,
}

/// What a resume-failure turn left behind once the completion postlude finished.
#[derive(Debug, PartialEq, Eq)]
struct TurnEnd {
    requeued: usize,
    continue_notice: bool,
    inflight_kept: bool,
    core_sid: Option<String>,
    /// DB provider-id clears issued by terminal delivery, and by the whole turn.
    db_clears: (usize, usize),
    persisted_sid: bool,
}

type ApiCalls = Arc<Mutex<Vec<(String, String)>>>;

// A resume failure reported in the response output, driven through terminal delivery and
// then the completion postlude with the same values the bridge threads between them.
async fn resume_failure_turn(row: StoredRow, with_user_message: bool, api: &ApiCalls) -> TurnEnd {
    // The driver holds the shared test-env lock, which comes before the database lock.
    let mut driver = TerminalDeliveryDriver::new(ReplaceBehaviour::Edited, 1);
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    Arc::get_mut(&mut driver.shared).unwrap().pg_pool = Some(pool.clone());
    let shared = driver.shared.clone();
    let channel_id = ChannelId::new(DRIVER_CHANNEL_ID);
    let mut session = resumable_session();
    session.channel_name = Some("p4c3w1-resume".to_string());
    shared
        .core
        .lock()
        .await
        .sessions
        .insert(channel_id, session);
    let _mailbox = shared.mailbox(channel_id);
    // The channel's own key, so completion owns the channel session effects.
    let key = crate::services::discord::adk_session::build_adk_session_key(
        &shared,
        channel_id,
        &ProviderKind::Claude,
        None,
    );
    let key = key.await.expect("channel session key");
    let bound = wire(&record(
        &owner(&DRIVER_CHANNEL_ID.to_string()),
        "n1",
        HostedState::Bound,
    ));
    let raw = match row {
        StoredRow::Legacy => Some(None),
        StoredRow::Bound => Some(Some(bound)),
        StoredRow::Missing => None,
    };
    if let Some(raw) = raw {
        crate::services::discord::inflight::seed_session_row_keyed(
            &pool,
            &key,
            DRIVER_CHANNEL_ID,
            raw,
        )
        .await;
    }

    let scheduled = Arc::new(AtomicUsize::new(0));
    let edits = Arc::new(Mutex::new(Vec::new()));
    let (mut ctx, mut state) = driver.parts();
    ctx.user_msg_id = with_user_message.then_some(MessageId::new(DRIVER_USER_MSG_ID));
    state.gateway = Arc::new(RetryCounter(scheduled.clone(), edits.clone()));
    state
        .cancel_token
        .bind_unmanaged_session_name(DRIVER_TMUX_SESSION);
    state.adk_session_key = Some(key);
    state.resume_failure_detected = true;
    state.full_response = "No conversation found with session ID".to_string();
    state.new_session_id = Some("sid-turn".to_string());
    state.new_raw_provider_session_id = Some("raw-turn".to_string());
    state.inflight_state.session_id = Some("sid-turn".to_string());
    state.terminal_full_replay_cleanup_msg_ids.clear();
    let output = tokio::time::timeout(DRIVER_TIMEOUT, run_terminal_outcome_delivery(ctx, state));
    let output = output.await.expect("terminal delivery finishes");
    // Scheduling runs on a spawned task; a turn that queued nothing never spawns one.
    for _ in 0..200 {
        if scheduled.load(Ordering::SeqCst) > 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    let clears = |api: &ApiCalls| {
        let calls = api.lock().unwrap();
        let clear = "/api/dispatched-sessions/clear-session-id";
        calls.iter().filter(|(path, _)| path == clear).count()
    };
    let terminal_clears = clears(api);
    run_bridge_postlude(&driver, output).await;

    let core_sid = shared.core.lock().await.sessions[&channel_id]
        .session_id
        .clone();
    let inflight = crate::services::discord::inflight::load_inflight_state_read_only(
        &ProviderKind::Claude,
        DRIVER_CHANNEL_ID,
    );
    let persisted_sid = api.lock().unwrap().iter().any(|(path, body)| {
        path == "/api/dispatched-sessions/webhook" && body.contains("\"sid-turn\"")
    });
    let end = TurnEnd {
        requeued: scheduled.load(Ordering::SeqCst),
        continue_notice: edits
            .lock()
            .unwrap()
            .iter()
            .any(|e| e.contains("자동으로 이어갑니다")),
        inflight_kept: inflight.is_some(),
        core_sid,
        db_clears: (terminal_clears, clears(api)),
        persisted_sid,
    };
    api.lock().unwrap().clear();
    pool.close().await;
    db.drop().await;
    drop(driver);
    end
}

// Mirrors how the bridge hands terminal delivery's output to the completion postlude.
#[rustfmt::skip]
async fn run_bridge_postlude(driver: &TerminalDeliveryDriver, output: TerminalOutcomeDeliveryOutput) {
    use super::super::super::{completion_postlude as postlude, guards};
    let channel_id = ChannelId::new(DRIVER_CHANNEL_ID);
    let (_, rx) = std::sync::mpsc::channel();
    let fence = tokio::sync::OnceCell::new();
    let _ = super::super::super::capture_bridge_clear_fence(&driver.shared, channel_id, rx, &fence).await;
    let user_id = output.inflight_state.user_msg_id;
    let mut completion_guard = guards::CompletionGuard::for_completion_test(driver.shared.clone(), channel_id, user_id);
    output.handoff_completion_authority(&mut completion_guard);
    let inflight_guard = guards::InflightCleanupGuard::for_completion_test(&output.inflight_state, driver.shared.token_hash.clone());
    let ctx = postlude::CompletionPostludeContext {
        shared_owned: output.shared_owned, gateway: output.gateway, channel_id,
        provider: output.provider, cancel_token: output.cancel_token,
        user_msg_id: (user_id != 0).then(|| MessageId::new(user_id)), turn_id: output.turn_id,
        request_owner_name: String::new(), final_session_status: "idle", status_panel_started_at: 0,
        has_queued_turns: false, defer_watcher_resume: true, can_chain_locally: true,
        single_message_panel_footer_mode: false, is_external_input_tui_direct: false,
        context_window_tokens: 0, context_compact_percent: 0,
        clear_fence: fence.into_inner().unwrap(), turn_start: output.turn_start,
    };
    let state = postlude::CompletionPostludeState {
        watcher_delivery_pin: driver.parts().0.watcher_delivery_pin,
        full_response: output.full_response, user_text_owned: output.user_text_owned,
        role_binding: None, adk_session_key: output.adk_session_key, adk_session_name: None,
        adk_session_info: None, adk_cwd: output.adk_cwd, dispatch_id: output.dispatch_id,
        dispatch_kind: None, new_session_id: output.new_session_id,
        new_raw_provider_session_id: output.new_raw_provider_session_id,
        status_panel_terminal_committed: output.status_panel_terminal_committed,
        bridge_should_emit_completion: output.bridge_should_emit_completion,
        current_msg_id: MessageId::new(DRIVER_CURRENT_MSG_ID), status_panel_msg_id: None,
        last_status_panel_text: String::new(),
        completion_footer_terminal_text: output.completion_footer_terminal_text,
        busy_requeue_outcome: output.busy_requeue_outcome, auto_retry: output.auto_retry,
        spin_idx: 0, status_panel_generation: 0,
        preserve_inflight_for_cleanup_retry: output.preserve_inflight_for_cleanup_retry,
        tmux_last_offset: None, watcher_owner_channel_id: channel_id,
        bridge_relay_delegated_to_watcher: false, is_prompt_too_long: false,
        resume_failure_detected: output.resume_failure_detected, recovery_retry: false,
        rx_disconnected: false, tmux_handed_off: false, bridge_output_owner: None,
        terminal_delivery_committed: output.terminal_delivery_committed,
        terminal_session_reset_required: false, transcript_events: Vec::new(),
        accumulated_input_tokens: 0, accumulated_cache_create_tokens: 0,
        accumulated_cache_read_tokens: 0, accumulated_output_tokens: 0,
        accumulated_memory_input_tokens: 0, accumulated_memory_output_tokens: 0,
        transport_error: false, api_friction_reports: output.api_friction_reports, cancelled: false,
        restart_followup_pending: None,
        bridge_skip_holder_owns_inflight: output.bridge_skip_holder_owns_inflight,
        completion_guard, inflight_guard, inflight_state: output.inflight_state,
    };
    tokio::time::timeout(DRIVER_TIMEOUT, postlude::run_completion_postlude(ctx, state)).await.unwrap();
}

// A kept session keeps its core and stored provider session id through completion and
// takes the no-retry branch: inflight preserved, no auto-continue notice, nothing queued.
#[test]
fn kept_resume_failure_keeps_the_session_id_and_inflight_through_completion_pg() {
    const CHILD: &str = "ADK_P4C3W1_RESUME_FAILURE_CHILD";
    if std::env::var_os(CHILD).is_none() {
        // The direct API context is process-global, so the fake API runs in a fresh process.
        let root = tempfile::tempdir().unwrap();
        let name = format!(
            "{}::kept_resume_failure_keeps_the_session_id_and_inflight_through_completion_pg",
            module_path!().split_once("::").unwrap().1
        );
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &name, "--nocapture", "--test-threads=1"])
            .env(CHILD, "1")
            .env("AGENTDESK_ROOT_DIR", root.path())
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&result.stdout);
        let stderr = String::from_utf8_lossy(&result.stderr);
        assert!(result.status.success(), "{stdout}\n{stderr}");
        assert!(stdout.contains("1 passed"), "{stdout}\n{stderr}");
        return;
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build();
    let _boot = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    runtime.unwrap().block_on(async {
        let api: ApiCalls = Arc::default();
        let captured = api.clone();
        let app = axum::Router::new().fallback(move |uri: axum::http::Uri, body: String| {
            let captured = captured.clone();
            async move {
                captured
                    .lock()
                    .unwrap()
                    .push((uri.path().to_string(), body));
                axum::Json(serde_json::json!({}))
            }
        });
        let loopback = crate::config::loopback();
        let listener = tokio::net::TcpListener::bind((loopback.as_str(), 0)).await;
        let listener = listener.unwrap();
        let port = listener.local_addr().unwrap().port();
        crate::services::discord::internal_api::init(port, None);
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let cleared = resume_failure_turn(StoredRow::Legacy, true, &api).await;
        assert_eq!(cleared.requeued, 1, "{cleared:?}");
        assert!(
            cleared.continue_notice && !cleared.inflight_kept,
            "{cleared:?}"
        );
        assert_eq!(cleared.core_sid, None, "{cleared:?}");
        let (terminal, total) = cleared.db_clears;
        assert!(terminal > 0 && total == 2 * terminal, "{cleared:?}");

        // The existing branch for a turn with no message to retry.
        let no_retry = resume_failure_turn(StoredRow::Legacy, false, &api).await;
        assert_eq!(no_retry.requeued, 0, "{no_retry:?}");
        assert!(
            !no_retry.continue_notice && no_retry.inflight_kept,
            "{no_retry:?}"
        );
        assert_eq!(no_retry.core_sid, None, "{no_retry:?}");

        for row in [StoredRow::Bound, StoredRow::Missing] {
            let kept = resume_failure_turn(row, true, &api).await;
            let branch = (kept.requeued, kept.continue_notice, kept.inflight_kept);
            let no_retry_branch = (0, false, true);
            assert_eq!(branch, no_retry_branch, "{row:?}: {kept:?}");
            assert_eq!(
                kept.core_sid.as_deref(),
                Some("sid-turn"),
                "{row:?}: {kept:?}"
            );
            assert_eq!(kept.db_clears, (0, 0), "{row:?}: {kept:?}");
            assert!(kept.persisted_sid, "{row:?}: {kept:?}");
        }
        server.abort();
    });
}
