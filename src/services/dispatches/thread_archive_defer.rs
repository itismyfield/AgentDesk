//! Whether a dispatch thread archive must wait: a fresh inflight row or an active session holds it.

use std::sync::OnceLock;

/// The inflight store's fresh-row probe for one channel, installed before any command runs.
static FRESH_INFLIGHT_PROBE: OnceLock<fn(u64) -> bool> = OnceLock::new();

pub(crate) fn install(probe: fn(u64) -> bool) {
    let _ = FRESH_INFLIGHT_PROBE.set(probe);
}

/// An uninstalled probe counts as fresh, like an unreadable row, so the archive waits.
fn has_fresh_inflight_for_channel(channel_id: u64) -> bool {
    FRESH_INFLIGHT_PROBE
        .get()
        .is_none_or(|probe| probe(channel_id))
}

async fn has_active_session_for_thread_pg(
    pg_pool: Option<&sqlx::PgPool>,
    thread_id: &str,
) -> Result<bool, String> {
    let Some(pool) = pg_pool else {
        return Ok(false);
    };

    let row = sqlx::query(
        "SELECT 1
         FROM sessions
         WHERE thread_channel_id = $1
           AND LOWER(COALESCE(status, '')) IN ('turn_active', 'working')
           AND COALESCE(last_heartbeat, created_at) > NOW() - INTERVAL '10 minutes'
         LIMIT 1",
    )
    .bind(thread_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load active session for thread {thread_id}: {error}"))?;

    Ok(row.is_some())
}

pub(crate) async fn should_defer_thread_archive_pg(
    pg_pool: Option<&sqlx::PgPool>,
    thread_id: &str,
) -> Result<bool, String> {
    if let Ok(channel_id) = thread_id.parse::<u64>()
        && has_fresh_inflight_for_channel(channel_id)
    {
        return Ok(true);
    }

    has_active_session_for_thread_pg(pg_pool, thread_id).await
}
