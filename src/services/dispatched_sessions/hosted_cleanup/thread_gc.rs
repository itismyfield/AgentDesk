//! Host and occupancy observations around the existing thread GC SQL boundary.
use sqlx::PgPool;

use crate::db::dispatched_sessions::{
    delete_stale_thread_cleanup_row_pg, load_stale_thread_cleanup_rows_pg,
};
use crate::services::platform::tmux::{SessionPresence, session_presence};

pub(crate) async fn gc_stale_thread_sessions_pg(pool: &PgPool) -> Vec<String> {
    gc_stale_thread_sessions_with_probe_pg(pool, probe_locally_owned_tmux).await
}

async fn probe_locally_owned_tmux(key: String) -> SessionPresence {
    tokio::task::spawn_blocking(move || {
        let Some(tmux_name) = crate::domain::session_identity::tmux_name_from_session_key(&key)
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
    .unwrap_or(SessionPresence::ProbeFailed)
}

pub(crate) async fn gc_stale_thread_sessions_with_probe_pg<F, Fut>(
    pool: &PgPool,
    probe: F,
) -> Vec<String>
where
    F: Fn(String) -> Fut,
    Fut: std::future::Future<Output = SessionPresence>,
{
    let candidates = match load_stale_thread_cleanup_rows_pg(pool).await {
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
        let Some(key) = row.session_key.clone() else {
            continue;
        };
        if !crate::services::tmux_turn_liveness::cleanup_row_is_unoccupied_pg(pool, &key, &row)
            .await
        {
            continue;
        }
        if probe(key.clone()).await != SessionPresence::Missing {
            continue;
        }
        // Recheck occupancy, the idle deadline and the judged row after the external probe.
        if delete_stale_thread_cleanup_row_pg(pool, &row)
            .await
            .unwrap_or(false)
        {
            deleted.push(key);
        }
    }
    deleted
}

#[cfg(test)]
mod tests_pg;
