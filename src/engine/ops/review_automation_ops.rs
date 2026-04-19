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
use crate::dispatch::{DispatchCreateOptions, apply_dispatch_attached_intents_on_conn};
use libsql_rusqlite::OptionalExtension;
use rquickjs::{Ctx, Function, Object, Result as JsResult};
use serde::Deserialize;
use serde_json::json;
use sqlx::{PgPool, Row};
use uuid::Uuid;

pub(super) fn register_review_automation_ops<'js>(
    ctx: &Ctx<'js>,
    db: Db,
    pg_pool: Option<PgPool>,
) -> JsResult<()> {
    let ad: Object<'js> = ctx.globals().get("agentdesk")?;
    let obj = Object::new(ctx.clone())?;

    let db_handoff = db.clone();
    let pg_handoff = pg_pool.clone();
    obj.set(
        "__handoffCreatePrRaw",
        Function::new(ctx.clone(), move |payload_json: String| -> String {
            handoff_create_pr_raw(&db_handoff, pg_handoff.as_ref(), &payload_json)
        })?,
    )?;

    let db_record = db.clone();
    let pg_record = pg_pool.clone();
    obj.set(
        "__recordPrCreateFailureRaw",
        Function::new(
            ctx.clone(),
            move |card_id: String, error: String, stamp_gen: String| -> String {
                record_pr_create_failure_raw(
                    &db_record,
                    pg_record.as_ref(),
                    &card_id,
                    &error,
                    &stamp_gen,
                )
            },
        )?,
    )?;

    let db_reseed = db.clone();
    let pg_reseed = pg_pool.clone();
    obj.set(
        "__reseedPrTrackingRaw",
        Function::new(ctx.clone(), move |card_id: String| -> String {
            reseed_pr_tracking_raw(&db_reseed, pg_reseed.as_ref(), &card_id)
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

fn handoff_create_pr_raw(db: &Db, pg_pool: Option<&PgPool>, payload_json: &str) -> String {
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
    let result = if let Some(pool) = pg_pool {
        crate::utils::async_bridge::block_on_pg_result(
            pool,
            {
                let payload = payload.clone();
                move |bridge_pool| async move { handoff_create_pr_pg(&bridge_pool, &payload).await }
            },
            |error| error,
        )
    } else {
        handoff_create_pr_tx(db, &payload).map_err(|e| format!("{e}"))
    };
    match result {
        Ok(v) => v.to_string(),
        Err(e) => json!({"error": e}).to_string(),
    }
}

async fn handoff_create_pr_pg(
    pool: &PgPool,
    payload: &HandoffPayload,
) -> Result<serde_json::Value, String> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| format!("begin postgres review automation transaction: {e}"))?;

    let existing = sqlx::query(
        "SELECT td.id,
                COALESCE(pt.dispatch_generation, '') AS dispatch_generation
         FROM task_dispatches td
         LEFT JOIN pr_tracking pt ON pt.card_id = td.kanban_card_id
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
        tx.rollback().await.ok();
        return Ok(json!({
            "ok": true,
            "reused": true,
            "dispatch_id": dispatch_id,
            "generation": generation,
        }));
    }

    let card_exists = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (
            SELECT 1
            FROM kanban_cards
            WHERE id = $1
         )",
    )
    .bind(&payload.card_id)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| format!("check postgres card {} existence: {e}", payload.card_id))?;
    if !card_exists {
        return Err(format!("card {} not found", payload.card_id));
    }

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

    let generation = Uuid::new_v4().to_string();
    let dispatch_id = Uuid::new_v4().to_string();

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
    .bind(&generation)
    .bind(current_round)
    .execute(&mut *tx)
    .await
    .map_err(|e| format!("upsert postgres pr_tracking for {}: {e}", payload.card_id))?;

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

    let insert_dispatch = sqlx::query(
        "INSERT INTO task_dispatches (
            id,
            kanban_card_id,
            to_agent_id,
            dispatch_type,
            status,
            title,
            context,
            parent_dispatch_id,
            chain_depth,
            created_at,
            updated_at
        ) VALUES (
            $1, $2, $3, 'create-pr', 'pending', $4, $5, NULL, 0, NOW(), NOW()
        )",
    )
    .bind(&dispatch_id)
    .bind(&payload.card_id)
    .bind(&payload.agent_id)
    .bind(&payload.title)
    .bind(&context_str)
    .execute(&mut *tx)
    .await;
    if let Err(error) = insert_dispatch {
        if matches!(&error, sqlx::Error::Database(db_error) if db_error.is_unique_violation()) {
            tx.rollback().await.ok();
            return Err(format!(
                "create-pr already exists for card {} (concurrent race prevented by DB constraint)",
                payload.card_id
            ));
        }
        return Err(format!(
            "insert postgres create-pr dispatch {} for {}: {error}",
            dispatch_id, payload.card_id
        ));
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
        ) VALUES (
            $1, $2, 'create-pr', NULL, 'pending', 'create_dispatch', NULL
        )",
    )
    .bind(&dispatch_id)
    .bind(&payload.card_id)
    .execute(&mut *tx)
    .await
    .map_err(|e| {
        format!(
            "insert postgres create-pr dispatch event {}: {e}",
            dispatch_id
        )
    })?;

    sqlx::query(
        "INSERT INTO dispatch_outbox (dispatch_id, action, agent_id, card_id, title)
         VALUES ($1, 'notify', $2, $3, $4)
         ON CONFLICT DO NOTHING",
    )
    .bind(&dispatch_id)
    .bind(&payload.agent_id)
    .bind(&payload.card_id)
    .bind(&payload.title)
    .execute(&mut *tx)
    .await
    .map_err(|e| format!("insert postgres create-pr outbox {}: {e}", dispatch_id))?;

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

fn handoff_create_pr_tx(db: &Db, payload: &HandoffPayload) -> anyhow::Result<serde_json::Value> {
    let mut conn = db
        .separate_conn()
        .map_err(|e| anyhow::anyhow!("DB conn error: {e}"))?;
    let tx = conn.transaction()?;

    // 1. Idempotent reuse — if an active create-pr dispatch already exists for
    //    this card, return its id and the stored generation rather than erroring.
    //    This preserves the existing dedupe contract (C5).
    let existing: Option<(String, String)> = tx
        .query_row(
            "SELECT td.id, COALESCE(pt.dispatch_generation, '')
             FROM task_dispatches td
             LEFT JOIN pr_tracking pt ON pt.card_id = td.kanban_card_id
             WHERE td.kanban_card_id = ?1
               AND td.dispatch_type = 'create-pr'
               AND td.status IN ('pending', 'dispatched')
             ORDER BY td.rowid DESC LIMIT 1",
            [&payload.card_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some((dispatch_id, generation)) = existing {
        return Ok(json!({
            "ok": true,
            "reused": true,
            "dispatch_id": dispatch_id,
            "generation": generation,
        }));
    }

    // 2. Read card row for pipeline resolution + current status.
    let (old_status, card_repo_id, card_agent_id): (String, Option<String>, Option<String>) = tx
        .query_row(
            "SELECT status, repo_id, assigned_agent_id FROM kanban_cards WHERE id = ?1",
            [&payload.card_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .map_err(|e| anyhow::anyhow!("card not found: {e}"))?;

    // 3. Read current review_round (observability stamp).
    let current_round: i64 = tx
        .query_row(
            "SELECT review_round FROM card_review_state WHERE card_id = ?1",
            [&payload.card_id],
            |row| row.get(0),
        )
        .unwrap_or(0);

    // 4. Resolve pipeline for TransitionContext.
    crate::pipeline::ensure_loaded();
    let effective =
        crate::pipeline::resolve_for_card(&tx, card_repo_id.as_deref(), card_agent_id.as_deref());

    // 5. Mint fresh generation + dispatch id.
    let generation = Uuid::new_v4().to_string();
    let dispatch_id = Uuid::new_v4().to_string();

    // 6. pr_tracking upsert with stamp.
    tx.execute(
        "INSERT INTO pr_tracking \
         (card_id, repo_id, worktree_path, branch, head_sha, state, last_error, \
          dispatch_generation, review_round, retry_count, created_at, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, 'create-pr', NULL, ?6, ?7, 0, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP) \
         ON CONFLICT(card_id) DO UPDATE SET \
           repo_id = excluded.repo_id, \
           worktree_path = excluded.worktree_path, \
           branch = excluded.branch, \
           head_sha = excluded.head_sha, \
           state = 'create-pr', \
           last_error = NULL, \
           dispatch_generation = excluded.dispatch_generation, \
           review_round = excluded.review_round, \
           retry_count = 0, \
           updated_at = CURRENT_TIMESTAMP",
        libsql_rusqlite::params![
            payload.card_id,
            payload.repo_id,
            payload.worktree_path,
            payload.branch,
            payload.head_sha,
            generation,
            current_round,
        ],
    )?;

    // 7. Build dispatch context with stamps.
    let context = json!({
        "dispatch_generation": generation,
        "review_round_at_dispatch": current_round,
        "sidecar_dispatch": true,
        "worktree_path": payload.worktree_path,
        "worktree_branch": payload.branch,
        "branch": payload.branch,
    });
    let context_str = serde_json::to_string(&context)?;

    // 8. Insert the create-pr dispatch using the on-conn variant so the whole
    //    handoff stays in one transaction.
    apply_dispatch_attached_intents_on_conn(
        &tx,
        &payload.card_id,
        &payload.agent_id,
        &dispatch_id,
        "create-pr",
        false, // is_review_type
        &old_status,
        &effective,
        &payload.title,
        &context_str,
        None, // parent_dispatch_id
        0,    // chain_depth
        DispatchCreateOptions {
            sidecar_dispatch: true,
            ..Default::default()
        },
    )?;

    // 9. Stamp blocked_reason='pr:creating' so timeouts/escalation treat the
    //    in-flight handoff as benign progress (manual_intervention.rs benign
    //    list includes "pr:creating" as of #743 commit 1).
    tx.execute(
        "UPDATE kanban_cards SET blocked_reason = 'pr:creating', updated_at = datetime('now') \
         WHERE id = ?1",
        [&payload.card_id],
    )?;

    tx.commit()?;

    Ok(json!({
        "ok": true,
        "reused": false,
        "dispatch_id": dispatch_id,
        "generation": generation,
    }))
}

// ── recordPrCreateFailure ──────────────────────────────────────────────

fn record_pr_create_failure_raw(
    db: &Db,
    pg_pool: Option<&PgPool>,
    card_id: &str,
    error: &str,
    stamp_gen: &str,
) -> String {
    if card_id.trim().is_empty() {
        return json!({"error": "card_id is required"}).to_string();
    }
    let result = if let Some(pool) = pg_pool {
        let card_id = card_id.to_string();
        let error = error.to_string();
        let stamp_gen = stamp_gen.to_string();
        crate::utils::async_bridge::block_on_pg_result(
            pool,
            move |bridge_pool| async move {
                record_pr_create_failure_pg(&bridge_pool, &card_id, &error, &stamp_gen).await
            },
            |runtime_error| runtime_error,
        )
    } else {
        record_pr_create_failure_tx(db, card_id, error, stamp_gen).map_err(|e| format!("{e}"))
    };
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

fn record_pr_create_failure_tx(
    db: &Db,
    card_id: &str,
    error: &str,
    stamp_gen: &str,
) -> anyhow::Result<serde_json::Value> {
    let mut conn = db
        .separate_conn()
        .map_err(|e| anyhow::anyhow!("DB conn error: {e}"))?;
    let tx = conn.transaction()?;

    // Stale guard only applies when caller passes a non-empty stamp. A null /
    // empty stamp means the failure happened before a stamped dispatch existed
    // (e.g. handoffCreatePr itself threw and rolled back), so we must still
    // record the failure and bump retry_count.
    if !stamp_gen.is_empty() {
        let current_gen: Option<String> = tx
            .query_row(
                "SELECT dispatch_generation FROM pr_tracking WHERE card_id = ?1",
                [card_id],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(cur) = current_gen.as_deref() {
            if !cur.is_empty() && cur != stamp_gen {
                return Ok(json!({
                    "ok": true,
                    "noop": true,
                    "reason": "stale_generation",
                }));
            }
        }
    }

    // Try to UPDATE an existing row first.
    let updated = tx.execute(
        "UPDATE pr_tracking SET \
           retry_count = retry_count + 1, \
           last_error = ?1, \
           updated_at = CURRENT_TIMESTAMP \
         WHERE card_id = ?2",
        libsql_rusqlite::params![error, card_id],
    )?;
    if updated == 0 {
        // Row missing — caller's handoff tx rolled back before seeding. Create
        // a fresh row with retry_count=1 so the JS retry loop can pick it up.
        tx.execute(
            "INSERT INTO pr_tracking ( \
               card_id, state, last_error, dispatch_generation, review_round, retry_count, \
               created_at, updated_at \
             ) VALUES (?1, 'create-pr', ?2, '', 0, 1, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)",
            libsql_rusqlite::params![card_id, error],
        )?;
    }

    let retry_count: i64 = tx.query_row(
        "SELECT retry_count FROM pr_tracking WHERE card_id = ?1",
        [card_id],
        |row| row.get(0),
    )?;
    let escalated = retry_count >= 3;
    if escalated {
        tx.execute(
            "UPDATE pr_tracking SET state = 'escalated', updated_at = CURRENT_TIMESTAMP \
             WHERE card_id = ?1",
            [card_id],
        )?;
    }

    tx.commit()?;

    Ok(json!({
        "ok": true,
        "retry_count": retry_count,
        "escalated": escalated,
    }))
}

// ── reseedPrTracking ──────────────────────────────────────────────────

fn reseed_pr_tracking_raw(db: &Db, pg_pool: Option<&PgPool>, card_id: &str) -> String {
    if card_id.trim().is_empty() {
        return json!({"error": "card_id is required"}).to_string();
    }
    let result = if let Some(pool) = pg_pool {
        let card_id = card_id.to_string();
        crate::utils::async_bridge::block_on_pg_result(
            pool,
            move |bridge_pool| async move { reseed_pr_tracking_pg(&bridge_pool, &card_id).await },
            |runtime_error| runtime_error,
        )
    } else {
        reseed_pr_tracking_tx(db, card_id).map_err(|e| format!("{e}"))
    };
    match result {
        Ok(v) => v.to_string(),
        Err(e) => json!({"error": e}).to_string(),
    }
}

async fn reseed_pr_tracking_pg(pool: &PgPool, card_id: &str) -> Result<serde_json::Value, String> {
    // Run cancel/reset + pr_tracking generation/head/review_round update in a
    // single transaction so a crash mid-flight cannot leave the dispatch
    // cancelled while pr_tracking still points at the previous generation.
    // This matches the SQLite `reseed_pr_tracking_tx` semantics (see below).
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

fn reseed_pr_tracking_tx(db: &Db, card_id: &str) -> anyhow::Result<serde_json::Value> {
    let mut conn = db
        .separate_conn()
        .map_err(|e| anyhow::anyhow!("DB conn error: {e}"))?;
    let tx = conn.transaction()?;

    // Cancel any active create-pr dispatch. Without this the next
    // handoffCreatePr would either hit the partial unique index or be forced
    // into idempotent reuse of a stale dispatch that will never complete.
    let active_id: Option<String> = tx
        .query_row(
            "SELECT id FROM task_dispatches \
             WHERE kanban_card_id = ?1 AND dispatch_type = 'create-pr' \
               AND status IN ('pending', 'dispatched') \
             ORDER BY rowid DESC LIMIT 1",
            [card_id],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(id) = active_id {
        crate::dispatch::cancel_dispatch_and_reset_auto_queue_on_conn(
            &tx,
            &id,
            Some("superseded_by_reseed"),
        )?;
    }

    // Read the latest completed work dispatch's head_sha (if any) so the new
    // pr_tracking row tracks the candidate commit. Field preference matches
    // the JS loader loadLatestCompletedWorkTarget (result first, context
    // fallback; completed_commit / reviewed_commit / head_sha).
    let latest_head: Option<String> = tx
        .query_row(
            "SELECT COALESCE( \
               json_extract(td.result, '$.head_sha'), \
               json_extract(td.result, '$.completed_commit'), \
               json_extract(td.result, '$.reviewed_commit'), \
               json_extract(td.context, '$.completed_commit'), \
               json_extract(td.context, '$.reviewed_commit') \
             ) FROM task_dispatches td \
             WHERE td.kanban_card_id = ?1 \
               AND td.status = 'completed' \
               AND td.dispatch_type IN ('implementation', 'rework') \
             ORDER BY td.completed_at DESC LIMIT 1",
            [card_id],
            |row| row.get(0),
        )
        .optional()?
        .flatten();

    let current_round: i64 = tx
        .query_row(
            "SELECT review_round FROM card_review_state WHERE card_id = ?1",
            [card_id],
            |row| row.get(0),
        )
        .unwrap_or(0);

    let generation = Uuid::new_v4().to_string();

    let updated = tx.execute(
        "UPDATE pr_tracking SET \
           dispatch_generation = ?1, \
           review_round = ?2, \
           head_sha = COALESCE(?3, head_sha), \
           state = 'create-pr', \
           retry_count = 0, \
           last_error = NULL, \
           updated_at = CURRENT_TIMESTAMP \
         WHERE card_id = ?4",
        libsql_rusqlite::params![generation, current_round, latest_head, card_id],
    )?;
    if updated == 0 {
        // No row yet — create a minimal one so the retry loop can act on it.
        tx.execute(
            "INSERT INTO pr_tracking ( \
               card_id, head_sha, state, dispatch_generation, review_round, retry_count, \
               created_at, updated_at \
             ) VALUES (?1, ?2, 'create-pr', ?3, ?4, 0, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)",
            libsql_rusqlite::params![card_id, latest_head, generation, current_round],
        )?;
    }

    tx.commit()?;

    Ok(json!({
        "ok": true,
        "generation": generation,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDatabase {
        admin_url: String,
        database_name: String,
        database_url: String,
    }

    impl TestDatabase {
        async fn create() -> Self {
            let admin_url = admin_database_url();
            let database_name = format!("agentdesk_review_auto_{}", uuid::Uuid::new_v4().simple());
            let database_url = format!("{}/{}", base_database_url(), database_name);
            let admin_pool = sqlx::PgPool::connect(&admin_url)
                .await
                .expect("connect postgres admin db");
            sqlx::query(&format!("CREATE DATABASE \"{database_name}\""))
                .execute(&admin_pool)
                .await
                .expect("create postgres test db");
            admin_pool.close().await;

            Self {
                admin_url,
                database_name,
                database_url,
            }
        }

        async fn migrate(&self) -> sqlx::PgPool {
            let pool = sqlx::PgPool::connect(&self.database_url)
                .await
                .expect("connect postgres test db");
            crate::db::postgres::migrate(&pool)
                .await
                .expect("migrate postgres test db");
            pool
        }

        async fn drop(self) {
            let admin_pool = sqlx::PgPool::connect(&self.admin_url)
                .await
                .expect("reconnect postgres admin db");
            sqlx::query(
                "SELECT pg_terminate_backend(pid)
                 FROM pg_stat_activity
                 WHERE datname = $1
                   AND pid <> pg_backend_pid()",
            )
            .bind(&self.database_name)
            .execute(&admin_pool)
            .await
            .expect("terminate postgres test db sessions");
            sqlx::query(&format!(
                "DROP DATABASE IF EXISTS \"{}\"",
                self.database_name
            ))
            .execute(&admin_pool)
            .await
            .expect("drop postgres test db");
            admin_pool.close().await;
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
                .try_get::<i32, _>("review_round")
                .expect("decode tracking review round"),
            2
        );
        assert_eq!(
            tracking
                .try_get::<i32, _>("retry_count")
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

        let second_handoff = handoff_create_pr_pg(&pool, &payload)
            .await
            .expect("second handoff create pr");
        assert_eq!(second_handoff["ok"], true);
        assert_eq!(second_handoff["reused"], true);
        assert_eq!(second_handoff["dispatch_id"], dispatch_id);
        assert_eq!(second_handoff["generation"], generation);

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
                .try_get::<i32, _>("review_round")
                .expect("decode reseeded review round"),
            2
        );
        assert_eq!(
            tracking_after_reseed
                .try_get::<i32, _>("retry_count")
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
                id, agent_id, status, total_entries, batch_phase_max,
                created_at, updated_at
             ) VALUES ($1, 'project-agentdesk', 'running', 1, 1, NOW(), NOW())",
        )
        .bind(&run_id)
        .execute(pool)
        .await
        .expect("insert auto_queue run");

        let entry_id = format!("entry-{}", uuid::Uuid::new_v4().simple());
        sqlx::query(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, status, dispatch_id, slot_index,
                batch_phase, thread_group, dispatched_at, created_at, updated_at
             ) VALUES ($1, $2, $3, 'dispatched', $4, 0, 1, 1, NOW(), NOW(), NOW())",
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
}
