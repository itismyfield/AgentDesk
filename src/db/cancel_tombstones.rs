//! PG mirror of the in-memory cancel tombstones (`RECENT_TURN_STOPS` in
//! `services/discord/tmux_kill_policy.rs`), so a dcserver restart between a cancel and
//! the watcher's death observation still suppresses the misleading 🔴 lifecycle notice.
//!
//! Rows are deleted by `client_id` after an in-memory hit, consumed one-shot after a
//! restart, and swept by the `storage.cancel_tombstone_prune` maintenance job.

use std::sync::OnceLock;

use sqlx::{PgPool, Row};
use uuid::Uuid;

/// Runtime pool, set once at boot by `crate::server::run`, for callers with no
/// `PgPool` in scope (e.g. `turn_lifecycle::stop_turn_with_policy`).
static GLOBAL_PG_POOL: OnceLock<PgPool> = OnceLock::new();

pub fn set_global_pool(pool: PgPool) {
    let _ = GLOBAL_PG_POOL.set(pool);
}

pub fn global_pool() -> Option<&'static PgPool> {
    GLOBAL_PG_POOL.get()
}

/// Row lifetime; mirrors the in-memory `RECENT_TURN_STOP_TTL`.
pub const CANCEL_TOMBSTONE_TTL_SECS: i64 = 10 * 60;

/// A watcher death only matches tombstones recorded within this window (enforced in
/// SQL); mirrors the in-memory `RECENT_TURN_STOP_METADATA_FALLBACK_TTL`.
pub const CANCEL_TOMBSTONE_FALLBACK_TTL_SECS: i64 = 60;

/// Mirrors the in-memory `CANCEL_TEARDOWN_GRACE_BYTES`: output past the cancel offset
/// plus this teardown slack belongs to a follow-up turn, not to the cancel.
pub const CANCEL_TEARDOWN_GRACE_BYTES: i64 = 4 * 1024;

/// Insert a tombstone. `client_id` is the in-memory entry's UUID, so an in-process
/// hit can later delete exactly this row.
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

/// Delete the rows mirroring in-memory hits. Callers wait for the matching insert
/// first, so a zero-row delete means another consumer already removed the row.
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

/// Find and delete matching tombstones in one transaction, so suppression stays
/// one-shot per cancel. `true` means the watcher death was cancel-induced.
///
/// Matches like the in-memory `cancel_induced_watcher_death`, minus its generation check:
/// same channel, session equal or NULL, recorded within the fallback window, and (when
/// both offsets are known) `current <= stop + CANCEL_TEARDOWN_GRACE_BYTES`.
pub async fn consume_cancel_tombstone(
    pool: &PgPool,
    channel_id: i64,
    tmux_session_name: &str,
    current_output_offset: Option<i64>,
) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;

    // Row locks keep a concurrent consume (any worker or dcserver) from double-suppressing.
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

/// Sweep expired rows left behind when no watcher ever observes the death.
pub async fn prune_expired_cancel_tombstones(pool: &PgPool) -> Result<u64, sqlx::Error> {
    let result = sqlx::query("DELETE FROM cancel_tombstones WHERE expires_at < NOW()")
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}
