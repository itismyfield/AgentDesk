use super::advance_tmux_relay_confirmed_end;
use super::completion_guard::{
    build_verdict_payload, extract_explicit_review_verdict, extract_explicit_work_outcome,
    extract_review_decision,
};
use super::context_window::{
    persisted_context_tokens, resolve_done_response, total_context_tokens,
};
use super::memory_lifecycle::{
    PROVIDER_SESSION_ASSISTANT_TURN_CAP, TurnEndMemoryPlan, optional_metric_token_fields,
    plan_turn_end_memory, take_memento_reflect_request,
};
use super::recovery_text::{
    build_session_retry_context_from_history, store_session_retry_context,
    store_session_retry_context_with_notify, take_session_retry_context,
};
use super::retry_state::{
    clear_local_session_state, clear_response_delivery_state, handle_gemini_retry_boundary,
    reset_gemini_retry_attempt_state, should_reset_gemini_retry_attempt_state,
    sync_response_delivery_state,
};
use super::skill_usage::extract_skill_id_from_tool_use;
use super::stale_resume::{
    contains_stale_resume_error_text, output_file_has_stale_resume_error_after_offset,
    result_event_has_stale_resume_error, stream_error_requires_terminal_session_reset,
};
use super::tmux_runtime::should_resume_watcher_after_turn;
use crate::db::turns::TurnTokenUsage;
use crate::services::agent_protocol::StreamMessage;
use crate::services::discord::ChannelId;
use crate::services::discord::DiscordSession;
use crate::services::discord::InflightTurnState;
use crate::services::discord::MessageId;
use crate::services::discord::gateway::HeadlessGateway;
use crate::services::discord::make_shared_data_for_tests;
use crate::services::discord::make_shared_data_for_tests_with_storage;
use crate::services::discord::settings::{MemoryBackendKind, ResolvedMemorySettings};
use crate::services::memory::{SessionEndReason, TokenUsage};
use crate::services::provider::{CancelToken, ProviderKind};
use crate::ui::ai_screen::{HistoryItem, HistoryType};
use std::io::Write;
use std::sync::{Arc, atomic::Ordering};
use std::time::{Duration, Instant};

#[test]
fn chained_batch_mid_turn_keeps_watcher_paused() {
    assert!(!should_resume_watcher_after_turn(true, false, false));
}

#[test]
fn locally_chainable_queue_keeps_watcher_paused() {
    assert!(!should_resume_watcher_after_turn(false, true, true));
}

#[test]
fn final_turn_without_remaining_queue_resumes_watcher() {
    assert!(should_resume_watcher_after_turn(false, false, true));
}

#[test]
fn persisted_context_tokens_uses_input_only() {
    // input_tokens represents full context window occupancy; output is excluded
    assert_eq!(persisted_context_tokens(610_000, 90_000), Some(610_000));
    assert_eq!(persisted_context_tokens(0, 0), None);
}

#[test]
fn total_context_tokens_saturates_on_overflow() {
    assert_eq!(total_context_tokens(u64::MAX, 1), u64::MAX);
}

#[test]
fn optional_metric_token_fields_omit_all_zero_usage() {
    assert_eq!(
        optional_metric_token_fields(TokenUsage::default()),
        (None, None)
    );
}

#[test]
fn optional_metric_token_fields_preserve_partial_usage() {
    assert_eq!(
        optional_metric_token_fields(TokenUsage {
            input_tokens: 13,
            output_tokens: 0,
        }),
        (Some(13), None)
    );
    assert_eq!(
        optional_metric_token_fields(TokenUsage {
            input_tokens: 0,
            output_tokens: 5,
        }),
        (None, Some(5))
    );
}

#[test]
fn advance_tmux_relay_confirmed_end_updates_shared_floor_monotonically() {
    let shared = make_shared_data_for_tests();
    let channel_id = ChannelId::new(1486333430516945999);

    advance_tmux_relay_confirmed_end(shared.as_ref(), channel_id, Some(128));
    let relay_coord = shared.tmux_relay_coord(channel_id);
    assert_eq!(
        relay_coord.confirmed_end_offset.load(Ordering::Acquire),
        128
    );

    advance_tmux_relay_confirmed_end(shared.as_ref(), channel_id, Some(64));
    assert_eq!(
        relay_coord.confirmed_end_offset.load(Ordering::Acquire),
        128
    );

    advance_tmux_relay_confirmed_end(shared.as_ref(), channel_id, None);
    assert_eq!(
        relay_coord.confirmed_end_offset.load(Ordering::Acquire),
        128
    );
}

#[tokio::test]
async fn active_turn_output_offset_refreshes_session_heartbeat_before_done() {
    let db = crate::db::test_db();
    let shared = make_shared_data_for_tests_with_storage(Some(db.clone()), None);
    let provider = ProviderKind::Codex;
    let channel_id = ChannelId::new(1485506232256168011);
    let channel_name = format!("adk-cdx-t{}", channel_id.get());
    let tmux_name = provider.build_tmux_session_name(&channel_name);
    let session_key = crate::services::discord::adk_session::build_namespaced_session_key(
        &shared.token_hash,
        &provider,
        &tmux_name,
    );
    let thread_channel_id = channel_id.get().to_string();

    db.lock()
        .expect("test db lock")
        .execute(
            "INSERT INTO sessions
             (session_key, provider, status, thread_channel_id, last_heartbeat, created_at)
             VALUES (?1, ?2, 'working', ?3, '2026-04-09 01:02:03', '2026-04-09 01:02:03')",
            [
                session_key.as_str(),
                provider.as_str(),
                thread_channel_id.as_str(),
            ],
        )
        .expect("insert session row");

    let (stream_tx, stream_rx) = std::sync::mpsc::channel();
    let (completion_tx, completion_rx) = tokio::sync::oneshot::channel();
    let mut inflight_state = InflightTurnState::new(
        provider.clone(),
        channel_id.get(),
        Some(channel_name.clone()),
        343742347365974026,
        1487795113240559788,
        1487799916758827138,
        "ping".to_string(),
        None,
        Some(tmux_name),
        Some("/tmp/agentdesk-test-output.jsonl".to_string()),
        Some("/tmp/agentdesk-test-input.fifo".to_string()),
        0,
    );
    inflight_state.session_key = Some(session_key.clone());

    super::spawn_turn_bridge(
        shared.clone(),
        Arc::new(CancelToken::new()),
        stream_rx,
        super::TurnBridgeContext {
            provider: provider.clone(),
            gateway: Arc::new(HeadlessGateway),
            channel_id,
            user_msg_id: MessageId::new(1487795113240559788),
            user_text_owned: "ping".to_string(),
            request_owner_name: "tester".to_string(),
            role_binding: None,
            adk_session_key: Some(session_key.clone()),
            adk_session_name: Some(channel_name),
            adk_session_info: None,
            adk_cwd: None,
            dispatch_id: None,
            memory_recall_usage: TokenUsage::default(),
            current_msg_id: MessageId::new(1487799916758827138),
            response_sent_offset: 0,
            full_response: String::new(),
            tmux_last_offset: Some(0),
            new_session_id: None,
            defer_watcher_resume: false,
            completion_tx: Some(completion_tx),
            inflight_state,
        },
    );

    stream_tx
        .send(StreamMessage::OutputOffset { offset: 128 })
        .expect("send output offset");

    let heartbeat_changed = async {
        loop {
            let last_heartbeat: String = db
                .lock()
                .expect("test db lock")
                .query_row(
                    "SELECT last_heartbeat FROM sessions WHERE session_key = ?1",
                    [session_key.as_str()],
                    |row| row.get(0),
                )
                .expect("select last heartbeat");
            if last_heartbeat != "2026-04-09 01:02:03" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(4), heartbeat_changed)
        .await
        .expect("active turn output offset should refresh last_heartbeat");

    stream_tx
        .send(StreamMessage::Text {
            content: "done".to_string(),
        })
        .expect("send text");
    stream_tx
        .send(StreamMessage::Done {
            result: String::new(),
            session_id: None,
        })
        .expect("send done");
    drop(stream_tx);

    tokio::time::timeout(Duration::from_secs(5), completion_rx)
        .await
        .expect("turn bridge should finish")
        .expect("completion sender should complete");
}

#[test]
fn active_turn_activity_heartbeat_refreshes_once_per_interval_window() {
    let db = crate::db::test_db();
    let shared = make_shared_data_for_tests_with_storage(Some(db.clone()), None);
    let provider = ProviderKind::Codex;
    let channel_id = ChannelId::new(1485506232256168022);
    let channel_name = format!("adk-cdx-t{}", channel_id.get());
    let tmux_name = provider.build_tmux_session_name(&channel_name);
    let session_key = crate::services::discord::adk_session::build_namespaced_session_key(
        &shared.token_hash,
        &provider,
        &tmux_name,
    );
    let thread_channel_id = channel_id.get().to_string();

    let conn = db.lock().expect("test db lock");
    conn.execute(
        "INSERT INTO sessions
         (session_key, provider, status, thread_channel_id, last_heartbeat, created_at)
         VALUES (?1, ?2, 'working', ?3, '2026-04-09 01:02:03', '2026-04-09 01:02:03')",
        [
            session_key.as_str(),
            provider.as_str(),
            thread_channel_id.as_str(),
        ],
    )
    .expect("insert session row");
    conn.execute(
        "CREATE TABLE heartbeat_audit (
             id INTEGER PRIMARY KEY,
             session_key TEXT NOT NULL,
             last_heartbeat TEXT
         )",
        [],
    )
    .expect("create heartbeat audit table");
    conn.execute(
        "CREATE TRIGGER heartbeat_audit_after_update
         AFTER UPDATE OF last_heartbeat ON sessions
         BEGIN
             INSERT INTO heartbeat_audit (session_key, last_heartbeat)
             VALUES (new.session_key, new.last_heartbeat);
         END",
        [],
    )
    .expect("create heartbeat audit trigger");
    drop(conn);

    let mut inflight_state = InflightTurnState::new(
        provider.clone(),
        channel_id.get(),
        Some(channel_name.clone()),
        343742347365974026,
        1487795113240559788,
        1487799916758827138,
        "ping".to_string(),
        None,
        Some(tmux_name),
        Some("/tmp/agentdesk-test-output.jsonl".to_string()),
        Some("/tmp/agentdesk-test-input.fifo".to_string()),
        0,
    );
    inflight_state.session_key = Some(session_key.clone());

    let mut last_heartbeat_at = None;
    let start = Instant::now();
    for tick in 0..=24 {
        inflight_state.last_offset = tick;
        super::maybe_refresh_active_turn_activity_heartbeat_at(
            shared.as_ref(),
            &provider,
            &inflight_state,
            Some(channel_name.as_str()),
            &mut last_heartbeat_at,
            start + Duration::from_secs(tick * 5),
        );
    }

    let refresh_count: i64 = db
        .lock()
        .expect("test db lock")
        .query_row(
            "SELECT COUNT(*) FROM heartbeat_audit WHERE session_key = ?1",
            [session_key.as_str()],
            |row| row.get(0),
        )
        .expect("count heartbeat refreshes");
    assert_eq!(
        refresh_count, 5,
        "continuous output over two minutes should refresh at t=0,30,60,90,120s"
    );
}

#[test]
fn skill_tool_use_extracts_skill_id_only_from_skill_tool() {
    assert_eq!(
        extract_skill_id_from_tool_use("Skill", r#"{"skill":" /memory-write "}"#),
        Some("/memory-write".to_string())
    );
    assert_eq!(
        extract_skill_id_from_tool_use("Bash", r#"{"skill":"memory-write"}"#),
        None
    );
    assert_eq!(extract_skill_id_from_tool_use("Skill", r#"{}"#), None);
}

#[test]
fn persist_turn_analytics_row_prefers_output_jsonl_usage_from_turn_start_offset() {
    let db = crate::db::test_db();
    let mut file = tempfile::NamedTempFile::new().unwrap();
    writeln!(
        file,
        "{}",
        r#"{"type":"result","subtype":"success","session_id":"old-session","usage":{"input_tokens":999,"cache_creation_input_tokens":99,"cache_read_input_tokens":88,"output_tokens":77},"result":"old turn"}"#
    )
    .unwrap();
    let turn_start_offset = file.as_file().metadata().unwrap().len();
    writeln!(
        file,
        "{}",
        r#"{"type":"system","subtype":"init","session_id":"session-init"}"#
    )
    .unwrap();
    writeln!(
        file,
        "{}",
        r#"{"type":"assistant","message":{"model":"claude-sonnet","usage":{"input_tokens":10,"cache_creation_input_tokens":3,"cache_read_input_tokens":4,"output_tokens":2},"content":[{"type":"text","text":"partial"}]}}"#
    )
    .unwrap();
    writeln!(
        file,
        "{}",
        r#"{"type":"result","subtype":"success","session_id":"session-final","usage":{"input_tokens":100,"cache_creation_input_tokens":20,"cache_read_input_tokens":30,"output_tokens":40},"result":"done"}"#
    )
    .unwrap();
    file.flush().unwrap();
    let end_offset = file.as_file().metadata().unwrap().len();

    let mut inflight_state = InflightTurnState::new(
        ProviderKind::Claude,
        1486333430516945008,
        Some("adk-cc-t1486333430516945008".to_string()),
        343742347365974026,
        1487795113240559788,
        1487799916758827138,
        "turn analytics".to_string(),
        Some("stale-session".to_string()),
        Some("AgentDesk-claude-adk-cc-t1486333430516945008".to_string()),
        Some(file.path().to_str().unwrap().to_string()),
        Some("/tmp/agentdesk-test.input".to_string()),
        turn_start_offset,
    );
    inflight_state.logical_channel_id = Some(1479671301387059200);
    inflight_state.thread_id = Some(1486333430516945008);
    inflight_state.thread_title = Some("[AgentDesk] #593 turns persistence".to_string());
    inflight_state.dispatch_id = Some("dispatch-593".to_string());
    inflight_state.last_offset = end_offset;

    super::persist_turn_analytics_row(
        &db,
        &ProviderKind::Claude,
        ChannelId::new(1486333430516945008),
        MessageId::new(1487795113240559788),
        None,
        Some("dispatch-593"),
        Some("claude/token/host:adk-cdx"),
        Some("stream-session"),
        &inflight_state,
        TurnTokenUsage {
            input_tokens: 1,
            cache_create_tokens: 1,
            cache_read_tokens: 1,
            output_tokens: 1,
        },
        12_000,
    );

    let conn = db.read_conn().unwrap();
    let row = conn
        .query_row(
            "SELECT thread_id, thread_title, channel_id, session_id,
                    input_tokens, cache_create_tokens, cache_read_tokens, output_tokens
             FROM turns
             WHERE turn_id = 'discord:1486333430516945008:1487795113240559788'",
            [],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, i64>(7)?,
                ))
            },
        )
        .unwrap();

    assert_eq!(row.0.as_deref(), Some("1486333430516945008"));
    assert_eq!(row.1.as_deref(), Some("[AgentDesk] #593 turns persistence"));
    assert_eq!(row.2, "1479671301387059200");
    assert_eq!(row.3.as_deref(), Some("session-final"));
    assert_eq!(row.4, 100);
    assert_eq!(row.5, 20);
    assert_eq!(row.6, 30);
    assert_eq!(row.7, 40);
}

fn fetch_persisted_turn_usage(
    sqlite: &crate::db::Db,
) -> Option<(Option<String>, i64, i64, i64, i64)> {
    let conn = sqlite.read_conn().unwrap();
    conn.query_row(
        "SELECT session_id, input_tokens, cache_create_tokens, cache_read_tokens, output_tokens
         FROM turns
         WHERE turn_id = 'discord:1486333430516945008:1487795113240559788'",
        [],
        |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
            ))
        },
    )
    .ok()
}

#[test]
fn persist_turn_analytics_row_snapshots_output_before_background_persist() {
    let db = crate::db::test_db();
    let mut file = tempfile::NamedTempFile::new().unwrap();
    writeln!(
        file,
        "{}",
        r#"{"type":"result","subtype":"success","session_id":"old-session","usage":{"input_tokens":999,"cache_creation_input_tokens":99,"cache_read_input_tokens":88,"output_tokens":77},"result":"old turn"}"#
    )
    .unwrap();
    let turn_start_offset = file.as_file().metadata().unwrap().len();
    writeln!(
        file,
        "{}",
        r#"{"type":"system","subtype":"init","session_id":"session-init"}"#
    )
    .unwrap();
    writeln!(
        file,
        "{}",
        r#"{"type":"result","subtype":"success","session_id":"session-final","usage":{"input_tokens":100,"cache_creation_input_tokens":20,"cache_read_input_tokens":30,"output_tokens":40},"result":"done"}"#
    )
    .unwrap();
    file.flush().unwrap();
    let current_turn_end_offset = file.as_file().metadata().unwrap().len();

    let mut inflight_state = InflightTurnState::new(
        ProviderKind::Claude,
        1486333430516945008,
        Some("adk-cc-t1486333430516945008".to_string()),
        343742347365974026,
        1487795113240559788,
        1487799916758827138,
        "turn analytics".to_string(),
        Some("stale-session".to_string()),
        Some("AgentDesk-claude-adk-cc-t1486333430516945008".to_string()),
        Some(file.path().to_str().unwrap().to_string()),
        Some("/tmp/agentdesk-test.input".to_string()),
        turn_start_offset,
    );
    inflight_state.last_offset = current_turn_end_offset;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .max_blocking_threads(1)
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let blocker = tokio::task::spawn_blocking(move || {
            let _ = release_rx.recv();
        });

        super::persist_turn_analytics_row(
            &db,
            &ProviderKind::Claude,
            ChannelId::new(1486333430516945008),
            MessageId::new(1487795113240559788),
            None,
            Some("dispatch-593"),
            Some("claude/token/host:adk-cdx"),
            Some("stream-session"),
            &inflight_state,
            TurnTokenUsage {
                input_tokens: 1,
                cache_create_tokens: 1,
                cache_read_tokens: 1,
                output_tokens: 1,
            },
            12_000,
        );

        writeln!(
            file,
            "{}",
            r#"{"type":"system","subtype":"init","session_id":"session-next-init"}"#
        )
        .unwrap();
        writeln!(
            file,
            "{}",
            r#"{"type":"result","subtype":"success","session_id":"session-next","usage":{"input_tokens":500,"cache_creation_input_tokens":50,"cache_read_input_tokens":60,"output_tokens":70},"result":"next turn"}"#
        )
        .unwrap();
        file.flush().unwrap();

        release_tx.send(()).unwrap();
        blocker.await.unwrap();

        let deadline = Instant::now() + Duration::from_secs(1);
        while fetch_persisted_turn_usage(&db).is_none() {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for background persistence"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    });

    let row = fetch_persisted_turn_usage(&db).unwrap();
    assert_eq!(row.0.as_deref(), Some("session-final"));
    assert_eq!(row.1, 100);
    assert_eq!(row.2, 20);
    assert_eq!(row.3, 30);
    assert_eq!(row.4, 40);
}

fn sample_session() -> DiscordSession {
    DiscordSession {
        session_id: Some("session-1".to_string()),
        memento_context_loaded: true,
        memento_reflected: false,
        current_path: Some("/tmp/project".to_string()),
        history: vec![
            HistoryItem {
                item_type: HistoryType::User,
                content: "hello".to_string(),
            },
            HistoryItem {
                item_type: HistoryType::Assistant,
                content: "world".to_string(),
            },
        ],
        pending_uploads: Vec::new(),
        cleared: false,
        channel_name: Some("adk-cdx".to_string()),
        category_name: None,
        remote_profile_name: None,
        channel_id: Some(42),
        last_active: tokio::time::Instant::now(),
        worktree: None,
        born_generation: 0,
        assistant_turns: 0,
    }
}

#[test]
fn turn_end_memory_plan_skips_only_cleared_sessions() {
    let mut cleared = sample_session();
    cleared.cleared = true;
    assert_eq!(
        plan_turn_end_memory(&cleared, MemoryBackendKind::File, false, false, false, true),
        None
    );
}

#[test]
fn turn_end_memory_plan_records_final_turn_before_memento_reflect_request() {
    let settings = ResolvedMemorySettings {
        backend: MemoryBackendKind::Memento,
        ..ResolvedMemorySettings::default()
    };
    let mut session = sample_session();
    let memory_plan = plan_turn_end_memory(
        &session,
        MemoryBackendKind::Memento,
        false,
        false,
        true,
        true,
    )
    .expect("turn end memory plan should exist");

    assert_eq!(
        memory_plan.session_end_reason,
        Some(SessionEndReason::LocalSessionReset)
    );

    if memory_plan.persist_transcript {
        session.history.push(HistoryItem {
            item_type: HistoryType::User,
            content: "current user".to_string(),
        });
        session.history.push(HistoryItem {
            item_type: HistoryType::Assistant,
            content: "current assistant".to_string(),
        });
    }

    let request = take_memento_reflect_request(
        &mut session,
        &settings,
        &ProviderKind::Codex,
        None,
        42,
        memory_plan.session_end_reason.expect("session end reason"),
    )
    .expect("final turn should be included in reflect request");

    assert!(request.transcript.contains("[User]: current user"));
    assert!(
        request
            .transcript
            .contains("[Assistant]: current assistant")
    );
}

#[test]
fn turn_end_memory_plan_keeps_memento_feedback_analysis_when_prompt_is_too_long() {
    let prompt_too_long = sample_session();
    assert_eq!(
        plan_turn_end_memory(
            &prompt_too_long,
            MemoryBackendKind::Memento,
            true,
            false,
            false,
            false,
        ),
        Some(TurnEndMemoryPlan {
            session_end_reason: None,
            clear_provider_session: false,
            persist_transcript: false,
            analyze_recall_feedback: true,
            spawn_capture: false,
        })
    );
}

#[test]
fn turn_end_memory_plan_prompt_too_long_does_not_clear_provider_session() {
    let prompt_too_long = sample_session();
    assert_eq!(
        plan_turn_end_memory(
            &prompt_too_long,
            MemoryBackendKind::Memento,
            true,
            true,
            true,
            false,
        ),
        Some(TurnEndMemoryPlan {
            session_end_reason: None,
            clear_provider_session: false,
            persist_transcript: false,
            analyze_recall_feedback: true,
            spawn_capture: false,
        })
    );
}

#[test]
fn turn_end_memory_plan_uses_background_capture_for_non_memento_turns() {
    let session = sample_session();
    assert_eq!(
        plan_turn_end_memory(&session, MemoryBackendKind::File, false, false, false, true),
        Some(TurnEndMemoryPlan {
            session_end_reason: None,
            clear_provider_session: false,
            persist_transcript: true,
            analyze_recall_feedback: false,
            spawn_capture: true,
        })
    );
}

#[test]
fn turn_end_memory_plan_uses_reflect_for_memento_local_session_reset() {
    let session = sample_session();
    assert_eq!(
        plan_turn_end_memory(
            &session,
            MemoryBackendKind::Memento,
            false,
            false,
            true,
            true
        ),
        Some(TurnEndMemoryPlan {
            session_end_reason: Some(SessionEndReason::LocalSessionReset),
            clear_provider_session: true,
            persist_transcript: true,
            analyze_recall_feedback: true,
            spawn_capture: false,
        })
    );
}

#[test]
fn turn_end_memory_plan_clears_provider_session_on_resume_failure_without_capture() {
    let session = sample_session();
    assert_eq!(
        plan_turn_end_memory(&session, MemoryBackendKind::File, false, true, false, false),
        Some(TurnEndMemoryPlan {
            session_end_reason: None,
            clear_provider_session: true,
            persist_transcript: false,
            analyze_recall_feedback: false,
            spawn_capture: false,
        })
    );
}

#[test]
fn turn_end_memory_plan_keeps_memento_feedback_analysis_on_resume_failure() {
    let session = sample_session();
    assert_eq!(
        plan_turn_end_memory(
            &session,
            MemoryBackendKind::Memento,
            false,
            true,
            false,
            false
        ),
        Some(TurnEndMemoryPlan {
            session_end_reason: None,
            clear_provider_session: true,
            persist_transcript: false,
            analyze_recall_feedback: true,
            spawn_capture: false,
        })
    );
}

#[test]
fn turn_end_memory_plan_skips_background_capture_for_normal_memento_turns() {
    let session = sample_session();
    assert_eq!(
        plan_turn_end_memory(
            &session,
            MemoryBackendKind::Memento,
            false,
            false,
            false,
            true
        ),
        Some(TurnEndMemoryPlan {
            session_end_reason: None,
            clear_provider_session: false,
            persist_transcript: true,
            analyze_recall_feedback: true,
            spawn_capture: false,
        })
    );
}

#[test]
fn turn_end_memory_plan_clears_provider_session_at_turn_cap() {
    let mut session = sample_session();
    session.history = (0..PROVIDER_SESSION_ASSISTANT_TURN_CAP.saturating_sub(1))
        .flat_map(|idx| {
            [
                HistoryItem {
                    item_type: HistoryType::User,
                    content: format!("user-{idx}"),
                },
                HistoryItem {
                    item_type: HistoryType::Assistant,
                    content: format!("assistant-{idx}"),
                },
            ]
        })
        .collect();

    assert_eq!(
        plan_turn_end_memory(&session, MemoryBackendKind::File, false, false, false, true),
        Some(TurnEndMemoryPlan {
            session_end_reason: Some(SessionEndReason::TurnCapReached),
            clear_provider_session: true,
            persist_transcript: true,
            analyze_recall_feedback: false,
            spawn_capture: true,
        })
    );
}

#[test]
fn turn_end_memory_plan_keeps_recall_feedback_analysis_for_normal_memento_turns() {
    let session = sample_session();
    let plan = plan_turn_end_memory(
        &session,
        MemoryBackendKind::Memento,
        false,
        false,
        false,
        true,
    )
    .expect("memento turns should still produce a memory plan");

    assert!(plan.persist_transcript);
    assert!(plan.analyze_recall_feedback);
    assert!(
        !plan.spawn_capture,
        "memento turns should skip background capture while still analyzing recall feedback"
    );
}

#[test]
fn retry_context_history_keeps_last_ten_visible_messages() {
    let history = (0..12)
        .flat_map(|idx| {
            [
                HistoryItem {
                    item_type: HistoryType::User,
                    content: format!("user-{idx}"),
                },
                HistoryItem {
                    item_type: HistoryType::Assistant,
                    content: format!("assistant-{idx}"),
                },
            ]
        })
        .collect::<Vec<_>>();

    let context = build_session_retry_context_from_history(&history).expect("retry context");
    let lines = context.lines().collect::<Vec<_>>();

    assert_eq!(lines.len(), 10);
    assert_eq!(lines.first().copied(), Some("User: user-7"));
    assert_eq!(lines.last().copied(), Some("Assistant: assistant-11"));
}

#[test]
fn stored_retry_context_is_consumed_once() {
    let db = crate::db::test_db();
    store_session_retry_context(Some(&db), None, 42, "User: hi\nAssistant: hello")
        .expect("store retry context");

    assert_eq!(
        take_session_retry_context(Some(&db), 42),
        Some("User: hi\nAssistant: hello".to_string())
    );
    assert_eq!(take_session_retry_context(Some(&db), 42), None);
}

#[test]
fn storing_retry_context_enqueues_deduped_lifecycle_notification() {
    let db = crate::db::test_db();

    assert!(
        store_session_retry_context_with_notify(
            Some(&db),
            None,
            42,
            "User: hi\nAssistant: hello",
            Some("session-a"),
        )
        .expect("store retry context with notify")
    );
    assert!(
        !store_session_retry_context_with_notify(
            Some(&db),
            None,
            42,
            "User: hi\nAssistant: hello again",
            Some("session-a"),
        )
        .expect("dedupe retry context notify")
    );

    let conn = db.read_conn().unwrap();
    let (reason_code, session_key, content, count): (Option<String>, Option<String>, String, i64) =
        conn.query_row(
            "SELECT
                MAX(reason_code),
                MAX(session_key),
                MAX(content),
                COUNT(*)
             FROM message_outbox",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();

    assert_eq!(count, 1);
    assert_eq!(reason_code.as_deref(), Some("lifecycle.recovery_context"));
    assert_eq!(session_key.as_deref(), Some("session-a"));
    assert!(content.contains("복원 컨텍스트로 저장했습니다"));
    assert_eq!(
        take_session_retry_context(Some(&db), 42),
        Some("User: hi\nAssistant: hello again".to_string())
    );
}

#[test]
fn memento_reflect_request_requires_loaded_unreflected_session() {
    let settings = ResolvedMemorySettings {
        backend: MemoryBackendKind::Memento,
        ..ResolvedMemorySettings::default()
    };
    let mut session = sample_session();

    let request = take_memento_reflect_request(
        &mut session,
        &settings,
        &ProviderKind::Codex,
        None,
        42,
        SessionEndReason::IdleExpiry,
    )
    .expect("memento reflect should be prepared");

    assert_eq!(request.session_id, "session-1");
    assert_eq!(request.channel_id, 42);
    assert_eq!(request.reason, SessionEndReason::IdleExpiry);
    assert_eq!(request.transcript, "[User]: hello\n[Assistant]: world");
    assert!(session.memento_reflected);

    let duplicate = take_memento_reflect_request(
        &mut session,
        &settings,
        &ProviderKind::Codex,
        None,
        42,
        SessionEndReason::IdleExpiry,
    );
    assert!(duplicate.is_none(), "reflect must be one-shot per session");
}

#[test]
fn memento_reflect_request_handles_local_session_reset_once() {
    let settings = ResolvedMemorySettings {
        backend: MemoryBackendKind::Memento,
        ..ResolvedMemorySettings::default()
    };
    let mut session = sample_session();
    session.history.push(HistoryItem {
        item_type: HistoryType::User,
        content: "current user".to_string(),
    });
    session.history.push(HistoryItem {
        item_type: HistoryType::Assistant,
        content: "current assistant".to_string(),
    });

    let request = take_memento_reflect_request(
        &mut session,
        &settings,
        &ProviderKind::Codex,
        None,
        42,
        SessionEndReason::TurnCapReached,
    )
    .expect("turn cap should trigger one reflect");

    assert_eq!(request.reason, SessionEndReason::TurnCapReached);
    assert!(request.transcript.contains("[User]: current user"));
    assert!(
        request
            .transcript
            .contains("[Assistant]: current assistant")
    );
    assert!(session.memento_reflected);

    let duplicate = take_memento_reflect_request(
        &mut session,
        &settings,
        &ProviderKind::Codex,
        None,
        42,
        SessionEndReason::TurnCapReached,
    );
    assert!(
        duplicate.is_none(),
        "reflect must stay one-shot after turn-cap reset"
    );
}

#[test]
fn memento_reflect_request_skips_other_backends_or_missing_state() {
    let mut unloaded = sample_session();
    unloaded.memento_context_loaded = false;
    assert!(
        take_memento_reflect_request(
            &mut unloaded,
            &ResolvedMemorySettings {
                backend: MemoryBackendKind::Memento,
                ..ResolvedMemorySettings::default()
            },
            &ProviderKind::Codex,
            None,
            42,
            SessionEndReason::LocalSessionReset,
        )
        .is_none()
    );

    let mut non_memento = sample_session();
    assert!(
        take_memento_reflect_request(
            &mut non_memento,
            &ResolvedMemorySettings::default(),
            &ProviderKind::Codex,
            None,
            42,
            SessionEndReason::LocalSessionReset,
        )
        .is_none()
    );

    let mut missing_session_id = sample_session();
    missing_session_id.session_id = None;
    assert!(
        take_memento_reflect_request(
            &mut missing_session_id,
            &ResolvedMemorySettings {
                backend: MemoryBackendKind::Memento,
                ..ResolvedMemorySettings::default()
            },
            &ProviderKind::Codex,
            None,
            42,
            SessionEndReason::LocalSessionReset,
        )
        .is_none()
    );

    let mut missing_history = sample_session();
    missing_history.history.clear();
    assert!(
        take_memento_reflect_request(
            &mut missing_history,
            &ResolvedMemorySettings {
                backend: MemoryBackendKind::Memento,
                ..ResolvedMemorySettings::default()
            },
            &ProviderKind::Codex,
            None,
            42,
            SessionEndReason::LocalSessionReset,
        )
        .is_none()
    );
}

#[test]
fn clear_local_session_state_drops_stale_resume_id_everywhere() {
    let mut new_session_id = Some("stale-session".to_string());
    let mut inflight_state = InflightTurnState::new(
        ProviderKind::Claude,
        1479671298497183835,
        Some("adk-cc".to_string()),
        343742347365974026,
        1,
        2,
        "resume me".to_string(),
        Some("stale-session".to_string()),
        Some("AgentDesk-claude-adk-cc".to_string()),
        Some("/tmp/out.jsonl".to_string()),
        Some("/tmp/in.fifo".to_string()),
        0,
    );

    let mut new_raw_provider_session_id = Some("raw-stale-session".to_string());

    clear_local_session_state(
        &mut new_session_id,
        &mut new_raw_provider_session_id,
        &mut inflight_state,
    );

    assert_eq!(new_session_id, None);
    assert_eq!(new_raw_provider_session_id, None);
    assert_eq!(inflight_state.session_id, None);
}

#[test]
fn stale_resume_text_helper_matches_known_error_shapes() {
    assert!(contains_stale_resume_error_text("Error: No conversation"));
    assert!(contains_stale_resume_error_text(
        "No conversation found for session"
    ));
    assert!(!contains_stale_resume_error_text(
        "The assistant explained why a conversation was missing context."
    ));
}

#[test]
fn terminal_session_reset_helper_matches_terminal_recovery_failures() {
    assert!(stream_error_requires_terminal_session_reset(
        "Gemini session could not be recovered after retry: Gemini stream ended without a terminal result",
        "",
    ));
    assert!(stream_error_requires_terminal_session_reset(
        "InvalidArgument: Gemini resume selector must be `latest`, a numeric session index, or a UUID-like Gemini session reference",
        "",
    ));
    assert!(stream_error_requires_terminal_session_reset(
        "Qwen session could not be recovered after retry: Qwen stream ended without a terminal result",
        "",
    ));
    assert!(stream_error_requires_terminal_session_reset(
        "Qwen stream ended without a terminal result",
        "",
    ));
    assert!(!stream_error_requires_terminal_session_reset(
        "Gemini CLI not found",
        "",
    ));
}

#[test]
fn gemini_retry_reset_helper_requires_current_turn_partial_state() {
    assert!(should_reset_gemini_retry_attempt_state(
        "partial answer",
        None,
        false,
        false,
    ));
    assert!(should_reset_gemini_retry_attempt_state(
        "",
        Some("⚙ Bash: pwd"),
        true,
        false,
    ));
    assert!(!should_reset_gemini_retry_attempt_state(
        "", None, false, false,
    ));
}

#[test]
fn reset_gemini_retry_attempt_state_clears_partial_output_and_tool_flags() {
    let mut full_response = "partial answer".to_string();
    let mut current_tool_line = Some("⚙ Bash: pwd".to_string());
    let mut prev_tool_status = Some("✓ Read: src/config.rs".to_string());
    let mut last_tool_name = Some("Bash".to_string());
    let mut last_tool_summary = Some("pwd".to_string());
    let mut any_tool_used = true;
    let mut has_post_tool_text = true;
    let mut response_sent_offset = 42usize;
    let mut inflight_state = InflightTurnState::new(
        ProviderKind::Gemini,
        1479671298497183835,
        Some("adk-gm".to_string()),
        343742347365974026,
        1,
        2,
        "resume me".to_string(),
        Some("latest".to_string()),
        Some("AgentDesk-gemini-adk-gm".to_string()),
        Some("/tmp/out.jsonl".to_string()),
        Some("/tmp/in.fifo".to_string()),
        0,
    );
    inflight_state.full_response = full_response.clone();
    inflight_state.current_tool_line = current_tool_line.clone();
    inflight_state.prev_tool_status = prev_tool_status.clone();
    inflight_state.any_tool_used = true;
    inflight_state.has_post_tool_text = true;
    inflight_state.response_sent_offset = response_sent_offset;

    reset_gemini_retry_attempt_state(
        &mut full_response,
        &mut current_tool_line,
        &mut prev_tool_status,
        &mut last_tool_name,
        &mut last_tool_summary,
        &mut any_tool_used,
        &mut has_post_tool_text,
        &mut response_sent_offset,
        &mut inflight_state,
    );

    assert!(full_response.is_empty());
    assert_eq!(current_tool_line, None);
    assert_eq!(prev_tool_status, None);
    assert_eq!(last_tool_name, None);
    assert_eq!(last_tool_summary, None);
    assert!(!any_tool_used);
    assert!(!has_post_tool_text);
    assert_eq!(response_sent_offset, 0);
    assert!(inflight_state.full_response.is_empty());
    assert_eq!(inflight_state.current_tool_line, None);
    assert_eq!(inflight_state.prev_tool_status, None);
    assert!(!inflight_state.any_tool_used);
    assert!(!inflight_state.has_post_tool_text);
    assert_eq!(inflight_state.response_sent_offset, 0);
}

#[test]
fn clear_response_delivery_state_resets_offset_for_handoff_cleanup() {
    let mut full_response = "partial answer".to_string();
    let mut response_sent_offset = 42usize;
    let mut inflight_state = InflightTurnState::new(
        ProviderKind::Gemini,
        1479671298497183835,
        Some("adk-gm".to_string()),
        343742347365974026,
        1,
        2,
        "resume me".to_string(),
        Some("latest".to_string()),
        Some("AgentDesk-gemini-adk-gm".to_string()),
        Some("/tmp/out.jsonl".to_string()),
        Some("/tmp/in.fifo".to_string()),
        0,
    );
    inflight_state.full_response = full_response.clone();
    inflight_state.response_sent_offset = response_sent_offset;

    clear_response_delivery_state(
        &mut full_response,
        &mut response_sent_offset,
        &mut inflight_state,
    );

    assert!(full_response.is_empty());
    assert_eq!(response_sent_offset, 0);
    assert!(inflight_state.full_response.is_empty());
    assert_eq!(inflight_state.response_sent_offset, 0);
}

#[test]
fn sync_response_delivery_state_clamps_offset_after_api_friction_cleanup() {
    let full_response = "응답".to_string();
    let mut response_sent_offset = 5usize;
    let mut inflight_state = InflightTurnState::new(
        ProviderKind::Gemini,
        1479671298497183835,
        Some("adk-gm".to_string()),
        343742347365974026,
        1,
        2,
        "resume me".to_string(),
        Some("latest".to_string()),
        Some("AgentDesk-gemini-adk-gm".to_string()),
        Some("/tmp/out.jsonl".to_string()),
        Some("/tmp/in.fifo".to_string()),
        0,
    );
    inflight_state.response_sent_offset = 99;

    sync_response_delivery_state(
        &full_response,
        &mut response_sent_offset,
        &mut inflight_state,
    );

    assert_eq!(response_sent_offset, 3);
    assert_eq!(inflight_state.full_response, full_response);
    assert_eq!(inflight_state.response_sent_offset, 3);
}

#[test]
fn handle_gemini_retry_boundary_clears_partial_output_and_local_session_state() {
    let mut full_response = "partial answer".to_string();
    let mut current_tool_line = Some("⚙ Bash: pwd".to_string());
    let mut prev_tool_status = Some("✓ Read: src/config.rs".to_string());
    let mut last_tool_name = Some("Bash".to_string());
    let mut last_tool_summary = Some("pwd".to_string());
    let mut any_tool_used = true;
    let mut has_post_tool_text = true;
    let mut response_sent_offset = 42usize;
    let mut last_edit_text = "partial answer".to_string();
    let mut new_session_id = Some("stale".to_string());
    let mut new_raw_provider_session_id = Some("raw-stale".to_string());
    let mut inflight_state = InflightTurnState::new(
        ProviderKind::Gemini,
        1479671298497183835,
        Some("adk-gm".to_string()),
        343742347365974026,
        1,
        2,
        "resume me".to_string(),
        Some("stale".to_string()),
        Some("AgentDesk-gemini-adk-gm".to_string()),
        Some("/tmp/out.jsonl".to_string()),
        Some("/tmp/in.fifo".to_string()),
        0,
    );
    inflight_state.full_response = full_response.clone();
    inflight_state.current_tool_line = current_tool_line.clone();
    inflight_state.prev_tool_status = prev_tool_status.clone();
    inflight_state.any_tool_used = true;
    inflight_state.has_post_tool_text = true;
    inflight_state.response_sent_offset = response_sent_offset;

    let changed = handle_gemini_retry_boundary(
        &mut full_response,
        &mut current_tool_line,
        &mut prev_tool_status,
        &mut last_tool_name,
        &mut last_tool_summary,
        &mut any_tool_used,
        &mut has_post_tool_text,
        &mut response_sent_offset,
        &mut last_edit_text,
        &mut new_session_id,
        &mut new_raw_provider_session_id,
        &mut inflight_state,
    );

    assert!(changed);
    assert!(full_response.is_empty());
    assert_eq!(current_tool_line, None);
    assert_eq!(prev_tool_status, None);
    assert_eq!(last_tool_name, None);
    assert_eq!(last_tool_summary, None);
    assert!(!any_tool_used);
    assert!(!has_post_tool_text);
    assert_eq!(response_sent_offset, 0);
    assert!(last_edit_text.is_empty());
    assert_eq!(new_raw_provider_session_id, None);
    assert_eq!(new_session_id, None);
    assert_eq!(inflight_state.session_id, None);
    assert!(inflight_state.full_response.is_empty());
    assert_eq!(inflight_state.current_tool_line, None);
    assert_eq!(inflight_state.prev_tool_status, None);
    assert!(!inflight_state.any_tool_used);
    assert!(!inflight_state.has_post_tool_text);
    assert_eq!(inflight_state.response_sent_offset, 0);
}

#[test]
fn stale_resume_result_helper_requires_error_result_record() {
    let assistant_text = serde_json::json!({
        "type": "assistant",
        "message": {
            "content": [{
                "type": "text",
                "text": "The log said No conversation found"
            }]
        }
    });
    let success_result = serde_json::json!({
        "type": "result",
        "subtype": "success",
        "result": "No conversation found while inspecting logs",
    });
    let error_result = serde_json::json!({
        "type": "result",
        "subtype": "error_during_execution",
        "is_error": true,
        "errors": ["No conversation found"],
    });

    assert!(!result_event_has_stale_resume_error(&assistant_text));
    assert!(!result_event_has_stale_resume_error(&success_result));
    assert!(result_event_has_stale_resume_error(&error_result));
}

#[test]
fn stale_resume_output_scan_ignores_assistant_mentions() {
    let mut file = tempfile::NamedTempFile::new().unwrap();
    writeln!(
        file,
        "{}",
        serde_json::json!({
            "type": "assistant",
            "message": {
                "content": [{
                    "type": "text",
                    "text": "I saw `No conversation found` in the logs."
                }]
            }
        })
    )
    .unwrap();
    writeln!(
        file,
        "{}",
        serde_json::json!({
            "type": "result",
            "subtype": "success",
            "result": "analysis complete"
        })
    )
    .unwrap();
    file.flush().unwrap();

    assert!(!output_file_has_stale_resume_error_after_offset(
        file.path().to_str().unwrap(),
        0,
    ));
}

#[test]
fn stale_resume_output_scan_detects_error_result_after_offset() {
    let mut file = tempfile::NamedTempFile::new().unwrap();
    writeln!(
        file,
        "{}",
        serde_json::json!({
            "type": "assistant",
            "message": {
                "content": [{
                    "type": "text",
                    "text": "before"
                }]
            }
        })
    )
    .unwrap();
    file.flush().unwrap();
    let offset = std::fs::metadata(file.path()).unwrap().len();
    writeln!(
        file,
        "{}",
        serde_json::json!({
            "type": "result",
            "subtype": "error_during_execution",
            "is_error": true,
            "errors": ["No conversation found"]
        })
    )
    .unwrap();
    file.flush().unwrap();

    assert!(output_file_has_stale_resume_error_after_offset(
        file.path().to_str().unwrap(),
        offset,
    ));
}

#[test]
fn explicit_review_verdict_parser_accepts_structured_marker() {
    assert_eq!(
        extract_explicit_review_verdict("VERDICT: pass\nNo findings."),
        Some("pass")
    );
    assert_eq!(
        extract_explicit_review_verdict("overall: improve\nNeeds work."),
        Some("improve")
    );
}

#[test]
fn explicit_review_verdict_parser_ignores_unstructured_text() {
    assert_eq!(
        extract_explicit_review_verdict("검토 완료. 전반적으로 좋아 보입니다."),
        None
    );
}

#[test]
fn review_decision_parser_accepts_explicit_marker() {
    assert_eq!(
        extract_review_decision("DECISION: accept\n리뷰 반영하겠습니다."),
        Some("accept")
    );
    assert_eq!(
        extract_review_decision("결정: dismiss\n이 리뷰는 무시합니다."),
        Some("dismiss")
    );
    assert_eq!(
        extract_review_decision("Decision: dispute\n반론을 제기합니다."),
        Some("dispute")
    );
}

#[test]
fn review_decision_parser_accepts_keyword_in_tail() {
    assert_eq!(
        extract_review_decision("리뷰 내용을 검토한 결과 수정이 필요합니다.\n\naccept"),
        Some("accept")
    );
    assert_eq!(
        extract_review_decision("불필요한 변경이므로 dismiss 합니다."),
        Some("dismiss")
    );
    assert_eq!(
        extract_review_decision("기여자가 직접 머지 가능하게 처리하겠습니다."),
        None
    );
}

#[test]
fn review_decision_parser_rejects_korean_dismiss_synonyms_without_explicit_dismiss() {
    assert_eq!(
        extract_review_decision("결정: 리뷰 우회\n직접 머지로 진행합니다."),
        None
    );
    assert_eq!(
        extract_review_decision("결정: 기여자가 직접 머지\n리뷰는 여기서 닫겠습니다."),
        None
    );
    assert_eq!(extract_review_decision("결정: 리뷰 스킵"), None);
    assert_eq!(extract_review_decision("결정: direct merge"), None);
}

#[test]
fn review_decision_parser_rejects_ambiguous_keywords() {
    // Multiple different keywords -> ambiguous, return None
    assert_eq!(
        extract_review_decision("accept or dismiss 중 선택해야 합니다."),
        None
    );
}

#[test]
fn review_decision_parser_ignores_unstructured_text() {
    assert_eq!(
        extract_review_decision("리뷰 피드백을 확인했습니다. 코드를 수정하겠습니다."),
        None
    );
    assert_eq!(
        extract_review_decision("리뷰 우회 인식이 왜 안먹는지 디버깅 중입니다."),
        None
    );
}

#[test]
fn review_decision_parser_rejects_negative_dismiss_phrases() {
    assert_eq!(extract_review_decision("결정: 직접 머지하지 마"), None);
    assert_eq!(
        extract_review_decision("결정: 리뷰 우회하면 안 됩니다"),
        None
    );
    assert_eq!(
        extract_review_decision("기여자가 직접 머지하면 안 됩니다."),
        None
    );
}

#[test]
fn review_decision_explicit_marker_takes_priority() {
    // Even with keywords in tail, explicit marker should be found first
    assert_eq!(
        extract_review_decision("DECISION: accept\n이 dismiss는 무시해도 됩니다."),
        Some("accept")
    );
}

#[test]
fn review_decision_parser_handles_korean_text_over_500_bytes() {
    // Korean chars are 3 bytes each in UTF-8; build a response > 500 bytes
    // to exercise the safe_suffix path without panicking
    let padding = "가".repeat(200); // 600 bytes of Korean text
    let response = format!("{padding}\ndismiss");
    assert_eq!(extract_review_decision(&response), Some("dismiss"));
}

#[test]
fn verdict_fallback_payload_includes_provider() {
    let payload = build_verdict_payload("d-123", "pass", "LGTM", "codex");
    assert_eq!(payload["dispatch_id"], "d-123");
    assert_eq!(payload["overall"], "pass");
    assert_eq!(payload["feedback"], "LGTM");
    assert_eq!(payload["provider"], "codex");
}

#[test]
fn verdict_fallback_payload_truncates_long_feedback() {
    let long_response = "x".repeat(5000);
    let payload = build_verdict_payload("d-456", "improve", &long_response, "claude");
    assert_eq!(payload["provider"], "claude");
    let feedback = payload["feedback"].as_str().unwrap();
    assert!(feedback.len() <= 4003); // 4000 + "..." ellipsis
}

#[test]
fn work_outcome_parser_accepts_explicit_noop_marker() {
    assert_eq!(
        extract_explicit_work_outcome("OUTCOME: noop\n변경 불필요 — 이미 반영됨"),
        Some("noop")
    );
}

#[test]
fn work_outcome_parser_rejects_non_explicit_noop_mentions() {
    assert_eq!(
        extract_explicit_work_outcome(
            "이번 턴은 noop에 가까워 보이지만 먼저 코드 확인이 필요합니다."
        ),
        None
    );
}

// ========== resolve_done_response tests ==========

#[test]
fn done_replaces_stale_pre_tool_text_with_result() {
    // Text -> ToolUse -> Done(result): intermediate text should be replaced
    let res = resolve_done_response("이슈를 생성합니다.\n\n", "이슈 #90 생성 완료", true, false);
    assert_eq!(res, Some("이슈 #90 생성 완료".to_string()));
}

#[test]
fn done_keeps_full_response_when_post_tool_text_exists() {
    // Text -> ToolUse -> Text -> Done(result): streaming captured everything
    let res = resolve_done_response(
        "진행 중...\n\n이슈 #90 생성 완료",
        "이슈 #90 생성 완료",
        true,
        true,
    );
    assert_eq!(res, None); // keep full_response as-is
}

#[test]
fn done_uses_result_when_full_response_empty() {
    let res = resolve_done_response("", "최종 응답", false, false);
    assert_eq!(res, Some("최종 응답".to_string()));
}

#[test]
fn done_uses_result_when_full_response_whitespace_only() {
    let res = resolve_done_response("  \n\n  ", "최종 응답", true, false);
    assert_eq!(res, Some("최종 응답".to_string()));
}

#[test]
fn done_keeps_full_response_when_no_tools_used() {
    // Pure text turn without tools — streaming text IS the final response
    let res = resolve_done_response(
        "여기 분석 결과입니다...",
        "여기 분석 결과입니다...",
        false,
        false,
    );
    assert_eq!(res, None);
}

#[test]
fn done_noop_when_result_empty() {
    // Synthetic Done with empty result — nothing to replace with
    let res = resolve_done_response("중간 텍스트\n\n", "", true, false);
    assert_eq!(res, None);
}
