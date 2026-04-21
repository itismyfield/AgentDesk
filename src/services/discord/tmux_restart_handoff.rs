use std::sync::Arc;

use poise::serenity_prelude as serenity;
use serenity::ChannelId;

use crate::services::provider::{ProviderKind, parse_provider_and_channel_from_tmux_name};

use super::SharedData;

fn seed_restart_handoff_session_metadata(
    sessions: &mut std::collections::HashMap<ChannelId, super::DiscordSession>,
    channel_id: ChannelId,
    state: &super::inflight::InflightTurnState,
) -> bool {
    let Some(channel_name) = state
        .channel_name
        .as_ref()
        .map(|name| name.trim())
        .filter(|name| !name.is_empty())
        .map(str::to_string)
    else {
        return false;
    };

    let session = sessions
        .entry(channel_id)
        .or_insert_with(|| super::DiscordSession {
            session_id: None,
            memento_context_loaded: false,
            memento_reflected: false,
            current_path: None,
            history: Vec::new(),
            pending_uploads: Vec::new(),
            cleared: false,
            remote_profile_name: None,
            channel_id: Some(channel_id.get()),
            channel_name: None,
            category_name: None,
            last_active: tokio::time::Instant::now(),
            worktree: None,
            born_generation: super::runtime_store::load_generation(),
            assistant_turns: 0,
        });

    let mut changed = false;
    if session
        .channel_name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .is_none()
    {
        session.channel_name = Some(channel_name);
        changed = true;
    }
    if session.channel_id.is_none() {
        session.channel_id = Some(channel_id.get());
        changed = true;
    }
    session.last_active = tokio::time::Instant::now();
    changed
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

fn resolve_dispatched_thread_dispatch(
    db: &crate::db::Db,
    thread_channel_id: u64,
) -> Option<String> {
    let thread_channel_id = thread_channel_id.to_string();
    let conn = db.read_conn().ok()?;

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
    pg_pool: Option<&sqlx::PgPool>,
    thread_channel_id: u64,
) -> Option<String> {
    if let Some(pg_pool) = pg_pool {
        let thread_channel_id = thread_channel_id.to_string();
        return crate::utils::async_bridge::block_on_pg_result(
            pg_pool,
            move |pool| async move {
                if let Some(dispatch_id) = sqlx::query_scalar::<_, String>(
                    "SELECT id FROM task_dispatches
                     WHERE status = 'dispatched' AND thread_id = $1
                     ORDER BY created_at DESC, id DESC
                     LIMIT 1",
                )
                .bind(&thread_channel_id)
                .fetch_optional(&pool)
                .await
                .map_err(|error| format!("load pg dispatched thread dispatch: {error}"))?
                {
                    return Ok(Some(dispatch_id));
                }

                sqlx::query_scalar::<_, String>(
                    "SELECT active_dispatch_id FROM sessions
                     WHERE thread_channel_id = $1
                       AND status = 'working'
                       AND active_dispatch_id IS NOT NULL
                     ORDER BY COALESCE(last_heartbeat, created_at) DESC, id DESC
                     LIMIT 1",
                )
                .bind(&thread_channel_id)
                .fetch_optional(&pool)
                .await
                .map_err(|error| format!("load pg session dispatch fallback: {error}"))
            },
            |message| message,
        )
        .ok()
        .flatten();
    }

    resolve_dispatched_thread_dispatch(db?, thread_channel_id)
}

fn build_restart_handoff_session_key(
    state: &super::inflight::InflightTurnState,
    token_hash: &str,
    provider_kind: &ProviderKind,
) -> Option<String> {
    state
        .session_key
        .as_ref()
        .filter(|key| !key.trim().is_empty())
        .cloned()
        .or_else(|| {
            state
                .tmux_session_name
                .as_deref()
                .filter(|name| !name.trim().is_empty())
                .map(|tmux_name| {
                    super::adk_session::build_namespaced_session_key(
                        token_hash,
                        provider_kind,
                        tmux_name,
                    )
                })
        })
        .or_else(|| {
            state
                .channel_name
                .as_deref()
                .filter(|name| !name.trim().is_empty())
                .map(|channel_name| {
                    let tmux_name = provider_kind.build_tmux_session_name(channel_name);
                    super::adk_session::build_namespaced_session_key(
                        token_hash,
                        provider_kind,
                        &tmux_name,
                    )
                })
        })
}

async fn clear_restart_handoff_provider_session(
    channel_id: ChannelId,
    shared: &Arc<SharedData>,
    provider_kind: &ProviderKind,
    state: &super::inflight::InflightTurnState,
) {
    let session_key =
        match build_restart_handoff_session_key(state, &shared.token_hash, provider_kind) {
            Some(key) => Some(key),
            None => {
                super::adk_session::build_adk_session_key(shared, channel_id, provider_kind).await
            }
        };
    {
        let mut data = shared.core.lock().await;
        if let Some(session) = data.sessions.get_mut(&channel_id) {
            session.clear_provider_session();
        }
    }

    if let Some(key) = session_key {
        super::adk_session::clear_provider_session_id(&key, shared.api_port).await;
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] ↻ watcher death recovery: cleared provider session before restart handoff for channel {} ({})",
            channel_id.get(),
            key
        );
    } else {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] ↻ watcher death recovery: cleared in-memory provider session before restart handoff for channel {}",
            channel_id.get()
        );
    }
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

    clear_restart_handoff_provider_session(channel_id, shared, provider_kind, &state).await;

    let seeded_channel_name = {
        let mut data = shared.core.lock().await;
        seed_restart_handoff_session_metadata(&mut data.sessions, channel_id, &state)
    };
    if seeded_channel_name {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] ↻ watcher death recovery: seeded session metadata after interrupted restart cleanup for channel {}",
            channel_id.get()
        );
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::warn!(
        "  [{ts}] ⚠ watcher death recovery: suppressed auto post-restart handoff for channel {}",
        channel_id.get()
    );

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
        RestartHandoffScope, build_restart_handoff_session_key, resolve_restart_handoff_scope,
        seed_restart_handoff_session_metadata,
    };
    use crate::config::Config;
    use crate::services::discord::DiscordSession;
    use crate::services::discord::inflight::InflightTurnState;
    use crate::services::provider::ProviderKind;
    use poise::serenity_prelude::ChannelId;

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

    fn fresh_restart_handoff_db() -> (tempfile::TempDir, crate::db::Db) {
        let temp = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.data.dir = temp.path().to_path_buf();
        config.data.db_name = "restart-handoff.sqlite".to_string();
        let db = crate::db::init(&config).unwrap();
        (temp, db)
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
    fn restart_handoff_session_key_prefers_persisted_inflight_key() {
        let mut state = sample_inflight_state();
        state.session_key = Some("claude/token-hash/host:AgentDesk-claude-adk-cc".to_string());

        let resolved =
            build_restart_handoff_session_key(&state, "other-token-hash", &ProviderKind::Claude);

        assert_eq!(
            resolved.as_deref(),
            Some("claude/token-hash/host:AgentDesk-claude-adk-cc")
        );
    }

    #[test]
    fn restart_handoff_session_key_falls_back_to_tmux_name() {
        let mut state = sample_inflight_state();
        state.session_key = None;
        let hostname = crate::services::platform::hostname_short();
        let expected = format!("claude/token-hash/{hostname}:AgentDesk-claude-adk-cc");

        let resolved =
            build_restart_handoff_session_key(&state, "token-hash", &ProviderKind::Claude);

        assert_eq!(resolved.as_deref(), Some(expected.as_str()));
    }

    #[test]
    fn restart_handoff_seeds_channel_name_into_missing_session() {
        let state = sample_inflight_state();
        let mut sessions = std::collections::HashMap::new();

        let changed = seed_restart_handoff_session_metadata(
            &mut sessions,
            ChannelId::new(state.channel_id),
            &state,
        );

        assert!(changed);
        let seeded = sessions.get(&ChannelId::new(state.channel_id)).unwrap();
        assert_eq!(seeded.channel_name.as_deref(), Some("adk-cc"));
        assert_eq!(seeded.channel_id, Some(state.channel_id));
    }

    #[test]
    fn restart_handoff_preserves_existing_session_channel_name() {
        let state = sample_inflight_state();
        let channel_id = ChannelId::new(state.channel_id);
        let mut sessions = std::collections::HashMap::new();
        sessions.insert(
            channel_id,
            DiscordSession {
                session_id: None,
                memento_context_loaded: false,
                memento_reflected: false,
                current_path: None,
                history: Vec::new(),
                pending_uploads: Vec::new(),
                cleared: false,
                remote_profile_name: None,
                channel_id: Some(state.channel_id),
                channel_name: Some("already-set".to_string()),
                category_name: None,
                last_active: tokio::time::Instant::now(),
                worktree: None,
                born_generation: 0,
                assistant_turns: 0,
            },
        );

        let changed = seed_restart_handoff_session_metadata(&mut sessions, channel_id, &state);

        assert!(!changed);
        let seeded = sessions.get(&channel_id).unwrap();
        assert_eq!(seeded.channel_name.as_deref(), Some("already-set"));
    }

    #[test]
    fn watcher_dispatch_db_fallback_prefers_dispatched_thread_row() {
        let (_temp, db) = fresh_restart_handoff_db();
        let conn = db.lock().unwrap();
        conn.execute_batch(
            "
            DROP TABLE IF EXISTS task_dispatches;
            DROP TABLE IF EXISTS sessions;
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
        drop(conn);

        let resolved = super::resolve_dispatched_thread_dispatch_from_db(
            Some(&db),
            None,
            1_492_091_375_422_930_966,
        );
        assert_eq!(resolved.as_deref(), Some("latest-dispatch"));
    }

    #[test]
    fn watcher_dispatch_db_fallback_uses_session_when_thread_row_missing() {
        let (_temp, db) = fresh_restart_handoff_db();
        let conn = db.lock().unwrap();
        conn.execute_batch(
            "
            DROP TABLE IF EXISTS task_dispatches;
            DROP TABLE IF EXISTS sessions;
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
        drop(conn);

        let resolved = super::resolve_dispatched_thread_dispatch_from_db(
            Some(&db),
            None,
            1_492_091_380_045_189_131,
        );
        assert_eq!(resolved.as_deref(), Some("session-dispatch"));
    }
}
