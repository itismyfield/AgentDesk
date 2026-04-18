use std::sync::Arc;

use poise::serenity_prelude as serenity;
use serenity::ChannelId;

use crate::services::provider::{ProviderKind, parse_provider_and_channel_from_tmux_name};
use crate::utils::format::tail_with_ellipsis;

use super::SharedData;

fn build_restart_handoff_context(
    state: &super::inflight::InflightTurnState,
    best_response: &str,
) -> String {
    let partial = best_response.trim();
    let partial_context = if partial.is_empty() {
        "(재시작 전까지 전달된 partial 응답 없음)".to_string()
    } else {
        tail_with_ellipsis(partial, 1200)
    };
    format!(
        "재시작 중 기존 tmux 세션이 종료되어 동일 turn에 재연결하지 못했습니다.\n\n원래 사용자 요청:\n{}\n\n재시작 전 partial 응답:\n{}",
        state.user_text.trim(),
        partial_context,
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RestartHandoffScope {
    ExactMetadata,
    ProviderChannelScopedFallback,
}

pub(super) fn resolve_restart_handoff_scope(
    state: &super::inflight::InflightTurnState,
    tmux_session_name: &str,
    output_path: &str,
) -> RestartHandoffScope {
    let tmux_matches = state.tmux_session_name.as_deref() == Some(tmux_session_name);
    let output_matches = state.output_path.as_deref() == Some(output_path);
    if tmux_matches || output_matches {
        RestartHandoffScope::ExactMetadata
    } else {
        RestartHandoffScope::ProviderChannelScopedFallback
    }
}

pub(super) fn resolve_dispatched_thread_dispatch_from_conn(
    conn: &libsql_rusqlite::Connection,
    thread_channel_id: u64,
) -> Option<String> {
    let thread_channel_id = thread_channel_id.to_string();

    conn.query_row(
        "SELECT id FROM task_dispatches
         WHERE status = 'dispatched' AND thread_id = ?1
         ORDER BY datetime(created_at) DESC, rowid DESC
         LIMIT 1",
        [thread_channel_id.as_str()],
        |row| row.get::<_, String>(0),
    )
    .ok()
    .or_else(|| {
        conn.query_row(
            "SELECT active_dispatch_id FROM sessions
             WHERE thread_channel_id = ?1
               AND status = 'working'
               AND active_dispatch_id IS NOT NULL
             ORDER BY datetime(COALESCE(last_heartbeat, created_at)) DESC, id DESC
             LIMIT 1",
            [thread_channel_id.as_str()],
            |row| row.get::<_, String>(0),
        )
        .ok()
    })
}

pub(super) fn resolve_dispatched_thread_dispatch_from_db(
    db: Option<&crate::db::Db>,
    thread_channel_id: u64,
) -> Option<String> {
    let db = db?;
    let conn = db.separate_conn().ok()?;
    resolve_dispatched_thread_dispatch_from_conn(&conn, thread_channel_id)
}

pub(super) async fn start_restart_handoff_from_state(
    channel_id: ChannelId,
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider_kind: &ProviderKind,
    state: super::inflight::InflightTurnState,
    best_response: &str,
) -> bool {
    let stale_text = super::turn_bridge::stale_inflight_message(best_response);
    let _ = super::formatting::replace_long_message_raw(
        http,
        channel_id,
        serenity::MessageId::new(state.current_msg_id),
        &stale_text,
        shared,
    )
    .await;

    let context = build_restart_handoff_context(&state, best_response);
    let handoff_prompt = format!(
        "dcserver가 재시작되었습니다. 재시작 전 작업의 후속 조치를 이어서 진행해주세요.\n\n## 재시작 전 컨텍스트\n{}\n\n## 요청 사항\n재시작 중 중단된 응답을 이어서 마무리",
        context
    );
    let placeholder_id = match channel_id
        .send_message(
            http,
            serenity::CreateMessage::new()
                .content("📎 **Post-restart handoff** — 재시작 후속 작업을 자동으로 이어받습니다."),
        )
        .await
    {
        Ok(msg) => msg.id,
        Err(e) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ⚠ failed to send watcher-handoff placeholder for channel {}: {}",
                channel_id.get(),
                e
            );
            serenity::MessageId::new(state.current_msg_id)
        }
    };

    let author_id = serenity::UserId::new(1);
    let mut started_immediately = false;
    if let (Some(ctx), Some(token)) = (
        shared.cached_serenity_ctx.get(),
        shared.cached_bot_token.get(),
    ) {
        match super::router::handle_text_message(
            ctx,
            channel_id,
            placeholder_id,
            author_id,
            "system",
            &handoff_prompt,
            shared,
            token,
            true,
            false,
            false,
            false,
            None,
            false,
            None,
        )
        .await
        {
            Ok(()) => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] ↻ watcher death recovery: started immediate handoff turn for channel {}",
                    channel_id.get()
                );
                started_immediately = true;
            }
            Err(e) => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] ⚠ watcher death recovery: immediate handoff start failed for channel {}: {}",
                    channel_id.get(),
                    e
                );
            }
        }
    }

    if !started_immediately {
        super::mailbox_enqueue_intervention(
            shared,
            provider_kind,
            channel_id,
            super::Intervention {
                author_id,
                message_id: placeholder_id,
                source_message_ids: vec![placeholder_id],
                text: handoff_prompt,
                mode: super::InterventionMode::Soft,
                created_at: std::time::Instant::now(),
                reply_context: None,
                has_reply_boundary: false,
                merge_consecutive: false,
            },
        )
        .await;
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] ↻ watcher death recovery: queued fallback handoff for channel {}",
            channel_id.get()
        );
    }

    super::inflight::clear_inflight_state(provider_kind, channel_id.get());
    true
}

pub(super) async fn resume_aborted_restart_turn(
    channel_id: ChannelId,
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    tmux_session_name: &str,
    output_path: &str,
) -> bool {
    let Some((provider_kind, _)) = parse_provider_and_channel_from_tmux_name(tmux_session_name)
    else {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] ⚠ watcher death recovery: failed to parse provider/channel from tmux session {}",
            tmux_session_name
        );
        return false;
    };
    let Some(state) = super::inflight::load_inflight_state(&provider_kind, channel_id.get()) else {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] ⚠ watcher death recovery: no inflight state for channel {} (provider {})",
            channel_id.get(),
            provider_kind.as_str()
        );
        return false;
    };

    let scope = resolve_restart_handoff_scope(&state, tmux_session_name, output_path);
    if matches!(scope, RestartHandoffScope::ProviderChannelScopedFallback) {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] ↻ watcher death recovery: inflight metadata mismatch for channel {} (state tmux: {:?}, watcher tmux: {}, state output: {:?}, watcher output: {}) — proceeding with provider/channel scoped handoff",
            channel_id.get(),
            state.tmux_session_name.as_deref(),
            tmux_session_name,
            state.output_path.as_deref(),
            output_path
        );
    }

    let extracted_full = super::recovery::extract_response_from_output_pub(output_path, 0);
    let best_response = if matches!(scope, RestartHandoffScope::ProviderChannelScopedFallback) {
        state.full_response.clone()
    } else if !extracted_full.trim().is_empty() {
        extracted_full
    } else {
        state.full_response.clone()
    };
    start_restart_handoff_from_state(
        channel_id,
        http,
        shared,
        &provider_kind,
        state,
        &best_response,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::{
        RestartHandoffScope, resolve_dispatched_thread_dispatch_from_conn,
        resolve_restart_handoff_scope,
    };
    use crate::services::discord::inflight::InflightTurnState;
    use crate::services::provider::ProviderKind;

    fn sample_inflight_state() -> InflightTurnState {
        InflightTurnState::new(
            ProviderKind::Claude,
            1479671298497183835,
            Some("adk-cc".to_string()),
            1,
            10,
            11,
            "restart me".to_string(),
            Some("session-123".to_string()),
            Some("AgentDesk-claude-adk-cc".to_string()),
            Some("/tmp/adk-cc.jsonl".to_string()),
            None,
            0,
        )
    }

    #[test]
    fn restart_handoff_prefers_exact_metadata_match() {
        let state = sample_inflight_state();
        let scope = resolve_restart_handoff_scope(
            &state,
            "AgentDesk-claude-adk-cc",
            "/tmp/other-output.jsonl",
        );
        assert_eq!(scope, RestartHandoffScope::ExactMetadata);
    }

    #[test]
    fn restart_handoff_allows_provider_channel_fallback_on_metadata_drift() {
        let state = sample_inflight_state();
        let scope = resolve_restart_handoff_scope(
            &state,
            "AgentDesk-claude-adk-cc-restarted",
            "/tmp/new-output.jsonl",
        );
        assert_eq!(scope, RestartHandoffScope::ProviderChannelScopedFallback);
    }

    #[test]
    fn watcher_dispatch_db_fallback_prefers_dispatched_thread_row() {
        let conn = libsql_rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "
            CREATE TABLE task_dispatches (
                id TEXT PRIMARY KEY,
                status TEXT,
                thread_id TEXT,
                created_at TEXT
            );
            CREATE TABLE sessions (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                status TEXT,
                active_dispatch_id TEXT,
                created_at TEXT,
                last_heartbeat TEXT,
                thread_channel_id TEXT
            );
            INSERT INTO task_dispatches (id, status, thread_id, created_at)
            VALUES
                ('older-dispatch', 'dispatched', '1492091375422930966', '2026-04-11 00:15:42'),
                ('latest-dispatch', 'dispatched', '1492091375422930966', '2026-04-11 00:15:43');
            INSERT INTO sessions (status, active_dispatch_id, created_at, last_heartbeat, thread_channel_id)
            VALUES ('working', 'session-dispatch', '2026-04-11 00:15:40', '2026-04-11 00:24:21', '1492091375422930966');
            ",
        )
        .unwrap();

        let resolved =
            resolve_dispatched_thread_dispatch_from_conn(&conn, 1_492_091_375_422_930_966);
        assert_eq!(resolved.as_deref(), Some("latest-dispatch"));
    }

    #[test]
    fn watcher_dispatch_db_fallback_uses_session_when_thread_row_missing() {
        let conn = libsql_rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "
            CREATE TABLE task_dispatches (
                id TEXT PRIMARY KEY,
                status TEXT,
                thread_id TEXT,
                created_at TEXT
            );
            CREATE TABLE sessions (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                status TEXT,
                active_dispatch_id TEXT,
                created_at TEXT,
                last_heartbeat TEXT,
                thread_channel_id TEXT
            );
            INSERT INTO sessions (status, active_dispatch_id, created_at, last_heartbeat, thread_channel_id)
            VALUES ('working', 'session-dispatch', '2026-04-11 00:15:40', '2026-04-11 00:24:21', '1492091380045189131');
            ",
        )
        .unwrap();

        let resolved =
            resolve_dispatched_thread_dispatch_from_conn(&conn, 1_492_091_380_045_189_131);
        assert_eq!(resolved.as_deref(), Some("session-dispatch"));
    }
}
