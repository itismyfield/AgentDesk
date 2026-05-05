//! Dispatch outbox repository — Postgres queries about per-dispatch outbox
//! state (message target, thread binding, slot index, reaction state, review
//! followup metadata).
//!
//! #1693 introduced this submodule when splitting
//! `src/server/routes/dispatches/discord_delivery.rs` into thin handlers +
//! orchestration + repo + DTOs. The full SQL bodies still live in
//! `super` (`db/dispatches/mod.rs`) to keep the refactor mechanical and
//! preserve git blame; this module re-exports the outbox-shaped surface so
//! callers depend on a stable, narrowly-named API.
//!
//! #1694 will move additional outbox queries here (and add new ones) without
//! requiring further restructuring of the route or service layer. New SQL
//! that lives only behind the dispatch outbox should land here directly.
//!
//! Public surface (re-exported from `super`):
//! - `persist_dispatch_message_target_pg` — record (channel_id, message_id)
//!   on a dispatch's context after Discord delivery.
//! - `persist_dispatch_thread_id_pg` — record the thread the dispatch was
//!   posted to (for reuse + reaction sync).
//! - `load_dispatch_reaction_row_pg` — load (status, context) used by
//!   announce-bot reaction sync.
//! - `persist_dispatch_slot_index_pg` — bind a slot to a dispatch's context.
//! - `load_dispatch_context_pg` — read the raw JSON context blob.
//! - `latest_work_dispatch_thread_pg` — locate the most recent
//!   implementation/rework dispatch's thread for thread reuse decisions.
//! - `load_review_followup_card_pg` — load review-followup card metadata.
//! - `review_followup_already_resolved_pg` — dedup check for review followup.
//! - `latest_completed_review_provider_on_conn` (legacy-sqlite-tests only).

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(crate) use super::latest_completed_review_provider_on_conn;

// ── #1694: Outbox queue + followup repository surface ────────────────────
//
// The route layer (`server/routes/dispatches/outbox.rs`) and the queue
// service layer (`services/dispatches/outbox_queue.rs`) call into the
// helpers below for every per-row outbox/dispatch state mutation. Keeping
// the SQL here lets the route/service modules stay free of raw `sqlx::*`
// strings (route SRP audit, #1282) and concentrates all dispatch_outbox
// schema knowledge in one place.

use serde_json::Value;
use sqlx::{PgPool, Row as SqlxRow};

const DISPATCH_OUTBOX_CLAIM_STALE_SECS: i64 = 300;

/// Tuple shape returned by `claim_pending_dispatch_outbox_batch_pg`.
/// Mirrors the (id, dispatch_id, action, agent_id, card_id, title,
/// retry_count, required_capabilities) column layout of `dispatch_outbox`.
pub(crate) type DispatchOutboxRow = (
    i64,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    i64,
    Option<Value>,
);

/// Snapshot of a completed dispatch row used to build followup summaries.
#[derive(Clone, Debug)]
pub(crate) struct CompletedDispatchInfo {
    pub(crate) dispatch_type: String,
    pub(crate) status: String,
    pub(crate) card_id: String,
    pub(crate) result_json: Option<String>,
    pub(crate) context_json: Option<String>,
    pub(crate) thread_id: Option<String>,
    pub(crate) duration_seconds: Option<i64>,
}

pub(crate) fn required_capabilities_empty(required: Option<&Value>) -> bool {
    match required {
        None | Some(Value::Null) => true,
        Some(Value::Object(map)) => map.is_empty(),
        _ => false,
    }
}

pub(crate) async fn dispatch_notify_delivery_suppressed_pg(
    pool: &PgPool,
    dispatch_id: &str,
) -> Result<bool, sqlx::Error> {
    let status =
        sqlx::query_scalar::<_, Option<String>>("SELECT status FROM task_dispatches WHERE id = $1")
            .bind(dispatch_id)
            .fetch_optional(pool)
            .await?;
    Ok(matches!(
        status.flatten().as_deref(),
        Some("completed") | Some("failed") | Some("cancelled")
    ))
}

pub(crate) async fn claim_pending_dispatch_outbox_batch_pg(
    pool: &PgPool,
    claim_owner: &str,
) -> Vec<DispatchOutboxRow> {
    let owner_node =
        match crate::server::cluster::worker_node_snapshot_by_instance(pool, claim_owner, 60).await
        {
            Ok(node) => node,
            Err(error) => {
                tracing::warn!(
                    claim_owner,
                    error,
                    "[dispatch-outbox] failed to load claim owner capabilities"
                );
                None
            }
        };
    let mut tx = match pool.begin().await {
        Ok(tx) => tx,
        Err(error) => {
            tracing::warn!("[dispatch-outbox] failed to begin postgres claim transaction: {error}");
            return Vec::new();
        }
    };

    let rows = match sqlx::query(
        "SELECT
            o.id,
            o.dispatch_id,
            o.action,
            o.agent_id,
            o.card_id,
            o.title,
            o.retry_count,
            COALESCE(o.required_capabilities, td.required_capabilities) AS required_capabilities
         FROM dispatch_outbox o
         LEFT JOIN task_dispatches td ON td.id = o.dispatch_id
         WHERE (
                o.status = 'pending'
                AND (o.next_attempt_at IS NULL OR o.next_attempt_at <= NOW())
             )
            OR (
                o.status = 'processing'
                AND (
                    o.claimed_at IS NULL
                    OR o.claimed_at <= NOW() - ($1::bigint * INTERVAL '1 second')
                )
            )
         ORDER BY o.id ASC
         FOR UPDATE OF o SKIP LOCKED
         LIMIT 20",
    )
    .bind(DISPATCH_OUTBOX_CLAIM_STALE_SECS)
    .fetch_all(&mut *tx)
    .await
    {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!("[dispatch-outbox] failed to select postgres outbox rows: {error}");
            let _ = tx.rollback().await;
            return Vec::new();
        }
    };

    let mut pending = Vec::new();
    for row in rows {
        let id = match row.try_get::<i64, _>("id") {
            Ok(id) => id,
            Err(_) => continue,
        };
        let dispatch_id = match row.try_get::<String, _>("dispatch_id") {
            Ok(dispatch_id) => dispatch_id,
            Err(_) => continue,
        };
        let required_capabilities = row
            .try_get::<Option<Value>, _>("required_capabilities")
            .ok()
            .flatten();

        if !required_capabilities_empty(required_capabilities.as_ref()) {
            let decision = owner_node
                .as_ref()
                .map(|node| {
                    crate::server::cluster::explain_capability_match(
                        node,
                        required_capabilities.as_ref().expect("checked above"),
                    )
                })
                .unwrap_or_else(|| crate::server::cluster::CapabilityRouteDecision {
                    instance_id: Some(claim_owner.to_string()),
                    eligible: false,
                    reasons: vec!["claim owner is not registered in worker_nodes".to_string()],
                });
            if !decision.eligible {
                let diagnostics = serde_json::json!({
                    "claim_owner": claim_owner,
                    "decision": decision,
                    "required_capabilities": required_capabilities,
                    "checked_at": chrono::Utc::now(),
                });
                if let Err(error) = sqlx::query(
                    "UPDATE dispatch_outbox
                        SET routing_diagnostics = $2,
                            next_attempt_at = NOW() + INTERVAL '5 seconds'
                      WHERE id = $1",
                )
                .bind(id)
                .bind(&diagnostics)
                .execute(&mut *tx)
                .await
                {
                    tracing::warn!(
                        outbox_id = id,
                        dispatch_id,
                        error = %error,
                        "[dispatch-outbox] failed to record routing diagnostics"
                    );
                }
                if let Err(error) = sqlx::query(
                    "UPDATE task_dispatches
                        SET routing_diagnostics = $2,
                            updated_at = NOW()
                      WHERE id = $1",
                )
                .bind(&dispatch_id)
                .bind(&diagnostics)
                .execute(&mut *tx)
                .await
                {
                    tracing::warn!(
                        dispatch_id,
                        error = %error,
                        "[dispatch-outbox] failed to record dispatch routing diagnostics"
                    );
                }
                continue;
            }
        }

        if let Err(error) = sqlx::query(
            "UPDATE dispatch_outbox
                SET status = 'processing',
                    claimed_at = NOW(),
                    claim_owner = $2
              WHERE id = $1",
        )
        .bind(id)
        .bind(claim_owner)
        .execute(&mut *tx)
        .await
        {
            tracing::warn!(
                outbox_id = id,
                dispatch_id,
                error = %error,
                "[dispatch-outbox] failed to claim postgres outbox row"
            );
            continue;
        }

        pending.push((
            id,
            dispatch_id,
            row.try_get::<String, _>("action").ok().unwrap_or_default(),
            row.try_get::<Option<String>, _>("agent_id").ok().flatten(),
            row.try_get::<Option<String>, _>("card_id").ok().flatten(),
            row.try_get::<Option<String>, _>("title").ok().flatten(),
            row.try_get::<i64, _>("retry_count")
                .ok()
                .unwrap_or_default(),
            required_capabilities,
        ));
        if pending.len() >= 5 {
            break;
        }
    }

    if let Err(error) = tx.commit().await {
        tracing::warn!("[dispatch-outbox] failed to commit postgres outbox claims: {error}");
        return Vec::new();
    }

    pending.sort_by_key(|row| row.0);
    pending
}

pub(crate) async fn mark_dispatch_dispatched_pg(
    pool: &PgPool,
    dispatch_id: &str,
) -> Result<(), String> {
    let current = sqlx::query(
        "SELECT status, kanban_card_id, dispatch_type
           FROM task_dispatches
          WHERE id = $1",
    )
    .bind(dispatch_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load postgres dispatch {dispatch_id} for status update: {error}"))?;

    let Some(current) = current else {
        return Ok(());
    };

    let current_status = current
        .try_get::<String, _>("status")
        .map_err(|error| format!("read postgres dispatch status for {dispatch_id}: {error}"))?;
    if current_status != "pending" {
        return Ok(());
    }

    let kanban_card_id = current
        .try_get::<Option<String>, _>("kanban_card_id")
        .map_err(|error| format!("read postgres dispatch card for {dispatch_id}: {error}"))?;
    let dispatch_type = current
        .try_get::<Option<String>, _>("dispatch_type")
        .map_err(|error| format!("read postgres dispatch type for {dispatch_id}: {error}"))?;

    let changed = sqlx::query(
        "UPDATE task_dispatches
            SET status = 'dispatched',
                updated_at = NOW()
          WHERE id = $1
            AND status = 'pending'",
    )
    .bind(dispatch_id)
    .execute(pool)
    .await
    .map_err(|error| format!("update postgres dispatch {dispatch_id} to dispatched: {error}"))?
    .rows_affected();
    if changed == 0 {
        return Ok(());
    }

    sqlx::query(
        "INSERT INTO dispatch_events (
            dispatch_id,
            kanban_card_id,
            dispatch_type,
            from_status,
            to_status,
            transition_source,
            payload_json
        ) VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(dispatch_id)
    .bind(kanban_card_id)
    .bind(dispatch_type)
    .bind(Some(current_status.as_str()))
    .bind("dispatched")
    .bind("dispatch_outbox_notify")
    .bind(Option::<serde_json::Value>::None)
    .execute(pool)
    .await
    .map_err(|error| format!("insert postgres dispatch event for {dispatch_id}: {error}"))?;

    sqlx::query(
        "INSERT INTO dispatch_outbox (dispatch_id, action)
         SELECT $1, 'status_reaction'
          WHERE NOT EXISTS (
              SELECT 1
                FROM dispatch_outbox
               WHERE dispatch_id = $1
                 AND action = 'status_reaction'
                 AND status IN ('pending', 'processing')
          )",
    )
    .bind(dispatch_id)
    .execute(pool)
    .await
    .map_err(|error| format!("enqueue postgres status_reaction for {dispatch_id}: {error}"))?;

    Ok(())
}

/// Mark an outbox row as `done` and clear claim state. Used both by the
/// notify-suppressed early-exit and by the success branch of the worker.
pub(crate) async fn mark_outbox_done_pg(
    pool: &PgPool,
    outbox_id: i64,
    delivery_status: &str,
    delivery_result_json: &str,
) {
    sqlx::query(
        "UPDATE dispatch_outbox
            SET status = 'done',
                processed_at = NOW(),
                error = NULL,
                delivery_status = $2,
                delivery_result = $3::jsonb,
                claimed_at = NULL,
                claim_owner = NULL
          WHERE id = $1",
    )
    .bind(outbox_id)
    .bind(delivery_status)
    .bind(delivery_result_json)
    .execute(pool)
    .await
    .ok();
}

/// Mark an outbox row as permanently `failed` after the retry budget is
/// exhausted.
pub(crate) async fn mark_outbox_failed_pg(
    pool: &PgPool,
    outbox_id: i64,
    error_message: &str,
    new_count: i64,
    delivery_status: &str,
    delivery_result_json: &str,
) {
    sqlx::query(
        "UPDATE dispatch_outbox
            SET status = 'failed',
                error = $1,
                retry_count = $2,
                processed_at = NOW(),
                delivery_status = $4,
                delivery_result = $5::jsonb,
                claimed_at = NULL,
                claim_owner = NULL
          WHERE id = $3",
    )
    .bind(error_message)
    .bind(new_count)
    .bind(outbox_id)
    .bind(delivery_status)
    .bind(delivery_result_json)
    .execute(pool)
    .await
    .ok();
}

/// Reschedule an outbox row for retry with `backoff_secs` delay.
pub(crate) async fn schedule_outbox_retry_pg(
    pool: &PgPool,
    outbox_id: i64,
    error_message: &str,
    new_count: i64,
    backoff_secs: i64,
) {
    sqlx::query(
        "UPDATE dispatch_outbox
            SET status = 'pending',
                error = $1,
                retry_count = $2,
                next_attempt_at = NOW() + ($3::bigint * INTERVAL '1 second'),
                claimed_at = NULL,
                claim_owner = NULL
          WHERE id = $4",
    )
    .bind(error_message)
    .bind(new_count)
    .bind(backoff_secs)
    .bind(outbox_id)
    .execute(pool)
    .await
    .ok();
}

pub(crate) async fn load_completed_dispatch_info_pg(
    pool: &PgPool,
    dispatch_id: &str,
) -> Result<Option<CompletedDispatchInfo>, String> {
    let row = sqlx::query(
        "SELECT td.dispatch_type,
                td.status,
                kc.id AS card_id,
                td.result,
                td.context,
                td.thread_id,
                CAST(
                    EXTRACT(
                        EPOCH FROM (
                            COALESCE(td.completed_at, td.updated_at, td.created_at) - td.created_at
                        )
                    ) AS BIGINT
                ) AS duration_seconds
         FROM task_dispatches td
         JOIN kanban_cards kc ON kc.id = td.kanban_card_id
         WHERE td.id = $1",
    )
    .bind(dispatch_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load dispatch {dispatch_id} followup info from postgres: {error}"))?;

    row.map(|row| {
        Ok(CompletedDispatchInfo {
            dispatch_type: row.try_get("dispatch_type").map_err(|error| {
                format!("read postgres dispatch_type for {dispatch_id}: {error}")
            })?,
            status: row
                .try_get("status")
                .map_err(|error| format!("read postgres status for {dispatch_id}: {error}"))?,
            card_id: row
                .try_get("card_id")
                .map_err(|error| format!("read postgres card_id for {dispatch_id}: {error}"))?,
            result_json: row
                .try_get("result")
                .map_err(|error| format!("read postgres result for {dispatch_id}: {error}"))?,
            context_json: row
                .try_get("context")
                .map_err(|error| format!("read postgres context for {dispatch_id}: {error}"))?,
            thread_id: row
                .try_get("thread_id")
                .map_err(|error| format!("read postgres thread_id for {dispatch_id}: {error}"))?,
            duration_seconds: row.try_get("duration_seconds").map_err(|error| {
                format!("read postgres duration_seconds for {dispatch_id}: {error}")
            })?,
        })
    })
    .transpose()
}

pub(crate) async fn load_card_status_pg(
    pool: &PgPool,
    card_id: &str,
) -> Result<Option<String>, String> {
    let row = sqlx::query("SELECT status FROM kanban_cards WHERE id = $1")
        .bind(card_id)
        .fetch_optional(pool)
        .await
        .map_err(|error| format!("load postgres card status for {card_id}: {error}"))?;
    row.map(|row| {
        row.try_get("status")
            .map_err(|error| format!("read postgres card status for {card_id}: {error}"))
    })
    .transpose()
}

pub(crate) async fn clear_all_dispatch_threads_pg(
    pool: &PgPool,
    card_id: &str,
) -> Result<(), String> {
    sqlx::query(
        "UPDATE kanban_cards
         SET channel_thread_map = NULL,
             active_thread_id = NULL
         WHERE id = $1",
    )
    .bind(card_id)
    .execute(pool)
    .await
    .map_err(|error| format!("clear postgres thread mappings for {card_id}: {error}"))?;
    Ok(())
}

/// Re-arm (or insert, if missing) the `notify` outbox row for `dispatch_id`.
/// Returns `Ok(true)` when an outbox row ends up in a fresh `pending` state,
/// `Ok(false)` when the dispatch is already terminal or no row exists to
/// rearm.
pub(crate) async fn requeue_dispatch_notify_pg(
    pool: &PgPool,
    dispatch_id: &str,
) -> Result<bool, String> {
    let dispatch = sqlx::query(
        "SELECT status, to_agent_id, kanban_card_id, title
           FROM task_dispatches
          WHERE id = $1",
    )
    .bind(dispatch_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load postgres dispatch {dispatch_id} for notify requeue: {error}"))?;

    let Some(dispatch) = dispatch else {
        return Ok(false);
    };

    let status = dispatch
        .try_get::<String, _>("status")
        .map_err(|error| format!("read postgres dispatch status for {dispatch_id}: {error}"))?;
    if matches!(status.as_str(), "completed" | "failed" | "cancelled") {
        return Ok(false);
    }

    let agent_id = dispatch
        .try_get::<Option<String>, _>("to_agent_id")
        .map_err(|error| format!("read postgres dispatch agent for {dispatch_id}: {error}"))?
        .ok_or_else(|| format!("postgres dispatch {dispatch_id} missing to_agent_id"))?;
    let card_id = dispatch
        .try_get::<Option<String>, _>("kanban_card_id")
        .map_err(|error| format!("read postgres dispatch card for {dispatch_id}: {error}"))?
        .ok_or_else(|| format!("postgres dispatch {dispatch_id} missing kanban_card_id"))?;
    let title = dispatch
        .try_get::<Option<String>, _>("title")
        .map_err(|error| format!("read postgres dispatch title for {dispatch_id}: {error}"))?
        .ok_or_else(|| format!("postgres dispatch {dispatch_id} missing title"))?;

    let updated = sqlx::query(
        "UPDATE dispatch_outbox
            SET agent_id = $2,
                card_id = $3,
                title = $4,
                status = 'pending',
                retry_count = 0,
                next_attempt_at = NULL,
                processed_at = NULL,
                error = NULL,
                delivery_status = NULL,
                delivery_result = NULL,
                claimed_at = NULL,
                claim_owner = NULL
          WHERE dispatch_id = $1
            AND action = 'notify'",
    )
    .bind(dispatch_id)
    .bind(&agent_id)
    .bind(&card_id)
    .bind(&title)
    .execute(pool)
    .await
    .map_err(|error| format!("reset postgres notify outbox for {dispatch_id}: {error}"))?
    .rows_affected();
    if updated > 0 {
        return Ok(true);
    }

    let inserted = sqlx::query(
        "INSERT INTO dispatch_outbox (
            dispatch_id, action, agent_id, card_id, title, status, retry_count
         ) VALUES ($1, 'notify', $2, $3, $4, 'pending', 0)
         ON CONFLICT DO NOTHING",
    )
    .bind(dispatch_id)
    .bind(&agent_id)
    .bind(&card_id)
    .bind(&title)
    .execute(pool)
    .await
    .map_err(|error| format!("insert postgres notify outbox for {dispatch_id}: {error}"))?
    .rows_affected();
    if inserted > 0 {
        return Ok(true);
    }

    let rearmed = sqlx::query(
        "UPDATE dispatch_outbox
            SET agent_id = $2,
                card_id = $3,
                title = $4,
                status = 'pending',
                retry_count = 0,
                next_attempt_at = NULL,
                processed_at = NULL,
                error = NULL,
                delivery_status = NULL,
                delivery_result = NULL,
                claimed_at = NULL,
                claim_owner = NULL
          WHERE dispatch_id = $1
            AND action = 'notify'",
    )
    .bind(dispatch_id)
    .bind(&agent_id)
    .bind(&card_id)
    .bind(&title)
    .execute(pool)
    .await
    .map_err(|error| format!("rearm postgres notify outbox for {dispatch_id}: {error}"))?
    .rows_affected();
    Ok(rearmed > 0)
}
