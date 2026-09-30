//! Reclaim stale thread rows only after their locally owned tmux is missing and
//! the hosted execution cleanup gate lets the row go.

use sqlx::PgPool;

use super::hosted_execution::CleanupRow;
use crate::db::session_agent_resolution::parse_thread_channel_id_from_session_key;

async fn backfill_legacy_thread_channel_ids_pg(pool: &PgPool) -> usize {
    let session_keys = match sqlx::query_scalar::<_, String>(
        "SELECT session_key
         FROM sessions
         WHERE thread_channel_id IS NULL",
    )
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(
                "[dispatched-sessions] backfill_legacy_thread_channel_ids_pg: failed to load session keys: {error}"
            );
            return 0;
        }
    };

    let mut updated = 0usize;
    for session_key in session_keys {
        let Some(thread_channel_id) = parse_thread_channel_id_from_session_key(&session_key) else {
            continue;
        };

        match sqlx::query(
            "UPDATE sessions
             SET thread_channel_id = $1
             WHERE session_key = $2
               AND thread_channel_id IS NULL",
        )
        .bind(&thread_channel_id)
        .bind(&session_key)
        .execute(pool)
        .await
        {
            Ok(result) => updated += result.rows_affected() as usize,
            Err(error) => tracing::warn!(
                "[dispatched-sessions] backfill_legacy_thread_channel_ids_pg: failed to update {}: {}",
                session_key,
                error
            ),
        }
    }

    updated
}

/// Reclaim only locally owned thread sessions whose tmux is confirmed missing.
/// Even a provider proven idle can start a new turn before DELETE; never
/// delete the canonical resume row while its tmux still exists.
pub async fn gc_stale_thread_sessions_pg(pool: &PgPool) -> Vec<String> {
    gc_stale_thread_sessions_with_probe_pg(pool, |key| async move {
        tokio::task::spawn_blocking(move || {
            use crate::services::platform::tmux::{SessionPresence, session_presence};
            let Some(tmux_name) =
                crate::services::discord::session_identity::tmux_name_from_session_key(&key)
            else {
                return SessionPresence::ProbeFailed;
            };
            // Missing on this host says nothing about a remote owner's tmux.
            let marker = crate::services::tmux_common::current_tmux_owner_marker();
            if !std::fs::read_to_string(crate::services::tmux_common::tmux_owner_path(&tmux_name))
                .is_ok_and(|owner| owner.trim() == marker)
            {
                return SessionPresence::ProbeFailed;
            }
            session_presence(&tmux_name)
        })
        .await
        .unwrap_or(crate::services::platform::tmux::SessionPresence::ProbeFailed)
    })
    .await
}

pub(crate) async fn gc_stale_thread_sessions_with_probe_pg<F, Fut>(
    pool: &PgPool,
    probe: F,
) -> Vec<String>
where
    F: Fn(String) -> Fut,
    Fut: std::future::Future<Output = crate::services::platform::tmux::SessionPresence>,
{
    let _ = backfill_legacy_thread_channel_ids_pg(pool).await;
    let candidates = sqlx::query(
        "SELECT s.id, s.session_key, s.provider, s.identity_kind, s.discord_token_hash,
                s.channel_id, s.hosted_execution,
                ARRAY(SELECT a.session_key FROM session_key_aliases a
                      WHERE a.session_id = s.id) AS aliases
         FROM sessions s
         WHERE s.thread_channel_id IS NOT NULL
           AND s.status IN ('idle', 'disconnected', 'aborted')
           AND s.active_dispatch_id IS NULL
           AND COALESCE(s.active_children, 0) = 0
           AND COALESCE(s.last_heartbeat, s.created_at) < NOW() - INTERVAL '1 hour'",
    )
    .fetch_all(pool)
    .await
    .and_then(|rows| {
        rows.iter()
            .map(CleanupRow::read)
            .collect::<Result<Vec<_>, _>>()
    });
    let candidates = match candidates {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(
                "[dispatched-sessions] gc_stale_thread_sessions_pg: failed to delete stale sessions: {error}"
            );
            Vec::new()
        }
    };
    let mut deleted = Vec::new();
    for row in candidates {
        // A live, unowned or unreadable record, or a non-tmux marker, is not a tmux session.
        let Some(key) = row.session_key.clone().filter(|_| row.deletable()) else {
            continue;
        };
        if !crate::services::tmux_turn_liveness::idle_cleanup_session_is_unoccupied(pool, &key)
            .await
        {
            continue;
        }
        if probe(key.clone()).await != crate::services::platform::tmux::SessionPresence::Missing {
            continue;
        }
        // Recheck occupancy, the idle deadline and the judged row after the external probe.
        let delete = sqlx::query(
            "DELETE FROM sessions
             WHERE id = $1
               AND thread_channel_id IS NOT NULL
               AND status IN ('idle', 'disconnected', 'aborted')
               AND active_dispatch_id IS NULL
               AND COALESCE(active_children, 0) = 0
               AND COALESCE(last_heartbeat, created_at) < NOW() - INTERVAL '1 hour'
               AND hosted_execution IS NOT DISTINCT FROM $2::JSONB
               AND provider IS NOT DISTINCT FROM $3 AND identity_kind IS NOT DISTINCT FROM $4
               AND discord_token_hash IS NOT DISTINCT FROM $5
               AND channel_id IS NOT DISTINCT FROM $6 AND session_key IS NOT DISTINCT FROM $7
               AND agentdesk_hosted_execution_deletable(hosted_execution)
               AND NOT EXISTS (
                   SELECT 1 FROM sessions child
                   WHERE child.parent_session_id = sessions.id AND child.closed_at IS NULL
               )",
        )
        .bind(row.id);
        let removed = row.bind_recheck(delete).execute(pool).await;
        if removed.is_ok_and(|result| result.rows_affected() > 0) {
            deleted.push(key);
        }
    }
    deleted
}
