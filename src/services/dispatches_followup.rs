//! Dispatch follow-up outbox enqueue — service-layer helpers.
//!
//! Previously these helpers lived in `server/routes/dispatches/outbox.rs` and
//! were called back into from service code (`services::dispatches`,
//! `services::discord::turn_bridge`, `server::routes::review_verdict`). That
//! produced a service→route reverse edge in the dispatch call graph.
//!
//! The helpers themselves are pure DB writes (insert into `dispatch_outbox`)
//! with no HTTP/Axum surface, so their correct home is in the service layer.
//! The outbox worker loop still lives under `server::routes::dispatches::outbox`
//! because it owns the Discord side-effect transport — but the *enqueue* side
//! that callers need is now here.
//!
//! Manual dispatch completion (PATCH /api/dispatches/:id), outbox-driven
//! follow-up, review-verdict completion, and recovery/turn-bridge completion
//! all funnel through the same `queue_dispatch_followup_sync` / `_pg` pair,
//! giving the call graph a single finalize guard shape (see
//! `deduplicates_followup_for_manual_and_outbox_sources` test below).

use sqlx::PgPool;

/// Queue a dispatch completion follow-up row on the SQLite fallback DB.
///
/// This is a one-shot insert — `INSERT OR IGNORE` guarantees that repeat calls
/// for the same dispatch_id do not create duplicate follow-up rows, which is
/// what lets manual/outbox/recovery share the same guard.
pub fn queue_dispatch_followup(db: &crate::db::Db, dispatch_id: &str) {
    if let Ok(conn) = db.separate_conn() {
        conn.execute(
            "INSERT OR IGNORE INTO dispatch_outbox (dispatch_id, action) VALUES (?1, 'followup')",
            [dispatch_id],
        )
        .ok();
    }
}

/// Queue a dispatch completion follow-up row on Postgres.
///
/// `ON CONFLICT DO NOTHING` is the Postgres-side analog of the SQLite
/// `INSERT OR IGNORE`, preserving the single-finalize invariant for
/// manual/outbox/recovery callers.
pub async fn queue_dispatch_followup_pg(pg_pool: &PgPool, dispatch_id: &str) -> Result<(), String> {
    sqlx::query(
        "INSERT INTO dispatch_outbox (dispatch_id, action)
         VALUES ($1, 'followup')
         ON CONFLICT DO NOTHING",
    )
    .bind(dispatch_id)
    .execute(pg_pool)
    .await
    .map_err(|error| format!("enqueue postgres followup for {dispatch_id}: {error}"))?;
    Ok(())
}

/// Sync wrapper over `queue_dispatch_followup_pg` that falls back to the
/// SQLite path when no Postgres pool is available.
///
/// This is the single entry point used by callers that don't want to deal
/// with the async/sync boundary directly (service update_dispatch path,
/// verdict route, etc.). All delivery finalize paths go through this
/// function or through `queue_dispatch_followup_pg` directly — see the
/// unified-guard test below.
pub fn queue_dispatch_followup_sync(
    db: &crate::db::Db,
    pg_pool: Option<&PgPool>,
    dispatch_id: &str,
) {
    if let Some(pool) = pg_pool {
        let dispatch_id_owned = dispatch_id.to_string();
        if let Err(error) = crate::utils::async_bridge::block_on_pg_result(
            pool,
            move |bridge_pool| async move {
                queue_dispatch_followup_pg(&bridge_pool, &dispatch_id_owned).await
            },
            |error| error,
        ) {
            tracing::warn!(
                dispatch_id = %dispatch_id,
                "failed to enqueue postgres followup: {error}"
            );
        }
        return;
    }

    queue_dispatch_followup(db, dispatch_id);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    /// Unified finalize guard: manual + outbox + recovery all eventually call
    /// one of the `queue_dispatch_followup*` variants. Regardless of how many
    /// sources try to enqueue a follow-up for the same dispatch_id, the
    /// `INSERT OR IGNORE` / `ON CONFLICT DO NOTHING` pair ensures only one
    /// pending follow-up row exists at any time. This is the test that pins
    /// the "shared guard" half of the dispatch lifecycle DoD.
    #[test]
    fn deduplicates_followup_for_manual_and_outbox_sources() {
        let db = db::test_db();

        // Simulate: manual PATCH /api/dispatches/:id fires first
        queue_dispatch_followup(&db, "dispatch-shared-guard");
        // Then outbox worker retries enqueue
        queue_dispatch_followup(&db, "dispatch-shared-guard");
        // Then recovery engine re-tries on the same id
        queue_dispatch_followup(&db, "dispatch-shared-guard");

        let conn = db.separate_conn().expect("open conn");
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM dispatch_outbox
                 WHERE dispatch_id = ?1 AND action = 'followup'",
                ["dispatch-shared-guard"],
                |row| row.get(0),
            )
            .expect("count");
        assert_eq!(
            count, 1,
            "manual/outbox/recovery must share one finalize row"
        );
    }

    /// The sync wrapper falls back to SQLite cleanly when no Postgres pool is
    /// injected — mirroring manual dispatch update path and verdict route.
    #[test]
    fn sync_wrapper_without_pg_pool_inserts_into_sqlite() {
        let db = db::test_db();
        queue_dispatch_followup_sync(&db, None, "dispatch-sync-fallback");

        let conn = db.separate_conn().expect("open conn");
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM dispatch_outbox
                 WHERE dispatch_id = ?1 AND action = 'followup'",
                ["dispatch-sync-fallback"],
                |row| row.get(0),
            )
            .expect("count");
        assert_eq!(count, 1);
    }
}
