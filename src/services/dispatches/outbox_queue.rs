//! Dispatch outbox queue + state-transition service (#1694).
//!
//! Owns the worker that drains `dispatch_outbox` rows, calls the configured
//! notifier (real Discord transport in production, mock in tests), and
//! transitions row state (`pending` → `processing` → `done`/`failed`/retry)
//! plus the `task_dispatches.status = 'dispatched'` flip on first successful
//! notify. Extracted from `src/server/routes/dispatches/outbox.rs` so the
//! route layer no longer mixes raw SQL + queue semantics +
//! `crate::services::*` calls (route SRP audit, #1282).
//!
//! All persistence goes through `crate::db::dispatches::outbox`; this module
//! holds no SQL of its own.

use crate::db::dispatches::outbox::{
    DispatchOutboxRow, claim_pending_dispatch_outbox_batch_pg,
    dispatch_notify_delivery_suppressed_pg, mark_dispatch_dispatched_pg, mark_outbox_done_pg,
    mark_outbox_failed_pg, schedule_outbox_retry_pg,
};
use crate::services::dispatches::discord_delivery::DispatchNotifyDeliveryResult;
use sqlx::PgPool;
use std::sync::Arc;

// ── Outbox worker trait ───────────────────────────────────────

/// Trait for outbox side-effects (Discord notifications, followups).
/// Extracted from `dispatch_outbox_loop` to allow mock injection in tests.
pub(crate) trait OutboxNotifier: Send + Sync {
    fn notify_dispatch(
        &self,
        db: Option<crate::db::Db>,
        agent_id: String,
        title: String,
        card_id: String,
        dispatch_id: String,
    ) -> impl std::future::Future<Output = Result<DispatchNotifyDeliveryResult, String>> + Send;

    fn handle_followup(
        &self,
        db: Option<crate::db::Db>,
        dispatch_id: String,
    ) -> impl std::future::Future<Output = Result<(), String>> + Send;

    fn sync_status_reaction(
        &self,
        db: Option<crate::db::Db>,
        dispatch_id: String,
    ) -> impl std::future::Future<Output = Result<(), String>> + Send;
}

/// Production notifier that calls the real Discord functions.
pub(crate) struct RealOutboxNotifier {
    pub(crate) pg_pool: Arc<PgPool>,
}

impl RealOutboxNotifier {
    pub(crate) fn new(pg_pool: Arc<PgPool>) -> Self {
        Self { pg_pool }
    }
}

impl OutboxNotifier for RealOutboxNotifier {
    async fn notify_dispatch(
        &self,
        db: Option<crate::db::Db>,
        agent_id: String,
        title: String,
        card_id: String,
        dispatch_id: String,
    ) -> Result<DispatchNotifyDeliveryResult, String> {
        // The route layer owns the actual Discord transport call (slot
        // thread reuse, message rendering, two-phase delivery guard).
        // #1694 keeps that side here as a thin shim so the queue worker
        // is free of Discord/HTTP detail.
        crate::server::routes::dispatches::send_dispatch_to_discord_with_pg_result(
            db.as_ref(),
            Some(self.pg_pool.as_ref()),
            &agent_id,
            &title,
            &card_id,
            &dispatch_id,
        )
        .await
    }

    async fn handle_followup(
        &self,
        db: Option<crate::db::Db>,
        dispatch_id: String,
    ) -> Result<(), String> {
        crate::services::dispatches::outbox_route::handle_completed_dispatch_followups_with_pg(
            db.as_ref(),
            Some(self.pg_pool.as_ref()),
            &dispatch_id,
        )
        .await
    }

    async fn sync_status_reaction(
        &self,
        db: Option<crate::db::Db>,
        dispatch_id: String,
    ) -> Result<(), String> {
        crate::server::routes::dispatches::sync_dispatch_status_reaction_with_pg(
            db.as_ref(),
            Some(self.pg_pool.as_ref()),
            &dispatch_id,
        )
        .await
    }
}

/// Backoff delays for outbox retries: 1m → 5m → 15m → 1h
const RETRY_BACKOFF_SECS: [i64; 4] = [60, 300, 900, 3600];
/// Maximum number of retries before marking as permanent failure.
const MAX_RETRY_COUNT: i64 = 4;

/// Invariant: `dispatch_outbox_retry_count_in_bounds`.
///
/// `dispatch_outbox.retry_count` is a `bigint` (i64 in Rust) with
/// `MAX_RETRY_COUNT = 4`. A valid row satisfies `0 <= retry_count <=
/// MAX_RETRY_COUNT + 1`. The `+1` slack tolerates the one-tick window between
/// `new_count = retry_count + 1` and the status flip to `failed`. Violations
/// are observed via `record_invariant_check` — no panic in release. See
/// `docs/invariants.md`.
fn check_dispatch_outbox_retry_count_in_bounds(
    outbox_id: i64,
    dispatch_id: &str,
    retry_count: i64,
) {
    let ok = retry_count >= 0 && retry_count <= MAX_RETRY_COUNT + 1;
    crate::services::observability::record_invariant_check(
        ok,
        crate::services::observability::InvariantViolation {
            provider: None,
            channel_id: None,
            dispatch_id: Some(dispatch_id),
            session_key: None,
            turn_id: None,
            invariant: "dispatch_outbox_retry_count_in_bounds",
            code_location: "src/services/dispatches/outbox_queue.rs:check_dispatch_outbox_retry_count_in_bounds",
            message: "dispatch_outbox.retry_count is out of valid bounds",
            details: serde_json::json!({
                "outbox_id": outbox_id,
                "dispatch_id": dispatch_id,
                "retry_count": retry_count,
                "max_retry_count": MAX_RETRY_COUNT,
            }),
        },
    );
}

fn dispatch_delivery_result_json(result: &DispatchNotifyDeliveryResult) -> String {
    serde_json::to_string(result).unwrap_or_else(|error| {
        serde_json::json!({
            "status": "serialization_failed",
            "detail": error.to_string(),
            "dispatch_id": result.dispatch_id,
            "action": result.action,
        })
        .to_string()
    })
}

fn generic_outbox_delivery_result(
    dispatch_id: &str,
    action: &str,
    detail: impl Into<String>,
) -> DispatchNotifyDeliveryResult {
    DispatchNotifyDeliveryResult::success(dispatch_id, action, detail)
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
fn dispatch_notify_delivery_suppressed_sqlite(
    conn: &sqlite_test::Connection,
    dispatch_id: &str,
) -> bool {
    let status: Option<String> = conn
        .query_row(
            "SELECT status FROM task_dispatches WHERE id = ?1",
            [dispatch_id],
            |row| row.get(0),
        )
        .ok()
        .flatten();
    matches!(
        status.as_deref(),
        Some("completed") | Some("failed") | Some("cancelled")
    )
}

/// Process one batch of pending outbox entries.
/// Returns the number of entries processed (0 if queue was empty).
///
/// Retry/backoff policy (#209):
/// - On notifier success: mark entry as 'done'
/// - On notifier failure (retry_count < MAX_RETRY_COUNT): increment retry_count,
///   set next_attempt_at with exponential backoff, revert to 'pending'
/// - On max retry exceeded: mark as 'failed' (permanent failure)
/// - For 'notify' actions: manages dispatch_notified reservation atomically
pub(crate) async fn process_outbox_batch<N: OutboxNotifier>(
    db: &crate::db::Db,
    notifier: &N,
) -> usize {
    #[cfg(all(test, feature = "legacy-sqlite-tests"))]
    {
        return process_outbox_batch_with_pg(Some(db), None, notifier, None).await;
    }
    #[cfg(not(feature = "legacy-sqlite-tests"))]
    {
        let _ = (db, notifier);
        0
    }
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
async fn process_outbox_batch_sqlite<N: OutboxNotifier>(db: &crate::db::Db, notifier: &N) -> usize {
    let pending: Vec<DispatchOutboxRow> = {
        let conn = match db.lock() {
            Ok(c) => c,
            Err(_) => return 0,
        };
        let mut stmt = match conn.prepare(
            "SELECT id, dispatch_id, action, agent_id, card_id, title, retry_count
             FROM dispatch_outbox
             WHERE status = 'pending'
               AND (next_attempt_at IS NULL OR next_attempt_at <= datetime('now'))
             ORDER BY id ASC LIMIT 5",
        ) {
            Ok(s) => s,
            Err(_) => return 0,
        };
        stmt.query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
                None,
            ))
        })
        .ok()
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
    };

    let count = pending.len();
    for (id, dispatch_id, action, agent_id, card_id, title, retry_count, _) in pending {
        check_dispatch_outbox_retry_count_in_bounds(id, &dispatch_id, retry_count);

        if action == "notify" {
            let suppress_delivery = db
                .lock()
                .map(|conn| dispatch_notify_delivery_suppressed_sqlite(&conn, &dispatch_id))
                .unwrap_or(false);
            if suppress_delivery {
                let delivery_result = generic_outbox_delivery_result(
                    &dispatch_id,
                    "notify",
                    "suppressed because dispatch is already terminal",
                );
                let delivery_result_json = dispatch_delivery_result_json(&delivery_result);
                if let Ok(conn) = db.lock() {
                    conn.execute(
                        "UPDATE dispatch_outbox
                            SET status = 'done',
                                processed_at = datetime('now'),
                                error = NULL,
                                delivery_status = ?2,
                                delivery_result = ?3
                          WHERE id = ?1",
                        sqlite_test::params![id, delivery_result.status, delivery_result_json],
                    )
                    .ok();
                }
                continue;
            }
        }

        if let Ok(conn) = db.lock() {
            conn.execute(
                "UPDATE dispatch_outbox SET status = 'processing' WHERE id = ?1",
                [id],
            )
            .ok();
        }

        let result = match action.as_str() {
            "notify" => {
                if let (Some(aid), Some(cid), Some(t)) =
                    (agent_id.clone(), card_id.clone(), title.clone())
                {
                    notifier
                        .notify_dispatch(Some(db.clone()), aid, t, cid, dispatch_id.clone())
                        .await
                } else {
                    Err("missing agent_id, card_id, or title for notify action".into())
                }
            }
            "followup" => notifier
                .handle_followup(Some(db.clone()), dispatch_id.clone())
                .await
                .map(|()| {
                    generic_outbox_delivery_result(
                        &dispatch_id,
                        "followup",
                        "followup handler completed",
                    )
                }),
            "status_reaction" => notifier
                .sync_status_reaction(Some(db.clone()), dispatch_id.clone())
                .await
                .map(|()| {
                    generic_outbox_delivery_result(
                        &dispatch_id,
                        "status_reaction",
                        "status reaction sync completed",
                    )
                }),
            other => {
                tracing::warn!("[dispatch-outbox] Unknown action: {other}");
                Err(format!("unknown action: {other}"))
            }
        };

        match result {
            Ok(delivery_result) => {
                let delivery_result_json = dispatch_delivery_result_json(&delivery_result);
                if let Ok(conn) = db.lock() {
                    conn.execute(
                        "UPDATE dispatch_outbox
                            SET status = 'done',
                                processed_at = datetime('now'),
                                error = NULL,
                                delivery_status = ?2,
                                delivery_result = ?3
                          WHERE id = ?1",
                        sqlite_test::params![id, delivery_result.status, delivery_result_json],
                    )
                    .ok();
                    if action == "notify" {
                        crate::dispatch::set_dispatch_status_on_conn(
                            &conn,
                            &dispatch_id,
                            "dispatched",
                            None,
                            "dispatch_outbox_notify",
                            Some(&["pending"]),
                            false,
                        )
                        .ok();
                    }
                }
            }
            Err(err) => {
                let new_count = retry_count + 1;
                if new_count > MAX_RETRY_COUNT {
                    let delivery_result = DispatchNotifyDeliveryResult::permanent_failure(
                        &dispatch_id,
                        &action,
                        &err,
                    );
                    let delivery_result_json = dispatch_delivery_result_json(&delivery_result);
                    if let Ok(conn) = db.lock() {
                        conn.execute(
                            "UPDATE dispatch_outbox
                                SET status = 'failed',
                                    error = ?1,
                                    retry_count = ?2,
                                    processed_at = datetime('now'),
                                    delivery_status = ?4,
                                    delivery_result = ?5
                              WHERE id = ?3",
                            sqlite_test::params![
                                err,
                                new_count,
                                id,
                                delivery_result.status,
                                delivery_result_json
                            ],
                        )
                        .ok();
                    }
                } else {
                    let backoff_idx = (new_count - 1) as usize;
                    let backoff_secs = RETRY_BACKOFF_SECS.get(backoff_idx).copied().unwrap_or(3600);
                    if let Ok(conn) = db.lock() {
                        conn.execute(
                            "UPDATE dispatch_outbox
                                SET status = 'pending',
                                    error = ?1,
                                    retry_count = ?2,
                                    next_attempt_at = datetime('now', '+' || ?3 || ' seconds')
                              WHERE id = ?4",
                            sqlite_test::params![err, new_count, backoff_secs, id],
                        )
                        .ok();
                    }
                }
            }
        }
    }

    count
}

pub(crate) async fn process_outbox_batch_with_pg<N: OutboxNotifier>(
    db: Option<&crate::db::Db>,
    pg_pool: Option<&PgPool>,
    notifier: &N,
    claim_owner: Option<&str>,
) -> usize {
    #[cfg(all(test, feature = "legacy-sqlite-tests"))]
    if pg_pool.is_none() {
        return match db {
            Some(db) => process_outbox_batch_sqlite(db, notifier).await,
            None => 0,
        };
    }

    #[cfg(not(feature = "legacy-sqlite-tests"))]
    let _ = (db, notifier);
    let Some(pool) = pg_pool else {
        return 0;
    };
    let pending: Vec<DispatchOutboxRow> =
        claim_pending_dispatch_outbox_batch_pg(pool, claim_owner.unwrap_or("dispatch-outbox"))
            .await;

    let count = pending.len();
    for (id, dispatch_id, action, agent_id, card_id, title, retry_count, _) in pending {
        check_dispatch_outbox_retry_count_in_bounds(id, &dispatch_id, retry_count);
        if action == "notify" {
            let suppress_delivery = dispatch_notify_delivery_suppressed_pg(pool, &dispatch_id)
                .await
                .unwrap_or(false);
            if suppress_delivery {
                let delivery_result = generic_outbox_delivery_result(
                    &dispatch_id,
                    "notify",
                    "suppressed because dispatch is already terminal",
                );
                let delivery_result_json = dispatch_delivery_result_json(&delivery_result);
                mark_outbox_done_pg(pool, id, &delivery_result.status, &delivery_result_json).await;
                continue;
            }
        }

        let result = match action.as_str() {
            "notify" => {
                if let (Some(aid), Some(cid), Some(t)) =
                    (agent_id.clone(), card_id.clone(), title.clone())
                {
                    // Two-phase delivery guard (reservation + notified marker) is handled
                    // inside send_dispatch_to_discord, protecting all callers uniformly.
                    notifier
                        .notify_dispatch(db.cloned(), aid, t, cid, dispatch_id.clone())
                        .await
                } else {
                    Err("missing agent_id, card_id, or title for notify action".into())
                }
            }
            "followup" => notifier
                .handle_followup(db.cloned(), dispatch_id.clone())
                .await
                .map(|()| {
                    generic_outbox_delivery_result(
                        &dispatch_id,
                        "followup",
                        "followup handler completed",
                    )
                }),
            "status_reaction" => {
                // #750: narrow-path sync — sync_dispatch_status_reaction only
                // writes ❌ on failed/cancelled dispatches (command bot owns
                // ⏳/✅). Drains legacy rows correctly and covers repair paths
                // that bypass turn_bridge (queue/API cancel, orphan recovery).
                notifier
                    .sync_status_reaction(db.cloned(), dispatch_id.clone())
                    .await
                    .map(|()| {
                        generic_outbox_delivery_result(
                            &dispatch_id,
                            "status_reaction",
                            "status reaction sync completed",
                        )
                    })
            }
            other => {
                tracing::warn!("[dispatch-outbox] Unknown action: {other}");
                Err(format!("unknown action: {other}"))
            }
        };

        match result {
            Ok(delivery_result) => {
                let delivery_result_json = dispatch_delivery_result_json(&delivery_result);
                // Mark done + transition dispatch pending → dispatched
                mark_outbox_done_pg(pool, id, &delivery_result.status, &delivery_result_json).await;
                if action == "notify" {
                    mark_dispatch_dispatched_pg(pool, &dispatch_id).await.ok();
                }
            }
            Err(err) => {
                let new_count = retry_count + 1;
                if new_count > MAX_RETRY_COUNT {
                    // Permanent failure — exhausted all 4 retries (1m → 5m → 15m → 1h)
                    tracing::error!(
                        "[dispatch-outbox] Permanent failure for entry {id} (dispatch={dispatch_id}, action={action}): {err}"
                    );
                    let delivery_result = DispatchNotifyDeliveryResult::permanent_failure(
                        &dispatch_id,
                        &action,
                        &err,
                    );
                    let delivery_result_json = dispatch_delivery_result_json(&delivery_result);
                    mark_outbox_failed_pg(
                        pool,
                        id,
                        &err,
                        new_count,
                        &delivery_result.status,
                        &delivery_result_json,
                    )
                    .await;
                } else {
                    // Schedule retry with backoff (index = new_count - 1, since retry 1 uses BACKOFF[0])
                    let backoff_idx = (new_count - 1) as usize;
                    let backoff_secs = RETRY_BACKOFF_SECS.get(backoff_idx).copied().unwrap_or(3600);
                    tracing::warn!(
                        "[dispatch-outbox] Retry {new_count}/{MAX_RETRY_COUNT} for entry {id} (dispatch={dispatch_id}, action={action}) \
                         in {backoff_secs}s: {err}",
                    );
                    schedule_outbox_retry_pg(pool, id, &err, new_count, backoff_secs).await;
                }
            }
        }
    }
    count
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(crate) async fn process_outbox_batch_with_real_notifier(
    db: Option<&crate::db::Db>,
    pg_pool: &PgPool,
) -> usize {
    let notifier = RealOutboxNotifier::new(Arc::new(pg_pool.clone()));
    process_outbox_batch_with_pg(db, Some(pg_pool), &notifier, None).await
}

/// Worker loop that drains dispatch_outbox and executes Discord side-effects.
///
/// This is the SINGLE place where dispatch-related Discord HTTP calls originate.
/// All other code paths insert into the outbox table and return immediately.
pub(crate) async fn dispatch_outbox_loop(pg_pool: Arc<PgPool>, claim_owner: String) {
    use std::time::Duration;

    // Wait for server to be ready
    tokio::time::sleep(Duration::from_secs(3)).await;
    tracing::info!(
        claim_owner,
        "[dispatch-outbox] Worker started (adaptive backoff 500ms-5s)"
    );

    let notifier = RealOutboxNotifier::new(pg_pool);
    let mut poll_interval = Duration::from_millis(500);
    let max_interval = Duration::from_secs(5);

    loop {
        tokio::time::sleep(poll_interval).await;

        let processed = process_outbox_batch_with_pg(
            None,
            Some(notifier.pg_pool.as_ref()),
            &notifier,
            Some(&claim_owner),
        )
        .await;
        if processed == 0 {
            poll_interval = (poll_interval.mul_f64(1.5)).min(max_interval);
        } else {
            poll_interval = Duration::from_millis(500);
        }
    }
}
