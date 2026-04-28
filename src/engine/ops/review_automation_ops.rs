//! `agentdesk.reviewAutomation.*` bridge ops for the #743 create-pr lifecycle
//! redesign.
//!
//! Three ops form the atomic primitives:
//! - `handoffCreatePr(cardId, payload)` — fresh review-pass handoff.
//!   Idempotent-reuses an existing active create-pr dispatch if any, or seeds
//!   pr_tracking + inserts a stamped dispatch + sets blocked_reason in a single
//!   transaction.
//! - `recordPrCreateFailure(cardId, error, stampGen)` — JS-orchestrated thin
//!   helper. Stale-guards via stampGen, increments retry_count (or inserts a
//!   fresh row with retry_count=1 if the tx that seeded it rolled back),
//!   flips state='escalated' at 3 retries. Does NOT terminalize or set
//!   blocked_reason — the JS caller owns that outer orchestration.
//! - `reseedPrTracking(cardId)` — cancels any active create-pr dispatch (to
//!   avoid the dedupe/unique-index deadlock) and mints a fresh generation,
//!   resetting retry_count and last_error.

use crate::db::Db;
use crate::dispatch::{DispatchCreateOptions, apply_dispatch_attached_intents_on_pg_tx};
use rquickjs::{Ctx, Function, Object, Result as JsResult};
use serde::Deserialize;
use serde_json::json;
use sqlx::{PgPool, Row};
use uuid::Uuid;

pub(super) fn register_review_automation_ops<'js>(
    ctx: &Ctx<'js>,
    _db: Option<Db>,
    pg_pool: Option<PgPool>,
) -> JsResult<()> {
    let ad: Object<'js> = ctx.globals().get("agentdesk")?;
    let obj = Object::new(ctx.clone())?;

    let pg_handoff = pg_pool.clone();
    obj.set(
        "__handoffCreatePrRaw",
        Function::new(ctx.clone(), move |payload_json: String| -> String {
            handoff_create_pr_raw(pg_handoff.as_ref(), &payload_json)
        })?,
    )?;

    let pg_record = pg_pool.clone();
    obj.set(
        "__recordPrCreateFailureRaw",
        Function::new(
            ctx.clone(),
            move |card_id: String, error: String, stamp_gen: String| -> String {
                record_pr_create_failure_raw(pg_record.as_ref(), &card_id, &error, &stamp_gen)
            },
        )?,
    )?;

    let pg_reseed = pg_pool.clone();
    obj.set(
        "__reseedPrTrackingRaw",
        Function::new(ctx.clone(), move |card_id: String| -> String {
            reseed_pr_tracking_raw(pg_reseed.as_ref(), &card_id)
        })?,
    )?;

    ad.set("reviewAutomation", obj)?;

    ctx.eval::<(), _>(
        r#"
        (function() {
            agentdesk.reviewAutomation.handoffCreatePr = function(cardId, payload) {
                var merged = Object.assign({card_id: cardId}, payload || {});
                var result = JSON.parse(
                    agentdesk.reviewAutomation.__handoffCreatePrRaw(JSON.stringify(merged))
                );
                if (result.error) throw new Error(result.error);
                return result;
            };
            agentdesk.reviewAutomation.recordPrCreateFailure = function(cardId, error, stampGen) {
                var result = JSON.parse(
                    agentdesk.reviewAutomation.__recordPrCreateFailureRaw(
                        cardId || "",
                        String(error || ""),
                        stampGen || ""
                    )
                );
                if (result.error) throw new Error(result.error);
                return result;
            };
            agentdesk.reviewAutomation.reseedPrTracking = function(cardId) {
                var result = JSON.parse(
                    agentdesk.reviewAutomation.__reseedPrTrackingRaw(cardId || "")
                );
                if (result.error) throw new Error(result.error);
                return result;
            };
        })();
        "#,
    )?;

    Ok(())
}

// ── handoffCreatePr ─────────────────────────────────────────────────────

#[derive(Clone, Debug, Deserialize)]
struct HandoffPayload {
    card_id: String,
    repo_id: String,
    worktree_path: Option<String>,
    branch: String,
    head_sha: Option<String>,
    agent_id: String,
    title: String,
}

fn handoff_create_pr_raw(pg_pool: Option<&PgPool>, payload_json: &str) -> String {
    let payload: HandoffPayload = match serde_json::from_str(payload_json) {
        Ok(p) => p,
        Err(e) => return json!({"error": format!("invalid payload: {e}")}).to_string(),
    };
    if payload.card_id.trim().is_empty() {
        return json!({"error": "card_id is required"}).to_string();
    }
    if payload.agent_id.trim().is_empty() {
        return json!({"error": "agent_id is required"}).to_string();
    }
    if payload.branch.trim().is_empty() {
        return json!({"error": "branch is required"}).to_string();
    }
    let Some(pool) = pg_pool else {
        return json!({"error": "postgres backend is required for reviewAutomation.handoffCreatePr"}).to_string();
    };
    let result = crate::utils::async_bridge::block_on_pg_result(
        pool,
        {
            let payload = payload.clone();
            move |bridge_pool| async move { handoff_create_pr_pg(&bridge_pool, &payload).await }
        },
        |error| error,
    );
    match result {
        Ok(v) => v.to_string(),
        Err(e) => json!({"error": e}).to_string(),
    }
}

async fn seed_pg_pr_tracking_handoff_state(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    payload: &HandoffPayload,
    generation: &str,
    current_round: i64,
) -> Result<(), String> {
    sqlx::query(
        "INSERT INTO pr_tracking (
            card_id,
            repo_id,
            worktree_path,
            branch,
            head_sha,
            state,
            last_error,
            dispatch_generation,
            review_round,
            retry_count,
            created_at,
            updated_at
         ) VALUES (
            $1, $2, $3, $4, $5, 'create-pr', NULL, $6, $7, 0, NOW(), NOW()
         )
         ON CONFLICT (card_id) DO UPDATE
         SET repo_id = EXCLUDED.repo_id,
             worktree_path = EXCLUDED.worktree_path,
             branch = EXCLUDED.branch,
             head_sha = EXCLUDED.head_sha,
             state = 'create-pr',
             last_error = NULL,
             dispatch_generation = EXCLUDED.dispatch_generation,
             review_round = EXCLUDED.review_round,
             retry_count = 0,
             updated_at = NOW()",
    )
    .bind(&payload.card_id)
    .bind(&payload.repo_id)
    .bind(payload.worktree_path.as_deref())
    .bind(&payload.branch)
    .bind(payload.head_sha.as_deref())
    .bind(generation)
    .bind(current_round)
    .execute(&mut **tx)
    .await
    .map_err(|e| format!("upsert postgres pr_tracking for {}: {e}", payload.card_id))?;

    Ok(())
}

async fn refresh_pg_pr_tracking_reuse_state_if_active(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    payload: &HandoffPayload,
    dispatch_id: &str,
    generation: &str,
    current_round: i64,
) -> Result<bool, String> {
    let refreshed = sqlx::query(
        "WITH active_dispatch AS (
             SELECT id
             FROM task_dispatches
             WHERE id = $8
               AND status IN ('pending', 'dispatched')
             FOR UPDATE
         ),
         upsert_tracking AS (
             INSERT INTO pr_tracking (
                 card_id,
                 repo_id,
                 worktree_path,
                 branch,
                 head_sha,
                 state,
                 last_error,
                 dispatch_generation,
                 review_round,
                 retry_count,
                 created_at,
                 updated_at
             )
             SELECT
                 $1,
                 $2,
                 $3,
                 $4,
                 $5,
                 'create-pr',
                 NULL,
                 $6,
                 $7,
                 0,
                 NOW(),
                 NOW()
             FROM active_dispatch
             ON CONFLICT (card_id) DO UPDATE
             SET state = 'create-pr',
                 last_error = NULL,
                 dispatch_generation = EXCLUDED.dispatch_generation,
                 review_round = EXCLUDED.review_round,
                 retry_count = 0,
                 updated_at = NOW()
             RETURNING 1
         ),
         update_card AS (
             UPDATE kanban_cards
             SET blocked_reason = 'pr:creating',
                 updated_at = NOW()
             WHERE id = $1
               AND EXISTS (SELECT 1 FROM active_dispatch)
             RETURNING 1
         )
         SELECT
             EXISTS (SELECT 1 FROM active_dispatch) AS dispatch_active,
             EXISTS (SELECT 1 FROM upsert_tracking) AS tracking_updated,
             EXISTS (SELECT 1 FROM update_card) AS card_updated",
    )
    .bind(&payload.card_id)
    .bind(&payload.repo_id)
    .bind(payload.worktree_path.as_deref())
    .bind(&payload.branch)
    .bind(payload.head_sha.as_deref())
    .bind(generation)
    .bind(current_round)
    .bind(dispatch_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(|e| {
        format!(
            "refresh postgres reused pr_tracking for {}: {e}",
            payload.card_id
        )
    })?;

    let dispatch_active = refreshed
        .try_get::<bool, _>("dispatch_active")
        .map_err(|e| {
            format!(
                "decode postgres reuse dispatch_active for {}: {e}",
                payload.card_id
            )
        })?;
    let tracking_updated = refreshed
        .try_get::<bool, _>("tracking_updated")
        .map_err(|e| {
            format!(
                "decode postgres reuse tracking_updated for {}: {e}",
                payload.card_id
            )
        })?;
    let card_updated = refreshed.try_get::<bool, _>("card_updated").map_err(|e| {
        format!(
            "decode postgres reuse card_updated for {}: {e}",
            payload.card_id
        )
    })?;

    Ok(dispatch_active && tracking_updated && card_updated)
}

async fn load_pg_dispatch_status(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    dispatch_id: &str,
) -> Result<Option<String>, String> {
    sqlx::query_scalar::<_, String>(
        "SELECT status
         FROM task_dispatches
         WHERE id = $1",
    )
    .bind(dispatch_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|e| format!("load postgres dispatch status for {dispatch_id}: {e}"))
}

async fn handoff_create_pr_pg(
    pool: &PgPool,
    payload: &HandoffPayload,
) -> Result<serde_json::Value, String> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| format!("begin postgres review automation transaction: {e}"))?;

    let current_round = sqlx::query_scalar::<_, i64>(
        "SELECT COALESCE(review_round, 0)::BIGINT
         FROM card_review_state
         WHERE card_id = $1",
    )
    .bind(&payload.card_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| format!("load postgres review_round for {}: {e}", payload.card_id))?
    .unwrap_or(0);

    let existing = sqlx::query(
        "SELECT td.id,
                COALESCE(
                    NULLIF(
                        substring(
                            COALESCE(td.context, '')
                            FROM '\"dispatch_generation\"\\s*:\\s*\"([^\"]+)\"'
                        ),
                        ''
                    ),
                    pt.dispatch_generation,
                    ''
                ) AS dispatch_generation
         FROM task_dispatches td
         LEFT JOIN pr_tracking pt
                ON pt.card_id = td.kanban_card_id
         WHERE td.kanban_card_id = $1
           AND td.dispatch_type = 'create-pr'
           AND td.status IN ('pending', 'dispatched')
         ORDER BY td.created_at DESC
         LIMIT 1",
    )
    .bind(&payload.card_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| {
        format!(
            "lookup active postgres create-pr dispatch for {}: {e}",
            payload.card_id
        )
    })?;
    if let Some(existing) = existing {
        let dispatch_id = existing
            .try_get::<String, _>("id")
            .map_err(|e| format!("decode active postgres create-pr dispatch id: {e}"))?;
        let generation = existing
            .try_get::<String, _>("dispatch_generation")
            .map_err(|e| format!("decode active postgres create-pr generation: {e}"))?;
        if refresh_pg_pr_tracking_reuse_state_if_active(
            &mut tx,
            payload,
            &dispatch_id,
            &generation,
            current_round,
        )
        .await?
        {
            tx.commit().await.map_err(|e| {
                format!(
                    "commit postgres create-pr reuse for {}: {e}",
                    payload.card_id
                )
            })?;
            return Ok(json!({
                "ok": true,
                "reused": true,
                "dispatch_id": dispatch_id,
                "generation": generation,
            }));
        }

        match load_pg_dispatch_status(&mut tx, &dispatch_id)
            .await?
            .as_deref()
        {
            Some("completed") => {
                tx.commit().await.map_err(|e| {
                    format!(
                        "commit postgres create-pr completed reuse for {}: {e}",
                        payload.card_id
                    )
                })?;
                return Ok(json!({
                    "ok": true,
                    "reused": true,
                    "dispatch_id": dispatch_id,
                    "generation": generation,
                }));
            }
            _ => {
                // The candidate dispatch stopped being active before we refreshed
                // pr_tracking/blocked_reason. Fall through to the fresh handoff path
                // instead of rewinding terminal or failed state.
            }
        }
    }

    let (old_status, card_repo_id, card_agent_id) =
        sqlx::query_as::<_, (String, Option<String>, Option<String>)>(
            "SELECT status, repo_id, assigned_agent_id
             FROM kanban_cards
             WHERE id = $1",
        )
        .bind(&payload.card_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| {
            format!(
                "load postgres card {} for create-pr handoff: {e}",
                payload.card_id
            )
        })?
        .ok_or_else(|| format!("card {} not found", payload.card_id))?;

    crate::pipeline::ensure_loaded();
    let effective = crate::pipeline::resolve_for_card_pg(
        pool,
        card_repo_id.as_deref(),
        card_agent_id.as_deref(),
    )
    .await;

    let generation = Uuid::new_v4().to_string();
    let dispatch_id = Uuid::new_v4().to_string();

    seed_pg_pr_tracking_handoff_state(&mut tx, payload, &generation, current_round).await?;

    let context = json!({
        "dispatch_generation": generation,
        "review_round_at_dispatch": current_round,
        "sidecar_dispatch": true,
        "worktree_path": payload.worktree_path,
        "worktree_branch": payload.branch,
        "branch": payload.branch,
    });
    let context_str = serde_json::to_string(&context).map_err(|e| {
        format!(
            "encode create-pr dispatch context for {}: {e}",
            payload.card_id
        )
    })?;

    match apply_dispatch_attached_intents_on_pg_tx(
        &mut tx,
        &payload.card_id,
        &payload.agent_id,
        &dispatch_id,
        "create-pr",
        false,
        &old_status,
        &effective,
        &payload.title,
        &context_str,
        None,
        0,
        DispatchCreateOptions {
            sidecar_dispatch: true,
            ..Default::default()
        },
    )
    .await
    {
        Ok(()) => {}
        Err(error)
            if error
                .to_string()
                .contains("concurrent race prevented by DB constraint") =>
        {
            tx.rollback().await.ok();
            return Err(error.to_string());
        }
        Err(error) => {
            return Err(format!(
                "attach postgres create-pr dispatch {} for {}: {error}",
                dispatch_id, payload.card_id
            ));
        }
    }

    sqlx::query(
        "UPDATE kanban_cards
         SET blocked_reason = 'pr:creating',
             updated_at = NOW()
         WHERE id = $1",
    )
    .bind(&payload.card_id)
    .execute(&mut *tx)
    .await
    .map_err(|e| format!("set postgres blocked_reason for {}: {e}", payload.card_id))?;

    tx.commit().await.map_err(|e| {
        format!(
            "commit postgres create-pr handoff for {}: {e}",
            payload.card_id
        )
    })?;

    Ok(json!({
        "ok": true,
        "reused": false,
        "dispatch_id": dispatch_id,
        "generation": generation,
    }))
}

// ── recordPrCreateFailure ──────────────────────────────────────────────

fn record_pr_create_failure_raw(
    pg_pool: Option<&PgPool>,
    card_id: &str,
    error: &str,
    stamp_gen: &str,
) -> String {
    if card_id.trim().is_empty() {
        return json!({"error": "card_id is required"}).to_string();
    }
    let Some(pool) = pg_pool else {
        return json!({"error": "postgres backend is required for reviewAutomation.recordPrCreateFailure"}).to_string();
    };
    let card_id = card_id.to_string();
    let error = error.to_string();
    let stamp_gen = stamp_gen.to_string();
    let result = crate::utils::async_bridge::block_on_pg_result(
        pool,
        move |bridge_pool| async move {
            record_pr_create_failure_pg(&bridge_pool, &card_id, &error, &stamp_gen).await
        },
        |runtime_error| runtime_error,
    );
    match result {
        Ok(v) => v.to_string(),
        Err(e) => json!({"error": e}).to_string(),
    }
}

async fn record_pr_create_failure_pg(
    pool: &PgPool,
    card_id: &str,
    error: &str,
    stamp_gen: &str,
) -> Result<serde_json::Value, String> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| format!("begin postgres pr_tracking failure transaction: {e}"))?;

    if !stamp_gen.is_empty() {
        let current_gen = sqlx::query_scalar::<_, String>(
            "SELECT dispatch_generation
             FROM pr_tracking
             WHERE card_id = $1",
        )
        .bind(card_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| format!("load postgres pr_tracking generation for {card_id}: {e}"))?;
        if let Some(cur) = current_gen.as_deref()
            && !cur.is_empty()
            && cur != stamp_gen
        {
            tx.rollback().await.ok();
            return Ok(json!({
                "ok": true,
                "noop": true,
                "reason": "stale_generation",
            }));
        }
    }

    let updated = sqlx::query(
        "UPDATE pr_tracking
         SET retry_count = retry_count + 1,
             last_error = $1,
             updated_at = NOW()
         WHERE card_id = $2",
    )
    .bind(error)
    .bind(card_id)
    .execute(&mut *tx)
    .await
    .map_err(|e| format!("update postgres pr_tracking failure for {card_id}: {e}"))?
    .rows_affected();
    if updated == 0 {
        sqlx::query(
            "INSERT INTO pr_tracking (
                card_id,
                state,
                last_error,
                dispatch_generation,
                review_round,
                retry_count,
                created_at,
                updated_at
             ) VALUES (
                $1, 'create-pr', $2, '', 0, 1, NOW(), NOW()
             )
             ON CONFLICT (card_id) DO UPDATE
             SET retry_count = pr_tracking.retry_count + 1,
                 last_error = EXCLUDED.last_error,
                 updated_at = NOW()",
        )
        .bind(card_id)
        .bind(error)
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("insert postgres pr_tracking failure row for {card_id}: {e}"))?;
    }

    let retry_count = sqlx::query_scalar::<_, i64>(
        "SELECT retry_count::BIGINT
         FROM pr_tracking
         WHERE card_id = $1",
    )
    .bind(card_id)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| format!("load postgres retry_count for {card_id}: {e}"))?;
    let escalated = retry_count >= 3;
    if escalated {
        sqlx::query(
            "UPDATE pr_tracking
             SET state = 'escalated',
                 updated_at = NOW()
             WHERE card_id = $1",
        )
        .bind(card_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("escalate postgres pr_tracking for {card_id}: {e}"))?;
    }

    tx.commit()
        .await
        .map_err(|e| format!("commit postgres pr_tracking failure for {card_id}: {e}"))?;

    Ok(json!({
        "ok": true,
        "retry_count": retry_count,
        "escalated": escalated,
    }))
}

// ── reseedPrTracking ──────────────────────────────────────────────────

fn reseed_pr_tracking_raw(pg_pool: Option<&PgPool>, card_id: &str) -> String {
    if card_id.trim().is_empty() {
        return json!({"error": "card_id is required"}).to_string();
    }
    let Some(pool) = pg_pool else {
        return json!({"error": "postgres backend is required for reviewAutomation.reseedPrTracking"}).to_string();
    };
    let card_id = card_id.to_string();
    let result = crate::utils::async_bridge::block_on_pg_result(
        pool,
        move |bridge_pool| async move { reseed_pr_tracking_pg(&bridge_pool, &card_id).await },
        |runtime_error| runtime_error,
    );
    match result {
        Ok(v) => v.to_string(),
        Err(e) => json!({"error": e}).to_string(),
    }
}

async fn reseed_pr_tracking_pg(pool: &PgPool, card_id: &str) -> Result<serde_json::Value, String> {
    // Run cancel/reset + pr_tracking generation/head/review_round update in a
    // single transaction so a crash mid-flight cannot leave the dispatch
    // cancelled while pr_tracking still points at the previous generation.
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| format!("begin postgres pr_tracking reseed transaction: {e}"))?;

    let active_id = sqlx::query_scalar::<_, String>(
        "SELECT id
         FROM task_dispatches
         WHERE kanban_card_id = $1
           AND dispatch_type = 'create-pr'
           AND status IN ('pending', 'dispatched')
         ORDER BY created_at DESC
         LIMIT 1",
    )
    .bind(card_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| format!("load active postgres create-pr dispatch for {card_id}: {e}"))?;
    if let Some(dispatch_id) = active_id {
        let _ = crate::dispatch::cancel_dispatch_and_reset_auto_queue_on_pg_tx(
            &mut tx,
            &dispatch_id,
            Some("superseded_by_reseed"),
        )
        .await
        .map_err(|e| format!("cancel active postgres create-pr dispatch {dispatch_id}: {e}"))?;
    }

    let latest_head = sqlx::query_scalar::<_, String>(
        "SELECT COALESCE(
            json_extract(td.result, '$.head_sha'),
            json_extract(td.result, '$.completed_commit'),
            json_extract(td.result, '$.reviewed_commit'),
            json_extract(td.context, '$.completed_commit'),
            json_extract(td.context, '$.reviewed_commit')
         )
         FROM task_dispatches td
         WHERE td.kanban_card_id = $1
           AND td.status = 'completed'
           AND td.dispatch_type IN ('implementation', 'rework')
         ORDER BY td.completed_at DESC
         LIMIT 1",
    )
    .bind(card_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| format!("load latest postgres completed work target for {card_id}: {e}"))?;

    let current_round = sqlx::query_scalar::<_, i64>(
        "SELECT COALESCE(review_round, 0)::BIGINT
         FROM card_review_state
         WHERE card_id = $1",
    )
    .bind(card_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| format!("load postgres review_round for {card_id}: {e}"))?
    .unwrap_or(0);

    let generation = Uuid::new_v4().to_string();

    let updated = sqlx::query(
        "UPDATE pr_tracking
         SET dispatch_generation = $1,
             review_round = $2,
             head_sha = COALESCE($3, head_sha),
             state = 'create-pr',
             retry_count = 0,
             last_error = NULL,
             updated_at = NOW()
         WHERE card_id = $4",
    )
    .bind(&generation)
    .bind(current_round)
    .bind(latest_head.as_deref())
    .bind(card_id)
    .execute(&mut *tx)
    .await
    .map_err(|e| format!("update postgres reseeded pr_tracking for {card_id}: {e}"))?
    .rows_affected();
    if updated == 0 {
        sqlx::query(
            "INSERT INTO pr_tracking (
                card_id,
                head_sha,
                state,
                dispatch_generation,
                review_round,
                retry_count,
                created_at,
                updated_at
             ) VALUES (
                $1, $2, 'create-pr', $3, $4, 0, NOW(), NOW()
             )",
        )
        .bind(card_id)
        .bind(latest_head.as_deref())
        .bind(&generation)
        .bind(current_round)
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("insert postgres reseeded pr_tracking for {card_id}: {e}"))?;
    }

    tx.commit()
        .await
        .map_err(|e| format!("commit postgres pr_tracking reseed for {card_id}: {e}"))?;

    Ok(json!({
        "ok": true,
        "generation": generation,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const REVIEW_AUTOMATION_PG_TEST_LABEL: &str = "review automation pg tests";

    struct TestDatabase {
        _lock: crate::db::postgres::PostgresTestLifecycleGuard,
        admin_url: String,
        database_name: String,
        database_url: String,
        cleanup_armed: bool,
    }

    impl TestDatabase {
        async fn create() -> Self {
            let lock = crate::db::postgres::lock_test_lifecycle();
            let admin_url = admin_database_url();
            let database_name = format!("agentdesk_review_auto_{}", uuid::Uuid::new_v4().simple());
            let database_url = format!("{}/{}", base_database_url(), database_name);
            crate::db::postgres::create_test_database(
                &admin_url,
                &database_name,
                REVIEW_AUTOMATION_PG_TEST_LABEL,
            )
            .await
            .expect("create postgres test db");

            Self {
                _lock: lock,
                admin_url,
                database_name,
                database_url,
                cleanup_armed: true,
            }
        }

        async fn migrate(&self) -> sqlx::PgPool {
            crate::db::postgres::connect_test_pool_and_migrate(
                &self.database_url,
                REVIEW_AUTOMATION_PG_TEST_LABEL,
            )
            .await
            .expect("migrate postgres test db")
        }

        async fn drop(mut self) {
            let drop_result = crate::db::postgres::drop_test_database(
                &self.admin_url,
                &self.database_name,
                REVIEW_AUTOMATION_PG_TEST_LABEL,
            )
            .await;
            if drop_result.is_ok() {
                self.cleanup_armed = false;
            }
            drop_result.expect("drop postgres test db");
        }
    }

    impl Drop for TestDatabase {
        fn drop(&mut self) {
            if !self.cleanup_armed {
                return;
            }

            cleanup_test_database_from_drop(
                self.admin_url.clone(),
                self.database_name.clone(),
                REVIEW_AUTOMATION_PG_TEST_LABEL,
            );
        }
    }

    fn cleanup_test_database_from_drop(
        admin_url: String,
        database_name: String,
        label: &'static str,
    ) {
        let cleanup_database_name = database_name.clone();
        let thread_name = format!("{label} cleanup {cleanup_database_name}");
        let spawn_result = std::thread::Builder::new()
            .name(thread_name)
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        eprintln!("{label} cleanup runtime failed for {database_name}: {error}");
                        return;
                    }
                };

                if let Err(error) = runtime.block_on(crate::db::postgres::drop_test_database(
                    &admin_url,
                    &database_name,
                    label,
                )) {
                    eprintln!("{label} cleanup failed for {database_name}: {error}");
                }
            });

        match spawn_result {
            Ok(handle) => {
                if handle.join().is_err() {
                    eprintln!("{label} cleanup thread panicked for {cleanup_database_name}");
                }
            }
            Err(error) => {
                eprintln!(
                    "{label} cleanup thread spawn failed for {cleanup_database_name}: {error}"
                );
            }
        }
    }

    fn base_database_url() -> String {
        if let Ok(base) = std::env::var("POSTGRES_TEST_DATABASE_URL_BASE") {
            let trimmed = base.trim();
            if !trimmed.is_empty() {
                return trimmed.trim_end_matches('/').to_string();
            }
        }

        let user = std::env::var("PGUSER")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| {
                std::env::var("USER")
                    .ok()
                    .filter(|value| !value.trim().is_empty())
            })
            .unwrap_or_else(|| "postgres".to_string());
        let password = std::env::var("PGPASSWORD")
            .ok()
            .filter(|value| !value.trim().is_empty());
        let host = std::env::var("PGHOST")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "localhost".to_string());
        let port = std::env::var("PGPORT")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "5432".to_string());

        match password {
            Some(password) => format!("postgresql://{user}:{password}@{host}:{port}"),
            None => format!("postgresql://{user}@{host}:{port}"),
        }
    }

    fn admin_database_url() -> String {
        let admin_db = std::env::var("POSTGRES_TEST_ADMIN_DB")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "postgres".to_string());
        format!("{}/{}", base_database_url(), admin_db)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn review_automation_pg_repeated_test_database_lifecycle_releases_admin_pool() {
        for _ in 0..4 {
            let test_db = TestDatabase::create().await;
            let pool = test_db.migrate().await;

            // #1019: `SELECT 1` returns INT4 in postgres by default, so
            // decode as i32. The value only exists to probe that the pool
            // is alive after migration.
            let one: i32 = sqlx::query_scalar("SELECT 1")
                .fetch_one(&pool)
                .await
                .expect("test postgres pool should answer after migration");
            assert_eq!(one, 1);

            pool.close().await;
            test_db.drop().await;
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn review_automation_pg_handoff_reuses_dispatch_with_malformed_context() {
        let test_db = TestDatabase::create().await;
        let pool = test_db.migrate().await;

        sqlx::query(
            "INSERT INTO agents (id, name, provider, status)
             VALUES ($1, $2, 'codex', 'idle')",
        )
        .bind("agent-reviewer")
        .bind("Reviewer Agent")
        .execute(&pool)
        .await
        .expect("insert reviewer agent");

        sqlx::query(
            "INSERT INTO kanban_cards (id, title, status, repo_id, assigned_agent_id)
             VALUES ($1, $2, 'review', $3, $4)",
        )
        .bind("card-pg-malformed-context")
        .bind("PG malformed create-pr context")
        .bind("repo-tracked")
        .bind("agent-reviewer")
        .execute(&pool)
        .await
        .expect("insert kanban card");

        sqlx::query(
            "INSERT INTO card_review_state (card_id, review_round, state)
             VALUES ($1, 7, 'in_review')",
        )
        .bind("card-pg-malformed-context")
        .execute(&pool)
        .await
        .expect("insert card review state");

        sqlx::query(
            "INSERT INTO pr_tracking (
                card_id,
                repo_id,
                worktree_path,
                branch,
                head_sha,
                state,
                last_error,
                dispatch_generation,
                review_round,
                retry_count,
                created_at,
                updated_at
             ) VALUES (
                $1, $2, $3, $4, $5, 'escalated', 'stale error', $6, 2, 4, NOW(), NOW()
             )",
        )
        .bind("card-pg-malformed-context")
        .bind("repo-tracked")
        .bind("/tracked/worktree")
        .bind("tracked/branch")
        .bind("tracked-head")
        .bind("tracked-generation")
        .execute(&pool)
        .await
        .expect("seed tracked pr_tracking row");

        sqlx::query(
            "INSERT INTO task_dispatches (
                id,
                kanban_card_id,
                dispatch_type,
                status,
                context,
                created_at,
                updated_at
             ) VALUES (
                $1, $2, 'create-pr', 'pending', $3, NOW(), NOW()
             )",
        )
        .bind("dispatch-pg-malformed-context")
        .bind("card-pg-malformed-context")
        .bind("{not-json")
        .execute(&pool)
        .await
        .expect("seed pending dispatch with malformed context");

        let payload = HandoffPayload {
            card_id: "card-pg-malformed-context".to_string(),
            repo_id: "repo-new".to_string(),
            worktree_path: Some("/new/worktree".to_string()),
            branch: "new/branch".to_string(),
            head_sha: Some("new-head".to_string()),
            agent_id: "agent-reviewer".to_string(),
            title: "Create PR".to_string(),
        };

        let reused = handoff_create_pr_pg(&pool, &payload)
            .await
            .expect("reuse should tolerate malformed postgres context");
        assert_eq!(reused["ok"], true);
        assert_eq!(reused["reused"], true);
        assert_eq!(reused["dispatch_id"], "dispatch-pg-malformed-context");
        assert_eq!(reused["generation"], "tracked-generation");

        let tracking = sqlx::query(
            "SELECT pt.repo_id,
                    pt.worktree_path,
                    pt.branch,
                    pt.head_sha,
                    pt.state,
                    pt.last_error,
                    pt.dispatch_generation,
                    pt.review_round,
                    pt.retry_count,
                    kc.blocked_reason
             FROM pr_tracking pt
             JOIN kanban_cards kc ON kc.id = pt.card_id
             WHERE pt.card_id = $1",
        )
        .bind("card-pg-malformed-context")
        .fetch_one(&pool)
        .await
        .expect("load refreshed tracking state");

        assert_eq!(
            tracking
                .try_get::<String, _>("repo_id")
                .expect("decode repo_id"),
            "repo-tracked"
        );
        assert_eq!(
            tracking
                .try_get::<Option<String>, _>("worktree_path")
                .expect("decode worktree_path")
                .as_deref(),
            Some("/tracked/worktree")
        );
        assert_eq!(
            tracking
                .try_get::<String, _>("branch")
                .expect("decode branch"),
            "tracked/branch"
        );
        assert_eq!(
            tracking
                .try_get::<Option<String>, _>("head_sha")
                .expect("decode head_sha")
                .as_deref(),
            Some("tracked-head")
        );
        assert_eq!(
            tracking
                .try_get::<String, _>("state")
                .expect("decode state"),
            "create-pr"
        );
        assert_eq!(
            tracking
                .try_get::<Option<String>, _>("last_error")
                .expect("decode last_error"),
            None
        );
        assert_eq!(
            tracking
                .try_get::<String, _>("dispatch_generation")
                .expect("decode generation"),
            "tracked-generation"
        );
        assert_eq!(
            tracking
                .try_get::<i64, _>("review_round")
                .expect("decode review_round"),
            7
        );
        assert_eq!(
            tracking
                .try_get::<i64, _>("retry_count")
                .expect("decode retry_count"),
            0
        );
        assert_eq!(
            tracking
                .try_get::<Option<String>, _>("blocked_reason")
                .expect("decode blocked_reason")
                .as_deref(),
            Some("pr:creating")
        );

        pool.close().await;
        test_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn review_automation_pg_reuse_state_guard_skips_inactive_dispatch() {
        let test_db = TestDatabase::create().await;
        let pool = test_db.migrate().await;

        sqlx::query(
            "INSERT INTO kanban_cards (id, title, status, repo_id)
             VALUES ($1, $2, 'review', $3)",
        )
        .bind("card-pg-inactive-reuse")
        .bind("PG inactive reuse guard")
        .bind("repo-tracked")
        .execute(&pool)
        .await
        .expect("insert kanban card");

        sqlx::query(
            "UPDATE kanban_cards
             SET blocked_reason = 'pr:failed'
             WHERE id = $1",
        )
        .bind("card-pg-inactive-reuse")
        .execute(&pool)
        .await
        .expect("seed blocked_reason");

        sqlx::query(
            "INSERT INTO pr_tracking (
                card_id,
                repo_id,
                worktree_path,
                branch,
                head_sha,
                state,
                last_error,
                dispatch_generation,
                review_round,
                retry_count,
                created_at,
                updated_at
             ) VALUES (
                $1, $2, $3, $4, $5, 'wait-ci', 'dispatch failed', $6, 4, 2, NOW(), NOW()
             )",
        )
        .bind("card-pg-inactive-reuse")
        .bind("repo-tracked")
        .bind("/tracked/worktree")
        .bind("tracked/branch")
        .bind("tracked-head")
        .bind("tracked-generation")
        .execute(&pool)
        .await
        .expect("seed tracked pr_tracking row");

        sqlx::query(
            "INSERT INTO task_dispatches (
                id,
                kanban_card_id,
                dispatch_type,
                status,
                context,
                created_at,
                updated_at
             ) VALUES (
                $1, $2, 'create-pr', 'failed', $3, NOW(), NOW()
             )",
        )
        .bind("dispatch-pg-inactive-reuse")
        .bind("card-pg-inactive-reuse")
        .bind("{\"dispatch_generation\":\"tracked-generation\"}")
        .execute(&pool)
        .await
        .expect("seed failed dispatch");

        let payload = HandoffPayload {
            card_id: "card-pg-inactive-reuse".to_string(),
            repo_id: "repo-new".to_string(),
            worktree_path: Some("/new/worktree".to_string()),
            branch: "new/branch".to_string(),
            head_sha: Some("new-head".to_string()),
            agent_id: "agent-reviewer".to_string(),
            title: "Create PR".to_string(),
        };

        let mut tx = pool.begin().await.expect("begin tx");
        let applied = refresh_pg_pr_tracking_reuse_state_if_active(
            &mut tx,
            &payload,
            "dispatch-pg-inactive-reuse",
            "generation-new",
            8,
        )
        .await
        .expect("inactive dispatch should skip reuse rewrites");
        tx.commit().await.expect("commit tx");

        assert!(!applied);

        let tracking = sqlx::query(
            "SELECT pt.repo_id,
                    pt.worktree_path,
                    pt.branch,
                    pt.head_sha,
                    pt.state,
                    pt.last_error,
                    pt.dispatch_generation,
                    pt.review_round,
                    pt.retry_count,
                    kc.blocked_reason
             FROM pr_tracking pt
             JOIN kanban_cards kc ON kc.id = pt.card_id
             WHERE pt.card_id = $1",
        )
        .bind("card-pg-inactive-reuse")
        .fetch_one(&pool)
        .await
        .expect("load tracking after inactive reuse attempt");

        assert_eq!(
            tracking
                .try_get::<String, _>("repo_id")
                .expect("decode repo_id"),
            "repo-tracked"
        );
        assert_eq!(
            tracking
                .try_get::<Option<String>, _>("worktree_path")
                .expect("decode worktree_path")
                .as_deref(),
            Some("/tracked/worktree")
        );
        assert_eq!(
            tracking
                .try_get::<String, _>("branch")
                .expect("decode branch"),
            "tracked/branch"
        );
        assert_eq!(
            tracking
                .try_get::<Option<String>, _>("head_sha")
                .expect("decode head_sha")
                .as_deref(),
            Some("tracked-head")
        );
        assert_eq!(
            tracking
                .try_get::<String, _>("state")
                .expect("decode state"),
            "wait-ci"
        );
        assert_eq!(
            tracking
                .try_get::<Option<String>, _>("last_error")
                .expect("decode last_error")
                .as_deref(),
            Some("dispatch failed")
        );
        assert_eq!(
            tracking
                .try_get::<String, _>("dispatch_generation")
                .expect("decode dispatch_generation"),
            "tracked-generation"
        );
        assert_eq!(
            tracking
                .try_get::<i64, _>("review_round")
                .expect("decode review_round"),
            4
        );
        assert_eq!(
            tracking
                .try_get::<i64, _>("retry_count")
                .expect("decode retry_count"),
            2
        );
        assert_eq!(
            tracking
                .try_get::<Option<String>, _>("blocked_reason")
                .expect("decode blocked_reason")
                .as_deref(),
            Some("pr:failed")
        );

        pool.close().await;
        test_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn review_automation_pg_handoff_failure_and_reseed_round_trip() {
        let test_db = TestDatabase::create().await;
        let pool = test_db.migrate().await;

        sqlx::query(
            "INSERT INTO agents (id, name, provider, status)
             VALUES ($1, $2, 'codex', 'idle')",
        )
        .bind("agent-reviewer")
        .bind("Reviewer Agent")
        .execute(&pool)
        .await
        .expect("insert reviewer agent");

        sqlx::query(
            "INSERT INTO kanban_cards (id, title, status, repo_id, assigned_agent_id)
             VALUES ($1, $2, 'review', $3, $4)",
        )
        .bind("card-pg-review")
        .bind("Review automation PG card")
        .bind("repo-1")
        .bind("agent-reviewer")
        .execute(&pool)
        .await
        .expect("insert kanban card");

        sqlx::query(
            "INSERT INTO card_review_state (card_id, review_round, state)
             VALUES ($1, 2, 'in_review')",
        )
        .bind("card-pg-review")
        .execute(&pool)
        .await
        .expect("insert card review state");

        let payload = HandoffPayload {
            card_id: "card-pg-review".to_string(),
            repo_id: "repo-1".to_string(),
            worktree_path: Some("/tmp/worktree/repo-1".to_string()),
            branch: "feature/review-automation".to_string(),
            head_sha: Some("abc123".to_string()),
            agent_id: "agent-reviewer".to_string(),
            title: "Create PR".to_string(),
        };

        let handoff = handoff_create_pr_pg(&pool, &payload)
            .await
            .expect("handoff create pr");
        assert_eq!(handoff["ok"], true);
        assert_eq!(handoff["reused"], false);
        let dispatch_id = handoff["dispatch_id"]
            .as_str()
            .expect("dispatch id")
            .to_string();
        let generation = handoff["generation"]
            .as_str()
            .expect("generation")
            .to_string();

        let blocked_reason: Option<String> = sqlx::query_scalar(
            "SELECT blocked_reason
             FROM kanban_cards
             WHERE id = $1",
        )
        .bind(&payload.card_id)
        .fetch_one(&pool)
        .await
        .expect("load blocked reason");
        assert_eq!(blocked_reason.as_deref(), Some("pr:creating"));

        let dispatch_status: String = sqlx::query_scalar(
            "SELECT status
             FROM task_dispatches
             WHERE id = $1",
        )
        .bind(&dispatch_id)
        .fetch_one(&pool)
        .await
        .expect("load dispatch status");
        assert_eq!(dispatch_status, "pending");

        let dispatch_context: String = sqlx::query_scalar(
            "SELECT context
             FROM task_dispatches
             WHERE id = $1",
        )
        .bind(&dispatch_id)
        .fetch_one(&pool)
        .await
        .expect("load dispatch context");
        let dispatch_context_json: serde_json::Value =
            serde_json::from_str(&dispatch_context).expect("parse dispatch context");
        assert_eq!(dispatch_context_json["dispatch_generation"], generation);
        assert_eq!(dispatch_context_json["review_round_at_dispatch"], 2);

        let tracking = sqlx::query(
            "SELECT state, dispatch_generation, review_round, retry_count
             FROM pr_tracking
             WHERE card_id = $1",
        )
        .bind(&payload.card_id)
        .fetch_one(&pool)
        .await
        .expect("load pr tracking after handoff");
        assert_eq!(
            tracking
                .try_get::<String, _>("state")
                .expect("decode tracking state"),
            "create-pr"
        );
        assert_eq!(
            tracking
                .try_get::<String, _>("dispatch_generation")
                .expect("decode tracking generation"),
            generation
        );
        assert_eq!(
            tracking
                .try_get::<i64, _>("review_round")
                .expect("decode tracking review round"),
            2
        );
        assert_eq!(
            tracking
                .try_get::<i64, _>("retry_count")
                .expect("decode tracking retry count"),
            0
        );

        let notify_outbox_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)
             FROM dispatch_outbox
             WHERE dispatch_id = $1
               AND action = 'notify'",
        )
        .bind(&dispatch_id)
        .fetch_one(&pool)
        .await
        .expect("count notify outbox");
        assert_eq!(notify_outbox_count, 1);

        let pending_event_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)
             FROM dispatch_events
             WHERE dispatch_id = $1
               AND to_status = 'pending'",
        )
        .bind(&dispatch_id)
        .fetch_one(&pool)
        .await
        .expect("count pending dispatch events");
        assert_eq!(pending_event_count, 1);

        sqlx::query(
            "UPDATE pr_tracking
             SET dispatch_generation = '00000000-0000-0000-0000-stale0reuse01'
             WHERE card_id = $1",
        )
        .bind(&payload.card_id)
        .execute(&pool)
        .await
        .expect("force stale tracking generation before reuse");
        sqlx::query(
            "UPDATE kanban_cards
             SET blocked_reason = NULL
             WHERE id = $1",
        )
        .bind(&payload.card_id)
        .execute(&pool)
        .await
        .expect("clear blocked reason before reuse");

        let reused_payload = HandoffPayload {
            repo_id: "repo-reused-override".to_string(),
            worktree_path: Some("/tmp/worktree/reused-override".to_string()),
            branch: "feature/reused-override".to_string(),
            head_sha: Some("override-head-456".to_string()),
            ..payload.clone()
        };

        let second_handoff = handoff_create_pr_pg(&pool, &reused_payload)
            .await
            .expect("second handoff create pr");
        assert_eq!(second_handoff["ok"], true);
        assert_eq!(second_handoff["reused"], true);
        assert_eq!(second_handoff["dispatch_id"], dispatch_id);
        assert_eq!(second_handoff["generation"], generation);

        let reused_tracking = sqlx::query(
            "SELECT pt.repo_id,
                    pt.worktree_path,
                    pt.branch,
                    pt.head_sha,
                    pt.dispatch_generation,
                    kc.blocked_reason
             FROM pr_tracking pt
             JOIN kanban_cards kc ON kc.id = pt.card_id
             WHERE pt.card_id = $1",
        )
        .bind(&payload.card_id)
        .fetch_one(&pool)
        .await
        .expect("load refreshed postgres reuse state");
        assert_eq!(
            reused_tracking
                .try_get::<String, _>("repo_id")
                .expect("decode refreshed postgres repo_id"),
            payload.repo_id
        );
        assert_eq!(
            reused_tracking
                .try_get::<Option<String>, _>("worktree_path")
                .expect("decode refreshed postgres worktree_path")
                .as_deref(),
            payload.worktree_path.as_deref()
        );
        assert_eq!(
            reused_tracking
                .try_get::<String, _>("branch")
                .expect("decode refreshed postgres branch"),
            payload.branch
        );
        assert_eq!(
            reused_tracking
                .try_get::<Option<String>, _>("head_sha")
                .expect("decode refreshed postgres head_sha")
                .as_deref(),
            payload.head_sha.as_deref()
        );
        assert_eq!(
            reused_tracking
                .try_get::<String, _>("dispatch_generation")
                .expect("decode refreshed postgres generation"),
            generation
        );
        assert_eq!(
            reused_tracking
                .try_get::<Option<String>, _>("blocked_reason")
                .expect("decode refreshed postgres blocked_reason")
                .as_deref(),
            Some("pr:creating")
        );

        let first_failure =
            record_pr_create_failure_pg(&pool, &payload.card_id, "git push failed", &generation)
                .await
                .expect("record first pr create failure");
        assert_eq!(first_failure["retry_count"], 1);
        assert_eq!(first_failure["escalated"], false);

        let second_failure = record_pr_create_failure_pg(
            &pool,
            &payload.card_id,
            "git push failed again",
            &generation,
        )
        .await
        .expect("record second pr create failure");
        assert_eq!(second_failure["retry_count"], 2);
        assert_eq!(second_failure["escalated"], false);

        let third_failure =
            record_pr_create_failure_pg(&pool, &payload.card_id, "permission denied", &generation)
                .await
                .expect("record third pr create failure");
        assert_eq!(third_failure["retry_count"], 3);
        assert_eq!(third_failure["escalated"], true);

        let escalated_state: String = sqlx::query_scalar(
            "SELECT state
             FROM pr_tracking
             WHERE card_id = $1",
        )
        .bind(&payload.card_id)
        .fetch_one(&pool)
        .await
        .expect("load escalated pr tracking state");
        assert_eq!(escalated_state, "escalated");

        sqlx::query(
            "INSERT INTO task_dispatches (
                id,
                kanban_card_id,
                dispatch_type,
                status,
                result,
                created_at,
                updated_at,
                completed_at
             ) VALUES (
                $1, $2, 'implementation', 'completed', $3, NOW(), NOW(), NOW()
             )",
        )
        .bind("impl-completed-1")
        .bind(&payload.card_id)
        .bind(r#"{"head_sha":"new-head-789"}"#)
        .execute(&pool)
        .await
        .expect("insert completed implementation dispatch");

        let reseed = reseed_pr_tracking_pg(&pool, &payload.card_id)
            .await
            .expect("reseed pr tracking");
        assert_eq!(reseed["ok"], true);
        let new_generation = reseed["generation"]
            .as_str()
            .expect("new generation")
            .to_string();
        assert_ne!(new_generation, generation);

        let tracking_after_reseed = sqlx::query(
            "SELECT state, dispatch_generation, review_round, retry_count, last_error, head_sha
             FROM pr_tracking
             WHERE card_id = $1",
        )
        .bind(&payload.card_id)
        .fetch_one(&pool)
        .await
        .expect("load pr tracking after reseed");
        assert_eq!(
            tracking_after_reseed
                .try_get::<String, _>("state")
                .expect("decode reseeded state"),
            "create-pr"
        );
        assert_eq!(
            tracking_after_reseed
                .try_get::<String, _>("dispatch_generation")
                .expect("decode reseeded generation"),
            new_generation
        );
        assert_eq!(
            tracking_after_reseed
                .try_get::<i64, _>("review_round")
                .expect("decode reseeded review round"),
            2
        );
        assert_eq!(
            tracking_after_reseed
                .try_get::<i64, _>("retry_count")
                .expect("decode reseeded retry count"),
            0
        );
        assert_eq!(
            tracking_after_reseed
                .try_get::<Option<String>, _>("last_error")
                .expect("decode reseeded last error"),
            None
        );
        assert_eq!(
            tracking_after_reseed
                .try_get::<Option<String>, _>("head_sha")
                .expect("decode reseeded head sha")
                .as_deref(),
            Some("new-head-789")
        );

        let cancelled_status: String = sqlx::query_scalar(
            "SELECT status
             FROM task_dispatches
             WHERE id = $1",
        )
        .bind(&dispatch_id)
        .fetch_one(&pool)
        .await
        .expect("load cancelled dispatch status");
        assert_eq!(cancelled_status, "cancelled");

        let cancel_event_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)
             FROM dispatch_events
             WHERE dispatch_id = $1
               AND to_status = 'cancelled'",
        )
        .bind(&dispatch_id)
        .fetch_one(&pool)
        .await
        .expect("count cancelled dispatch events");
        assert_eq!(cancel_event_count, 1);

        let status_reaction_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)
             FROM dispatch_outbox
             WHERE dispatch_id = $1
               AND action = 'status_reaction'",
        )
        .bind(&dispatch_id)
        .fetch_one(&pool)
        .await
        .expect("count status reaction outbox");
        assert_eq!(status_reaction_count, 1);

        pool.close().await;
        test_db.drop().await;
    }

    // Helper used by the atomicity-focused tests below. Seeds a card + active
    // create-pr dispatch + pr_tracking row + a dispatched auto_queue entry tied
    // to that dispatch so `reseed_pr_tracking_pg` has state to cancel/reset and
    // rewrite. Returns the dispatch id, the original pr_tracking generation,
    // and the seeded auto_queue entry id (so the rollback test can verify the
    // queue half also rolls back atomically).
    async fn seed_reseed_fixture(pool: &sqlx::PgPool, card_id: &str) -> (String, String, String) {
        sqlx::query(
            "INSERT INTO kanban_cards (id, title, status, repo_id)
             VALUES ($1, $2, 'review', 'repo-1')",
        )
        .bind(card_id)
        .bind("Atomicity fixture")
        .execute(pool)
        .await
        .expect("insert kanban card");

        sqlx::query(
            "INSERT INTO card_review_state (card_id, review_round, state)
             VALUES ($1, 1, 'in_review')",
        )
        .bind(card_id)
        .execute(pool)
        .await
        .expect("insert card review state");

        let dispatch_id = format!("dispatch-{}", uuid::Uuid::new_v4().simple());
        sqlx::query(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, dispatch_type, status, created_at, updated_at
             ) VALUES ($1, $2, 'create-pr', 'pending', NOW(), NOW())",
        )
        .bind(&dispatch_id)
        .bind(card_id)
        .execute(pool)
        .await
        .expect("insert pending create-pr dispatch");

        let original_generation = "gen-original".to_string();
        sqlx::query(
            "INSERT INTO pr_tracking (
                card_id, state, dispatch_generation, review_round, retry_count,
                created_at, updated_at
             ) VALUES ($1, 'create-pr', $2, 0, 0, NOW(), NOW())",
        )
        .bind(card_id)
        .bind(&original_generation)
        .execute(pool)
        .await
        .expect("insert initial pr_tracking row");

        // Seed an auto_queue run + entry tied to the dispatch so the cancel
        // helper exercises its queue-reset half (it resets status and clears
        // dispatch_id / slot_index, and inserts an auto_queue_entry_transitions
        // row). The atomicity tests assert these mutations also roll back when
        // the caller-owned tx is aborted.
        let run_id = format!("run-{}", uuid::Uuid::new_v4().simple());
        sqlx::query(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, max_concurrent_threads, thread_group_count,
                created_at
             ) VALUES ($1, 'itismyfield/AgentDesk', 'project-agentdesk', 'running', 1, 1, NOW())",
        )
        .bind(&run_id)
        .execute(pool)
        .await
        .expect("insert auto_queue run");

        let entry_id = format!("entry-{}", uuid::Uuid::new_v4().simple());
        sqlx::query(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, status, dispatch_id, slot_index,
                batch_phase, thread_group, dispatched_at, created_at
             ) VALUES ($1, $2, $3, 'dispatched', $4, 0, 1, 1, NOW(), NOW())",
        )
        .bind(&entry_id)
        .bind(&run_id)
        .bind(card_id)
        .bind(&dispatch_id)
        .execute(pool)
        .await
        .expect("insert dispatched auto_queue entry");

        (dispatch_id, original_generation, entry_id)
    }

    // Regression guard for #766: a successful reseed leaves dispatch cancelled
    // AND pr_tracking rewritten with the new generation inside the same
    // observable state. Before the fix the two mutations lived in separate
    // transactions, creating a crash window where only one half applied.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reseed_pr_tracking_pg_is_atomic_on_success() {
        let test_db = TestDatabase::create().await;
        let pool = test_db.migrate().await;

        let card_id = "card-reseed-atomic-ok";
        let (dispatch_id, original_generation, entry_id) =
            seed_reseed_fixture(&pool, card_id).await;
        let _ = entry_id; // success path doesn't assert per-entry — see rollback test

        let reseed = reseed_pr_tracking_pg(&pool, card_id)
            .await
            .expect("reseed pr tracking");
        assert_eq!(reseed["ok"], true);
        let new_generation = reseed["generation"]
            .as_str()
            .expect("new generation")
            .to_string();
        assert_ne!(new_generation, original_generation);

        let dispatch_status: String =
            sqlx::query_scalar("SELECT status FROM task_dispatches WHERE id = $1")
                .bind(&dispatch_id)
                .fetch_one(&pool)
                .await
                .expect("load dispatch status");
        assert_eq!(dispatch_status, "cancelled");

        let tracking_generation: String =
            sqlx::query_scalar("SELECT dispatch_generation FROM pr_tracking WHERE card_id = $1")
                .bind(card_id)
                .fetch_one(&pool)
                .await
                .expect("load pr_tracking generation");
        assert_eq!(tracking_generation, new_generation);

        pool.close().await;
        test_db.drop().await;
    }

    // Atomicity guard: when the cancel/reset helper runs inside a caller-owned
    // transaction that subsequently rolls back, NEITHER the dispatch cancel
    // NOR the auto-queue reset must persist. This is the exact contract
    // `reseed_pr_tracking_pg` relies on to stay atomic across both mutations.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancel_dispatch_and_reset_auto_queue_on_pg_tx_rolls_back_with_caller() {
        let test_db = TestDatabase::create().await;
        let pool = test_db.migrate().await;

        let card_id = "card-reseed-atomic-rollback";
        let (dispatch_id, original_generation, entry_id) =
            seed_reseed_fixture(&pool, card_id).await;

        let mut tx = pool.begin().await.expect("begin outer tx");
        let changed = crate::dispatch::cancel_dispatch_and_reset_auto_queue_on_pg_tx(
            &mut tx,
            &dispatch_id,
            Some("superseded_by_reseed"),
        )
        .await
        .expect("cancel dispatch inside caller tx");
        assert_eq!(changed, 1, "cancel should mark exactly one dispatch");

        // Caller decides to abort the wider unit of work. The dispatch cancel
        // must be rolled back atomically.
        tx.rollback().await.expect("rollback outer tx");

        let dispatch_status: String =
            sqlx::query_scalar("SELECT status FROM task_dispatches WHERE id = $1")
                .bind(&dispatch_id)
                .fetch_one(&pool)
                .await
                .expect("load dispatch status after rollback");
        assert_eq!(
            dispatch_status, "pending",
            "rollback must revert the dispatch cancel"
        );

        let tracking_generation: String =
            sqlx::query_scalar("SELECT dispatch_generation FROM pr_tracking WHERE card_id = $1")
                .bind(card_id)
                .fetch_one(&pool)
                .await
                .expect("load pr_tracking generation after rollback");
        assert_eq!(
            tracking_generation, original_generation,
            "pr_tracking must remain at the original generation when the outer tx rolls back"
        );

        let cancel_event_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM dispatch_events
             WHERE dispatch_id = $1 AND to_status = 'cancelled'",
        )
        .bind(&dispatch_id)
        .fetch_one(&pool)
        .await
        .expect("count cancel events after rollback");
        assert_eq!(
            cancel_event_count, 0,
            "rollback must also discard the dispatch_events audit row"
        );

        // Auto-queue half of the helper must roll back too: the entry stays
        // 'dispatched' with its dispatch_id intact, and no
        // auto_queue_entry_transitions row was persisted for the cancel.
        let entry_status: String =
            sqlx::query_scalar("SELECT status FROM auto_queue_entries WHERE id = $1")
                .bind(&entry_id)
                .fetch_one(&pool)
                .await
                .expect("load auto_queue entry status after rollback");
        assert_eq!(
            entry_status, "dispatched",
            "rollback must revert the auto_queue entry status reset"
        );

        let entry_dispatch_id: Option<String> =
            sqlx::query_scalar("SELECT dispatch_id FROM auto_queue_entries WHERE id = $1")
                .bind(&entry_id)
                .fetch_one(&pool)
                .await
                .expect("load auto_queue entry dispatch_id after rollback");
        assert_eq!(
            entry_dispatch_id.as_deref(),
            Some(dispatch_id.as_str()),
            "rollback must revert the auto_queue entry dispatch_id clear"
        );

        let queue_transition_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM auto_queue_entry_transitions
             WHERE entry_id = $1 AND trigger_source = 'dispatch_cancel'",
        )
        .bind(&entry_id)
        .fetch_one(&pool)
        .await
        .expect("count auto_queue_entry_transitions after rollback");
        assert_eq!(
            queue_transition_count, 0,
            "rollback must also discard the auto_queue_entry_transitions row"
        );

        pool.close().await;
        test_db.drop().await;
    }

    /// #821 (1+2, PG variant): the PostgreSQL `cancel_dispatch_and_reset_*`
    /// path must honour the same user-cancel invariants as the SQLite path
    /// (src/dispatch/mod.rs tests):
    ///   - entry transitions to `user_cancelled` (NOT `pending`).
    ///   - card status stays `in_progress` (NOT forced into `done`).
    ///   - the next auto-queue tick query (`active` run + `pending` entry)
    ///     does not surface this entry, so no re-dispatch can fire.
    ///
    /// This locks the behaviour that #815 P2 landed for the PG branch
    /// (routing through `update_entry_status_on_pg_tx` with the
    /// `ENTRY_STATUS_USER_CANCELLED` target).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn user_stop_does_not_redispatch_or_mark_done_pg() {
        let test_db = TestDatabase::create().await;
        let pool = test_db.migrate().await;

        let card_id = "card-821-user-pg";
        let dispatch_id = format!("dispatch-{}", uuid::Uuid::new_v4().simple());
        let run_id = format!("run-{}", uuid::Uuid::new_v4().simple());
        let entry_id = format!("entry-{}", uuid::Uuid::new_v4().simple());

        sqlx::query(
            "INSERT INTO kanban_cards (id, title, status, repo_id)
             VALUES ($1, 'User cancel PG', 'in_progress', 'repo-1')",
        )
        .bind(card_id)
        .execute(&pool)
        .await
        .expect("seed kanban card");

        sqlx::query(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, dispatch_type, status, created_at, updated_at
             ) VALUES ($1, $2, 'implementation', 'dispatched', NOW(), NOW())",
        )
        .bind(&dispatch_id)
        .bind(card_id)
        .execute(&pool)
        .await
        .expect("seed dispatched task dispatch");

        sqlx::query(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, max_concurrent_threads, thread_group_count,
                created_at
             ) VALUES ($1, 'itismyfield/AgentDesk', 'project-agentdesk', 'active', 1, 1, NOW())",
        )
        .bind(&run_id)
        .execute(&pool)
        .await
        .expect("seed active auto_queue run");

        sqlx::query(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, status, dispatch_id, slot_index,
                batch_phase, thread_group, dispatched_at, created_at
             ) VALUES ($1, $2, $3, 'dispatched', $4, 0, 0, 0, NOW(), NOW())",
        )
        .bind(&entry_id)
        .bind(&run_id)
        .bind(card_id)
        .bind(&dispatch_id)
        .execute(&pool)
        .await
        .expect("seed dispatched auto_queue entry");

        // User stop via the PG cancel path with the canonical reaction-stop
        // reason. Uses the pool-scoped helper so the test exercises the
        // commit path (not the rollback path that the sibling test covers).
        let changed = crate::dispatch::cancel_dispatch_and_reset_auto_queue_on_pg(
            &pool,
            &dispatch_id,
            Some("turn_bridge_cancelled"),
        )
        .await
        .expect("PG cancel with user reason must succeed");
        assert_eq!(changed, 1);

        // (1) Entry must be `user_cancelled`, dispatch pointer cleared,
        // completed_at stamped — and the next tick query must NOT see it.
        let entry_status: String =
            sqlx::query_scalar("SELECT status FROM auto_queue_entries WHERE id = $1")
                .bind(&entry_id)
                .fetch_one(&pool)
                .await
                .expect("load entry status");
        assert_eq!(
            entry_status, "user_cancelled",
            "PG user cancel must move the entry to user_cancelled (not pending)"
        );
        let entry_dispatch_id: Option<String> =
            sqlx::query_scalar("SELECT dispatch_id FROM auto_queue_entries WHERE id = $1")
                .bind(&entry_id)
                .fetch_one(&pool)
                .await
                .expect("load entry dispatch_id");
        assert!(
            entry_dispatch_id.is_none(),
            "PG user cancel must detach the entry from its dispatch"
        );

        let tick_visible: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM auto_queue_entries e
             JOIN auto_queue_runs r ON e.run_id = r.id
             WHERE r.status = 'active' AND e.status = 'pending' AND e.id = $1",
        )
        .bind(&entry_id)
        .fetch_one(&pool)
        .await
        .expect("tick-visibility query");
        assert_eq!(
            tick_visible, 0,
            "user-cancelled entry must not be visible to the PG tick"
        );

        // Run must stay active so the operator can flip the entry back to
        // pending if they want to restart — see the SQLite
        // `user_cancelled_entry_can_be_restarted_via_pending_flip` test.
        let run_status: String =
            sqlx::query_scalar("SELECT status FROM auto_queue_runs WHERE id = $1")
                .bind(&run_id)
                .fetch_one(&pool)
                .await
                .expect("load run status");
        assert_eq!(
            run_status, "active",
            "PG user cancel must leave the run active for operator restart"
        );

        // (2) Card must remain `in_progress` — the cancel path must NOT
        // force-transition it into `done` / `review` / any terminal state.
        let card_status: String =
            sqlx::query_scalar("SELECT status FROM kanban_cards WHERE id = $1")
                .bind(card_id)
                .fetch_one(&pool)
                .await
                .expect("load card status");
        assert_eq!(
            card_status, "in_progress",
            "PG user cancel must not mark the card done"
        );

        pool.close().await;
        test_db.drop().await;
    }
}
