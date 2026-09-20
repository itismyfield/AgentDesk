use super::super::*;

/// Periodic GC: delete stale idle/disconnected thread sessions from DB.
pub(super) async fn gc_stale_thread_sessions(shared: &Arc<SharedData>) {
    let Some(pool) = shared.pg_pool.as_ref() else {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!("  [{ts}] ⚠ Thread session GC skipped: postgres pool unavailable");
        return;
    };
    let deleted_keys = crate::db::dispatched_sessions::gc_stale_thread_sessions_pg(pool).await;
    if deleted_keys.is_empty() {
        return;
    }
    // DB GC only removes confirmed-missing tmux sessions. Do not reap after
    // deleting rows: a replacement provider could now occupy the same name.
    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!(
        "  [{ts}] 🧹 GC: removed {} stale thread session(s) with confirmed-missing tmux",
        deleted_keys.len(),
    );
}

/// Periodic GC: disconnect stale fixed-channel working sessions from the DB so
/// restart recovery cannot restore dead provider session IDs.
pub(super) async fn gc_stale_fixed_working_sessions(shared: &Arc<SharedData>) {
    let Some(pool) = shared.pg_pool.as_ref() else {
        return;
    };
    let cleared = crate::db::dispatched_sessions::gc_stale_fixed_working_sessions_db_pg(pool).await;

    if cleared > 0 {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] 🧹 GC: disconnected {cleared} stale fixed-channel working session(s)"
        );
    }
}
