//! Persistent backing store for cancel-induced watcher-death tombstones (#1309).
//!
//! See the matching in-memory store at
//! `crate::services::discord::tmux::RECENT_TURN_STOPS` (PR #1277). The
//! in-memory copy is the fast path; this PG-backed mirror exists so a
//! dcserver restart between the cancel and the watcher's death observation
//! can still suppress the misleading 🔴 lifecycle notice.
//!
//! Lifecycle:
//! - `insert_cancel_tombstone` is awaited by the cancel path before the
//!   matching in-memory tombstone is allowed to be consumed as complete. The
//!   10-minute `expires_at` window mirrors `RECENT_TURN_STOP_TTL`.
//! - `delete_cancel_tombstones_by_client_ids` removes the exact PG rows for
//!   in-memory hits. `consume_cancel_tombstone` handles the post-restart case
//!   where memory is empty, returning the matching row AND deleting it in the
//!   same transaction so suppression remains one-shot per cancel (codex P1 on
//!   #1277).
//! - `prune_expired_cancel_tombstones` is invoked periodically by the
//!   `cancel_tombstone_pruner` maintenance worker so the table cannot grow
//!   without bound when the watcher never observes the death.

use std::sync::OnceLock;

use sqlx::{PgPool, Row};
use uuid::Uuid;

/// Global handle to the runtime PG pool, populated by `set_global_pool` from
/// `crate::server::run` during boot. Lets call sites that don't already
/// thread a `PgPool` through their signatures — for example
/// `turn_lifecycle::stop_turn_with_policy` — still mirror cancel tombstones
/// to PG (#1309).
static GLOBAL_PG_POOL: OnceLock<PgPool> = OnceLock::new();

pub fn set_global_pool(pool: PgPool) {
    let _ = GLOBAL_PG_POOL.set(pool);
}

pub fn global_pool() -> Option<&'static PgPool> {
    GLOBAL_PG_POOL.get()
}

/// Mirrors `crate::services::discord::tmux::RECENT_TURN_STOP_TTL`.
pub const CANCEL_TOMBSTONE_TTL_SECS: i64 = 10 * 60;

/// Mirrors `RECENT_TURN_STOP_METADATA_FALLBACK_TTL` — a tighter window used
/// when matching the watcher death back to a cancel that did not record an
/// `stop_output_offset`. We over-fetch within the 10-minute outer TTL and
/// re-check this 60s window in Rust.
pub const CANCEL_TOMBSTONE_FALLBACK_TTL_SECS: i64 = 60;

/// Same teardown grace as the in-memory store
/// (`CANCEL_TEARDOWN_GRACE_BYTES`). The wrapper writes ~2 KB of post-cancel
/// teardown bytes after the cancel boundary; anything beyond this small
/// buffer means the watcher already saw a follow-up turn's output and the
/// death is no longer attributable to the cancel.
pub const CANCEL_TEARDOWN_GRACE_BYTES: i64 = 4 * 1024;

// reason: public cancel-tombstone DTO for the read path; consumers are wired on
// selected cancel-attribution routes, not every compile target. See #3034.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct CancelTombstone {
    pub channel_id: i64,
    pub tmux_session_name: Option<String>,
    pub stop_output_offset: Option<i64>,
    pub reason: String,
}

/// Insert a cancel tombstone with `expires_at = NOW() + ttl`. The `client_id`
/// is the same UUID that the in-memory entry carries, letting the
/// in-process watcher delete the exact PG mirror after the durable write has
/// completed.
pub async fn insert_cancel_tombstone(
    pool: &PgPool,
    client_id: Uuid,
    channel_id: i64,
    tmux_session_name: Option<&str>,
    stop_output_offset: Option<i64>,
    reason: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO cancel_tombstones (
            client_id, channel_id, tmux_session_name, stop_output_offset, reason,
            recorded_at, expires_at
         ) VALUES ($1, $2, $3, $4, $5, NOW(), NOW() + ($6::bigint || ' seconds')::interval)
         ON CONFLICT (client_id) DO NOTHING",
    )
    .bind(client_id)
    .bind(channel_id)
    .bind(tmux_session_name)
    .bind(stop_output_offset)
    .bind(reason)
    .bind(CANCEL_TOMBSTONE_TTL_SECS)
    .execute(pool)
    .await
    .map(|_| ())
}

/// Delete exact PG mirrors for tombstones already matched by the in-memory
/// cache. The caller waits for the corresponding insert to finish before
/// invoking this, so a zero-row delete means another consumer already removed
/// the durable row.
pub async fn delete_cancel_tombstones_by_client_ids(
    pool: &PgPool,
    client_ids: &[Uuid],
) -> Result<u64, sqlx::Error> {
    if client_ids.is_empty() {
        return Ok(0);
    }
    let result = sqlx::query("DELETE FROM cancel_tombstones WHERE client_id = ANY($1)")
        .bind(client_ids)
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}

/// Look up + DELETE matching tombstones in a single transaction. Returns
/// `true` when at least one row matched and was consumed; the caller treats
/// that as "this watcher death was cancel-induced, suppress the lifecycle
/// notification".
///
/// Matching rules mirror
/// `crate::services::discord::tmux::cancel_induced_watcher_death`:
/// 1. Same `channel_id`.
/// 2. `tmux_session_name` matches OR is NULL on the tombstone (legacy
///    cancels recorded without a session name).
/// 3. Recorded within the 60s metadata-fallback window.
/// 4. If both `stop_output_offset` and `current_output_offset` are known,
///    require `current_output_offset <= stop_output_offset + grace`. Past
///    the grace boundary the death belongs to a follow-up turn and must
///    surface its own lifecycle signal.
pub async fn consume_cancel_tombstone(
    pool: &PgPool,
    channel_id: i64,
    tmux_session_name: &str,
    current_output_offset: Option<i64>,
) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;

    // Lock candidate rows so a concurrent consume on a different
    // dcserver / worker cannot double-suppress.
    let rows = sqlx::query(
        "SELECT id, tmux_session_name, stop_output_offset
         FROM cancel_tombstones
         WHERE channel_id = $1
           AND recorded_at >= NOW() - ($2::bigint || ' seconds')::interval
           AND (tmux_session_name IS NULL OR tmux_session_name = $3)
         ORDER BY recorded_at DESC
         FOR UPDATE",
    )
    .bind(channel_id)
    .bind(CANCEL_TOMBSTONE_FALLBACK_TTL_SECS)
    .bind(tmux_session_name)
    .fetch_all(&mut *tx)
    .await?;

    let mut suppress_ids: Vec<i64> = Vec::new();
    for row in rows {
        let id: i64 = row.try_get("id")?;
        let stop_offset: Option<i64> = row.try_get("stop_output_offset")?;

        if let (Some(stop), Some(current)) = (stop_offset, current_output_offset) {
            if current > stop.saturating_add(CANCEL_TEARDOWN_GRACE_BYTES) {
                // Follow-up turn output already past the cancel boundary.
                continue;
            }
        }
        suppress_ids.push(id);
    }

    if suppress_ids.is_empty() {
        tx.rollback().await?;
        return Ok(false);
    }

    sqlx::query("DELETE FROM cancel_tombstones WHERE id = ANY($1)")
        .bind(&suppress_ids)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;
    Ok(true)
}

/// Sweep expired tombstones. Runs from the maintenance scheduler so the
/// table cannot grow without bound when the watcher never observes a
/// cancel-induced death.
pub async fn prune_expired_cancel_tombstones(pool: &PgPool) -> Result<u64, sqlx::Error> {
    let result = sqlx::query("DELETE FROM cancel_tombstones WHERE expires_at < NOW()")
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}
