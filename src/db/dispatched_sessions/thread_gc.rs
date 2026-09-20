//! Reclaim stale thread rows only after their locally owned tmux is missing.

use sqlx::PgPool;

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
    let candidates = match sqlx::query_scalar::<_, String>(
        "SELECT session_key FROM sessions
         WHERE thread_channel_id IS NOT NULL
           AND status IN ('idle', 'disconnected', 'aborted')
           AND active_dispatch_id IS NULL
           AND COALESCE(active_children, 0) = 0
           AND COALESCE(last_heartbeat, created_at) < NOW() - INTERVAL '1 hour'",
    )
    .fetch_all(pool)
    .await
    {
        Ok(keys) => keys,
        Err(error) => {
            tracing::warn!(
                "[dispatched-sessions] gc_stale_thread_sessions_pg: failed to delete stale sessions: {error}"
            );
            Vec::new()
        }
    };
    let mut deleted = Vec::new();
    for key in candidates {
        if !crate::services::tmux_turn_liveness::idle_cleanup_session_is_unoccupied(pool, &key)
            .await
        {
            continue;
        }
        if probe(key.clone()).await != crate::services::platform::tmux::SessionPresence::Missing {
            continue;
        }
        // Recheck occupancy and the idle deadline after the external probe.
        let removed = sqlx::query(
            "DELETE FROM sessions
             WHERE session_key = $1
               AND thread_channel_id IS NOT NULL
               AND status IN ('idle', 'disconnected', 'aborted')
               AND active_dispatch_id IS NULL
               AND COALESCE(active_children, 0) = 0
               AND COALESCE(last_heartbeat, created_at) < NOW() - INTERVAL '1 hour'
               AND NOT EXISTS (
                   SELECT 1 FROM sessions child
                   WHERE child.parent_session_id = sessions.id AND child.closed_at IS NULL
               )",
        )
        .bind(&key)
        .execute(pool)
        .await;
        if removed.is_ok_and(|result| result.rows_affected() > 0) {
            deleted.push(key);
        }
    }
    deleted
}
