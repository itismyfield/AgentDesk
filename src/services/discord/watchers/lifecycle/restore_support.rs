use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RestoreDispatchRebindOutcome {
    NotRebound,
    Rebound,
}

/// Rebind only an inflight dispatch that is still active. The status and blank
/// dispatch-link predicates are the CAS guard: a concurrent new turn must not
/// have its session link overwritten by an older restore pass.
pub(super) async fn rebind_restored_dispatch_if_missing(
    pg_pool: Option<&sqlx::PgPool>,
    state: &super::super::inflight::InflightTurnState,
) -> RestoreDispatchRebindOutcome {
    let (Some(pool), Some(session_key), Some(dispatch_id), Some(turn_nonce)) = (
        pg_pool,
        state.session_key.as_deref(),
        state
            .dispatch_id
            .as_deref()
            .map(str::trim)
            .filter(|dispatch_id| !dispatch_id.is_empty()),
        state
            .turn_nonce
            .as_deref()
            .filter(|nonce| !nonce.is_empty()),
    ) else {
        return RestoreDispatchRebindOutcome::NotRebound;
    };
    let channel_id = state.channel_id.to_string();

    match sqlx::query(
        "UPDATE sessions s
            SET active_dispatch_id = $3,
                session_info = 'Rebound restored dispatch link',
                last_heartbeat = NOW()
          WHERE s.session_key = $1
            AND s.channel_id = $2
            AND s.status = 'turn_active'
            AND s.active_turn_nonce = $4
            AND COALESCE(BTRIM(s.active_dispatch_id), '') = ''
            AND EXISTS (
                SELECT 1 FROM task_dispatches d
                 WHERE d.id = $3 AND d.status IN ('pending', 'dispatched')
            )",
    )
    .bind(session_key)
    .bind(&channel_id)
    .bind(dispatch_id)
    .bind(turn_nonce)
    .execute(pool)
    .await
    {
        Ok(result) if result.rows_affected() == 1 => RestoreDispatchRebindOutcome::Rebound,
        Ok(_) => RestoreDispatchRebindOutcome::NotRebound,
        Err(error) => {
            tracing::warn!(
                channel_id = state.channel_id,
                dispatch_id,
                error = %error,
                "failed to CAS-rebind restored dispatch"
            );
            RestoreDispatchRebindOutcome::NotRebound
        }
    }
}

pub(super) fn extract_result_error_text(value: &serde_json::Value) -> String {
    let errors = value
        .get("errors")
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::trim))
                .filter(|item| !item.is_empty())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();

    if !errors.trim().is_empty() {
        errors
    } else {
        value
            .get("result")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string()
    }
}

/// Resolve a restored session's persisted cwd (worktree) from the `sessions`
/// table, scoped to the unique Discord `channel_id`.
///
/// #3207 (part 2) P0-b: `session_key` derives from the sanitized/truncated
/// channel NAME, so name-colliding channels would resolve EACH OTHER's
/// persisted cwd straight into the restored runtime state. The
/// `channel_id = $2` predicate is the cross-channel guard; legacy NULL
/// `channel_id` rows are intentionally NOT reused (that is exactly the hazard
/// being closed — reuse self-heals on the next turn once the row is stamped).
pub(super) fn load_restored_session_cwd(
    pg_pool: Option<&sqlx::PgPool>,
    session_keys: &[String],
    channel_id: u64,
) -> Option<String> {
    if let Some(pg_pool) = pg_pool {
        let session_keys = session_keys.to_vec();
        let channel_id = channel_id.to_string();
        return crate::utils::async_bridge::block_on_pg_result(
            pg_pool,
            move |pool| async move {
                for session_key in session_keys {
                    let path = sqlx::query_scalar::<_, String>(
                        "SELECT cwd FROM sessions \
                         WHERE session_key = $1 AND channel_id = $2 LIMIT 1",
                    )
                    .bind(&session_key)
                    .bind(&channel_id)
                    .fetch_optional(&pool)
                    .await
                    .map_err(|error| format!("load tmux restore cwd {session_key}: {error}"))?;
                    if let Some(path) =
                        path.filter(|path| !path.is_empty() && std::path::Path::new(path).is_dir())
                    {
                        return Ok(Some(path));
                    }
                }
                Ok(None)
            },
            |message| message,
        )
        .ok()
        .flatten();
    }

    let _ = (session_keys, channel_id);
    None
}

pub(super) fn push_transcript_event(
    events: &mut Vec<SessionTranscriptEvent>,
    event: SessionTranscriptEvent,
) {
    let has_payload = !event.content.trim().is_empty()
        || event
            .summary
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty())
        || event
            .tool_name
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty());
    if has_payload
        || matches!(
            event.kind,
            SessionTranscriptEventKind::Thinking
                | SessionTranscriptEventKind::Result
                | SessionTranscriptEventKind::Error
                | SessionTranscriptEventKind::Task
                | SessionTranscriptEventKind::System
        )
    {
        events.push(event);
    }
}

pub(super) const REDACTED_THINKING_STATUS_LINE: &str = "💭 Thinking...";

pub(super) fn redacted_thinking_transcript_event() -> SessionTranscriptEvent {
    SessionTranscriptEvent {
        kind: SessionTranscriptEventKind::Thinking,
        tool_name: None,
        summary: None,
        content: String::new(),
        status: Some("info".to_string()),
        is_error: false,
    }
}

pub(super) fn inflight_duration_ms(started_at: Option<&str>) -> Option<i64> {
    let started_at = started_at?.trim();
    if started_at.is_empty() {
        return None;
    }
    let parsed = chrono::NaiveDateTime::parse_from_str(started_at, "%Y-%m-%d %H:%M:%S").ok()?;
    let elapsed = chrono::Local::now().naive_local() - parsed;
    Some(elapsed.num_milliseconds().max(0))
}

pub(super) fn load_restored_provider_session_id(
    pg_pool: Option<&sqlx::PgPool>,
    token_hash: &str,
    provider: &ProviderKind,
    channel_name: &str,
) -> Option<String> {
    let tmux_name = provider.build_tmux_session_name(channel_name);
    let session_keys =
        super::super::adk_session::build_session_key_candidates(token_hash, provider, &tmux_name);

    if let Some(pg_pool) = pg_pool {
        let session_keys = session_keys.clone();
        let provider_name = provider.as_str().to_string();
        return crate::utils::async_bridge::block_on_pg_result(
            pg_pool,
            move |pool| async move {
                for session_key in session_keys {
                    let session_id = sqlx::query_scalar::<_, Option<String>>(
                        "SELECT claude_session_id
                         FROM sessions
                         WHERE session_key = $1 AND provider = $2
                         LIMIT 1",
                    )
                    .bind(&session_key)
                    .bind(&provider_name)
                    .fetch_optional(&pool)
                    .await
                    .map_err(|error| format!("load tmux provider session {session_key}: {error}"))?
                    .flatten();
                    if let Some(session_id) = session_id.filter(|session_id| !session_id.is_empty())
                    {
                        return Ok(Some(session_id));
                    }
                }
                Ok(None)
            },
            |message| message,
        )
        .ok()
        .flatten();
    }

    let _ = session_keys;
    None
}

pub(super) fn recovery_handled_channel_key(channel_id: u64) -> String {
    format!("recovery_handled_channel:{channel_id}")
}

pub(super) fn watcher_has_post_work_ready_evidence(
    full_response: &str,
    tool_state: &WatcherToolState,
    _task_notification_kind: Option<TaskNotificationKind>,
) -> bool {
    !full_response.trim().is_empty() || tool_state.any_tool_used
}
