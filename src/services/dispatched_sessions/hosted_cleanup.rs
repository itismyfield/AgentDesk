//! Marker observations around the existing hosted cleanup database boundaries.
pub(crate) mod thread_gc;

use sqlx::PgPool;

use crate::db::dispatched_sessions::hosted_execution::{
    CleanupRow, changed_after_check, delete_disconnected_sessions_pg, load_cleanup_row_pg,
    load_disconnected_cleanup_rows_pg, load_session_delete_candidate_pg, wait_out_row_lock_pg,
};
use crate::db::dispatched_sessions::{DeleteSessionResult, apply_session_delete_pg};
use crate::services::tmux_turn_liveness::cleanup_host::cleanup_row_deletable;

pub(crate) async fn cleanup_disconnected_sessions_pg(pool: &PgPool) -> Result<u64, String> {
    let judged = load_disconnected_cleanup_rows_pg(pool)
        .await?
        .into_iter()
        .filter(cleanup_row_deletable)
        .collect::<Vec<_>>();
    delete_disconnected_sessions_pg(pool, &judged).await
}

pub(crate) async fn delete_session_by_key_pg(
    pool: &PgPool,
    session_key: &str,
) -> Result<DeleteSessionResult, String> {
    let judged = judge_session_delete_pg(pool, session_key).await?;
    apply_session_delete_pg(pool, session_key, judged.as_ref()).await
}

/// Judges markers unlocked, including those written while the first row lock was held.
async fn judge_session_delete_pg(
    pool: &PgPool,
    session_key: &str,
) -> Result<Option<CleanupRow>, String> {
    let Some(row) = load_session_delete_candidate_pg(pool, session_key).await? else {
        return Ok(None);
    };
    refuse_kept_row(&row)?;
    wait_out_row_lock_pg(pool, session_key).await?;
    match load_cleanup_row_pg(pool, row.id).await? {
        None => Ok(None),
        Some(current) if current != row => Err(changed_after_check(row.id)),
        Some(current) => refuse_kept_row(&current).map(|()| Some(current)),
    }
}

fn refuse_kept_row(row: &CleanupRow) -> Result<(), String> {
    if cleanup_row_deletable(row) {
        return Ok(());
    }
    Err(format!(
        "session {} keeps a live, unowned or unreadable hosted execution record \
         or a non-tmux host marker",
        row.id
    ))
}
