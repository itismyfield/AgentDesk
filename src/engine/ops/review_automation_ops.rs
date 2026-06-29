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

use crate::dispatch::{DispatchCreateOptions, apply_dispatch_attached_intents_on_pg_tx};
use rquickjs::{Ctx, Function, Object, Result as JsResult};
use serde::Deserialize;
use serde_json::json;
use sqlx::{PgPool, Row};
use uuid::Uuid;

pub(super) fn register_review_automation_ops<'js>(
    ctx: &Ctx<'js>,
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
        false,
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
