//! Host and occupancy observations around the existing thread GC SQL boundary.
use futures::future::BoxFuture;
use sqlx::PgPool;

use crate::db::dispatched_sessions::{
    delete_stale_thread_cleanup_row_pg, load_stale_thread_cleanup_rows_pg, thread_session_exists_pg,
};
use crate::db::session_agent_resolution::parse_thread_channel_id_from_session_key;
use crate::services::platform::tmux::{SessionPresence, session_presence};

/// O's thread child stores as thread GC reaches them: pinned before a row delete and asked to
/// retire only after that exact delete. The owner decides; GC grants it nothing else.
pub(crate) trait ChildStore: Sync {
    type Pinned: Send;
    fn pin(&self, thread: u64) -> Option<Self::Pinned>;
    fn retire<'a>(
        &'a self,
        pinned: Self::Pinned,
        successor: BoxFuture<'a, Result<bool, String>>,
    ) -> BoxFuture<'a, Result<(), String>>;
}

/// No child store to retire.
impl ChildStore for () {
    type Pinned = std::convert::Infallible;
    fn pin(&self, _thread: u64) -> Option<Self::Pinned> {
        None
    }
    fn retire<'a>(
        &'a self,
        pinned: Self::Pinned,
        _successor: BoxFuture<'a, Result<bool, String>>,
    ) -> BoxFuture<'a, Result<(), String>> {
        match pinned {}
    }
}

pub(crate) async fn gc_stale_thread_sessions_pg(
    pool: &PgPool,
    children: &impl ChildStore,
) -> Vec<String> {
    gc_stale_thread_sessions_retiring_pg(pool, probe_locally_owned_tmux, children).await
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

#[cfg(test)]
pub(crate) async fn gc_stale_thread_sessions_with_probe_pg<F, Fut>(
    pool: &PgPool,
    probe: F,
) -> Vec<String>
where
    F: Fn(String) -> Fut,
    Fut: std::future::Future<Output = SessionPresence>,
{
    gc_stale_thread_sessions_retiring_pg(pool, probe, &()).await
}

pub(crate) async fn gc_stale_thread_sessions_retiring_pg<F, Fut>(
    pool: &PgPool,
    probe: F,
    children: &impl ChildStore,
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
        // Pinned before the delete, so an input that arrives after it stales the request.
        let thread = parse_thread_channel_id_from_session_key(&key);
        let pinned = thread
            .as_deref()
            .and_then(|thread| thread.parse().ok())
            .and_then(|thread| children.pin(thread));
        // Recheck occupancy, the idle deadline and the judged row after the external probe.
        if delete_stale_thread_cleanup_row_pg(pool, &row)
            .await
            .unwrap_or(false)
        {
            if let (Some(thread), Some(pinned)) = (thread, pinned) {
                retire_child_store(pool, children, &thread, pinned).await;
            }
            deleted.push(key);
        }
    }
    deleted
}

/// Only this row's exact delete asks O to retire the thread's child store; a refusal keeps the
/// store active and is logged, never retried here.
async fn retire_child_store<C: ChildStore>(
    pool: &PgPool,
    children: &C,
    thread: &str,
    pinned: C::Pinned,
) {
    let successor = Box::pin(async {
        thread_session_exists_pg(pool, thread)
            .await
            .map_err(|error| format!("successor read failed: {error}"))
    });
    match children.retire(pinned, successor).await {
        Ok(()) => tracing::info!(
            thread_channel_id = thread,
            "[dispatched-sessions] thread child store retired after its row delete"
        ),
        Err(reason) => tracing::warn!(
            thread_channel_id = thread,
            %reason,
            "[dispatched-sessions] thread child store retirement refused; the store stays active"
        ),
    }
}

#[cfg(test)]
mod tests_pg;
