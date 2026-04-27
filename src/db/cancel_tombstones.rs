//! Persistent backing store for cancel-induced watcher-death tombstones (#1309).
//!
//! See the matching in-memory store at
//! `crate::services::discord::tmux::RECENT_TURN_STOPS` (PR #1277). The
//! in-memory copy is the fast path; this PG-backed mirror exists so a
//! dcserver restart between the cancel and the watcher's death observation
//! can still suppress the misleading 🔴 lifecycle notice.
//!
//! Lifecycle:
//! - `insert_cancel_tombstone` is called fire-and-forget alongside the
//!   in-memory record. The 10-minute `expires_at` window mirrors
//!   `RECENT_TURN_STOP_TTL`.
//! - `consume_cancel_tombstone` is called only when the in-memory store
//!   misses (post-restart case). It returns the matching row AND deletes it
//!   in the same transaction so suppression remains one-shot per cancel
//!   (codex P1 on #1277).
//! - `prune_expired_cancel_tombstones` is invoked periodically by the
//!   `cancel_tombstone_pruner` maintenance worker so the table cannot grow
//!   without bound when the watcher never observes the death.

use std::collections::VecDeque;
use std::sync::{LazyLock, Mutex, OnceLock};

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

/// Window during which a UUID drained by the in-memory consume can still
/// suppress the corresponding (possibly late-landing) PG row from being
/// honoured. Mirrors `RECENT_TURN_STOP_METADATA_FALLBACK_TTL` so the PG
/// shadow can never out-live the in-memory consume in practice. Codex
/// rounds 3/4 on PR #1310 — the shared `client_id` plus this drained-set
/// guards both the slow-PG cancel-race AND the late-PG-row stale-suppress
/// race at once.
const DRAINED_TTL_SECS: u64 = 60;
const DRAINED_CAPACITY: usize = 256;

#[derive(Debug, Clone, Copy)]
struct DrainedEntry {
    id: Uuid,
    drained_at: std::time::Instant,
}

static RECENTLY_DRAINED_IDS: LazyLock<Mutex<VecDeque<DrainedEntry>>> =
    LazyLock::new(|| Mutex::new(VecDeque::with_capacity(DRAINED_CAPACITY)));

fn drained_ids() -> std::sync::MutexGuard<'static, VecDeque<DrainedEntry>> {
    match RECENTLY_DRAINED_IDS.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn prune_drained_ids(entries: &mut VecDeque<DrainedEntry>, now: std::time::Instant) {
    let ttl = std::time::Duration::from_secs(DRAINED_TTL_SECS);
    entries.retain(|entry| now.saturating_duration_since(entry.drained_at) <= ttl);
}

/// Register a UUID as having been consumed by the in-memory watcher path.
/// `consume_cancel_tombstone` will skip + delete any PG row carrying this
/// UUID for the next `DRAINED_TTL_SECS`, so a late-landing PG row from the
/// fire-and-forget insert cannot false-suppress an unrelated watcher death
/// later in the fallback window.
pub fn register_drained_ids(ids: &[Uuid]) {
    if ids.is_empty() {
        return;
    }
    let now = std::time::Instant::now();
    let mut entries = drained_ids();
    prune_drained_ids(&mut entries, now);
    for id in ids {
        while entries.len() >= DRAINED_CAPACITY {
            entries.pop_front();
        }
        entries.push_back(DrainedEntry {
            id: *id,
            drained_at: now,
        });
    }
}

fn is_id_drained(entries: &VecDeque<DrainedEntry>, id: &Uuid) -> bool {
    entries.iter().any(|entry| entry.id == *id)
}

#[cfg(test)]
pub fn clear_drained_ids_for_tests() {
    drained_ids().clear();
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
/// teardown bytes after the cancel boundary; anything beyond 16 KB means
/// the watcher already saw a follow-up turn's output and the death is no
/// longer attributable to the cancel.
pub const CANCEL_TEARDOWN_GRACE_BYTES: i64 = 16 * 1024;

#[derive(Debug, Clone)]
pub struct CancelTombstone {
    pub channel_id: i64,
    pub tmux_session_name: Option<String>,
    pub stop_output_offset: Option<i64>,
    pub reason: String,
}

/// Insert a cancel tombstone with `expires_at = NOW() + ttl`. The
/// `client_id` is the same UUID that the in-memory entry carries — it is
/// the cross-layer key that lets `consume_cancel_tombstone` recognise rows
/// already drained via the in-memory path.
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
        "SELECT id, client_id, tmux_session_name, stop_output_offset
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

    let drained_snapshot = {
        let mut entries = drained_ids();
        prune_drained_ids(&mut entries, std::time::Instant::now());
        entries.clone()
    };

    // Codex round-3/4 P2 on PR #1310: `delete_only_ids` are rows whose
    // `client_id` was already drained by the in-memory consume — typically
    // a late-landing PG row that arrived after the in-process watcher
    // already suppressed via the in-memory tombstone. We DELETE these so
    // the table doesn't accumulate stale rows AND so they can't false-
    // suppress an unrelated future watcher death within the fallback
    // window. We do NOT count them as a consume hit.
    let mut suppress_ids: Vec<i64> = Vec::new();
    let mut delete_only_ids: Vec<i64> = Vec::new();
    for row in rows {
        let id: i64 = row.try_get("id")?;
        let client_id: Uuid = row.try_get("client_id")?;
        let stop_offset: Option<i64> = row.try_get("stop_output_offset")?;

        if is_id_drained(&drained_snapshot, &client_id) {
            delete_only_ids.push(id);
            continue;
        }

        if let (Some(stop), Some(current)) = (stop_offset, current_output_offset) {
            if current > stop.saturating_add(CANCEL_TEARDOWN_GRACE_BYTES) {
                // Follow-up turn output already past the cancel boundary.
                continue;
            }
        }
        suppress_ids.push(id);
    }

    let mut to_delete: Vec<i64> = Vec::with_capacity(suppress_ids.len() + delete_only_ids.len());
    to_delete.extend(suppress_ids.iter().copied());
    to_delete.extend(delete_only_ids.iter().copied());

    if to_delete.is_empty() {
        tx.rollback().await?;
        return Ok(false);
    }

    sqlx::query("DELETE FROM cancel_tombstones WHERE id = ANY($1)")
        .bind(&to_delete)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;
    Ok(!suppress_ids.is_empty())
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

#[cfg(test)]
pub async fn count_cancel_tombstones_for_tests(pool: &PgPool) -> Result<i64, sqlx::Error> {
    let row = sqlx::query("SELECT COUNT(*)::BIGINT AS c FROM cancel_tombstones")
        .fetch_one(pool)
        .await?;
    row.try_get("c")
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

    /// Per-test PG database wrapper. Holds the lifecycle guard for the
    /// duration of the test and drops the temporary database in `teardown`
    /// so repeated PG-enabled runs don't leak `agentdesk_cancel_tombstones_*`
    /// databases (codex round-1 P3 on PR #1310).
    struct TestPg {
        pool: PgPool,
        admin_url: String,
        database_name: String,
        _lifecycle: crate::db::postgres::PostgresTestLifecycleGuard,
    }

    impl TestPg {
        async fn teardown(self) {
            let TestPg {
                pool,
                admin_url,
                database_name,
                _lifecycle,
            } = self;
            // Close pool before the admin DROP so PG isn't holding sessions
            // open against the database we're about to drop.
            pool.close().await;
            let _ = crate::db::postgres::drop_test_database(
                &admin_url,
                &database_name,
                "cancel_tombstones tests",
            )
            .await;
        }
    }

    /// Spin up an isolated PG database for this test, run all migrations,
    /// then return a `TestPg` guard. Gated on `POSTGRES_TEST_DATABASE_URL_BASE`
    /// — without it the helper returns `None` and tests no-op so
    /// `cargo test` stays green on machines without a local PG server.
    async fn fresh_pg() -> Option<TestPg> {
        use crate::db::postgres;
        let base = std::env::var("POSTGRES_TEST_DATABASE_URL_BASE").ok()?;
        let trimmed = base.trim().trim_end_matches('/');
        if trimmed.is_empty() {
            return None;
        }
        let admin_db = std::env::var("POSTGRES_TEST_ADMIN_DB")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "postgres".to_string());
        let admin_url = format!("{}/{}", trimmed, admin_db);
        let database_name = format!(
            "agentdesk_cancel_tombstones_{}",
            uuid::Uuid::new_v4().simple()
        );
        let lifecycle = postgres::lock_test_lifecycle();
        postgres::create_test_database(&admin_url, &database_name, "cancel_tombstones tests")
            .await
            .ok()?;

        let connect_url = format!("{}/{}", trimmed, database_name);
        let opts: PgConnectOptions = connect_url.parse().ok()?;
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .connect_with(opts)
            .await
            .ok()?;
        postgres::migrate(&pool).await.ok()?;
        Some(TestPg {
            pool,
            admin_url,
            database_name,
            _lifecycle: lifecycle,
        })
    }

    /// A successful insert + matching consume returns true and leaves the
    /// table empty (one-shot consume).
    #[tokio::test]
    async fn insert_then_consume_is_one_shot() {
        let Some(test_pg) = fresh_pg().await else {
            eprintln!("[cancel_tombstones] PG unavailable; skipping insert_then_consume test");
            return;
        };
        let pool = test_pg.pool.clone();

        insert_cancel_tombstone(
            &pool,
            Uuid::new_v4(),
            4242,
            Some("AgentDesk-codex-x"),
            Some(128),
            "user-cancel",
        )
        .await
        .expect("insert");
        assert_eq!(count_cancel_tombstones_for_tests(&pool).await.unwrap(), 1);

        let consumed = consume_cancel_tombstone(&pool, 4242, "AgentDesk-codex-x", Some(128))
            .await
            .expect("consume");
        assert!(consumed, "first consume returns true");
        assert_eq!(count_cancel_tombstones_for_tests(&pool).await.unwrap(), 0);

        // Second call sees nothing.
        let consumed_again = consume_cancel_tombstone(&pool, 4242, "AgentDesk-codex-x", Some(128))
            .await
            .expect("consume idempotent");
        assert!(!consumed_again, "second consume returns false");

        test_pg.teardown().await;
    }

    /// Beyond cancel offset + teardown grace, the death is unrelated and
    /// must NOT consume the tombstone.
    #[tokio::test]
    async fn consume_skips_when_past_cancel_eof() {
        let Some(test_pg) = fresh_pg().await else {
            return;
        };
        let pool = test_pg.pool.clone();

        let stop_offset: i64 = 1024;
        insert_cancel_tombstone(
            &pool,
            Uuid::new_v4(),
            5151,
            Some("AgentDesk-codex-eof"),
            Some(stop_offset),
            "user-cancel",
        )
        .await
        .expect("insert");

        let post_followup = stop_offset + CANCEL_TEARDOWN_GRACE_BYTES + 4096;
        let consumed =
            consume_cancel_tombstone(&pool, 5151, "AgentDesk-codex-eof", Some(post_followup))
                .await
                .expect("consume");
        assert!(
            !consumed,
            "death past cancel EOF + grace is not cancel-induced"
        );
        // Tombstone still present for legitimate later consumer (TTL).
        assert_eq!(count_cancel_tombstones_for_tests(&pool).await.unwrap(), 1);

        test_pg.teardown().await;
    }

    /// Mismatched `tmux_session_name` is rejected even when the channel
    /// matches.
    #[tokio::test]
    async fn consume_skips_when_session_name_mismatch() {
        let Some(test_pg) = fresh_pg().await else {
            return;
        };
        let pool = test_pg.pool.clone();

        insert_cancel_tombstone(
            &pool,
            Uuid::new_v4(),
            6262,
            Some("AgentDesk-codex-A"),
            None,
            "user-cancel",
        )
        .await
        .expect("insert");

        let consumed = consume_cancel_tombstone(&pool, 6262, "AgentDesk-codex-OTHER", None)
            .await
            .expect("consume");
        assert!(!consumed);
        assert_eq!(count_cancel_tombstones_for_tests(&pool).await.unwrap(), 1);

        test_pg.teardown().await;
    }

    /// `prune_expired_cancel_tombstones` removes rows whose `expires_at`
    /// has passed and leaves the live ones in place.
    #[tokio::test]
    async fn prune_drops_expired_rows() {
        let Some(test_pg) = fresh_pg().await else {
            return;
        };
        let pool = test_pg.pool.clone();

        // Live row.
        insert_cancel_tombstone(
            &pool,
            Uuid::new_v4(),
            7373,
            Some("live"),
            None,
            "user-cancel",
        )
        .await
        .expect("insert live");
        // Expired row — backdate explicitly.
        sqlx::query(
            "INSERT INTO cancel_tombstones (
                client_id, channel_id, tmux_session_name, reason, recorded_at, expires_at
             ) VALUES ($1, $2, $3, $4, NOW() - INTERVAL '20 minutes', NOW() - INTERVAL '5 minutes')",
        )
        .bind(Uuid::new_v4())
        .bind(7374_i64)
        .bind("expired")
        .bind("user-cancel")
        .execute(&pool)
        .await
        .expect("insert expired");

        assert_eq!(count_cancel_tombstones_for_tests(&pool).await.unwrap(), 2);
        let deleted = prune_expired_cancel_tombstones(&pool).await.expect("prune");
        assert_eq!(deleted, 1, "only the expired row should be removed");
        assert_eq!(count_cancel_tombstones_for_tests(&pool).await.unwrap(), 1);

        test_pg.teardown().await;
    }
}
