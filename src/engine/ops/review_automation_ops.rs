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
use rquickjs::{Ctx, Function, Object, Result as JsResult};
use rusqlite::OptionalExtension;
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

pub(super) fn register_review_automation_ops<'js>(ctx: &Ctx<'js>, db: Db) -> JsResult<()> {
    let ad: Object<'js> = ctx.globals().get("agentdesk")?;
    let obj = Object::new(ctx.clone())?;

    let db_handoff = db.clone();
    obj.set(
        "__handoffCreatePrRaw",
        Function::new(ctx.clone(), move |payload_json: String| -> String {
            handoff_create_pr_raw(&db_handoff, &payload_json)
        })?,
    )?;

    let db_record = db.clone();
    obj.set(
        "__recordPrCreateFailureRaw",
        Function::new(
            ctx.clone(),
            move |card_id: String, error: String, stamp_gen: String| -> String {
                record_pr_create_failure_raw(&db_record, &card_id, &error, &stamp_gen)
            },
        )?,
    )?;

    let db_reseed = db.clone();
    obj.set(
        "__reseedPrTrackingRaw",
        Function::new(ctx.clone(), move |card_id: String| -> String {
            reseed_pr_tracking_raw(&db_reseed, &card_id)
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

#[derive(Debug, Deserialize)]
struct HandoffPayload {
    card_id: String,
    repo_id: String,
    worktree_path: Option<String>,
    branch: String,
    head_sha: Option<String>,
    agent_id: String,
    title: String,
}

fn handoff_create_pr_raw(db: &Db, payload_json: &str) -> String {
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
    match handoff_create_pr_tx(db, &payload) {
        Ok(v) => v.to_string(),
        Err(e) => json!({"error": format!("{e}")}).to_string(),
    }
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
        rusqlite::params![
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

fn record_pr_create_failure_raw(db: &Db, card_id: &str, error: &str, stamp_gen: &str) -> String {
    if card_id.trim().is_empty() {
        return json!({"error": "card_id is required"}).to_string();
    }
    match record_pr_create_failure_tx(db, card_id, error, stamp_gen) {
        Ok(v) => v.to_string(),
        Err(e) => json!({"error": format!("{e}")}).to_string(),
    }
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
        rusqlite::params![error, card_id],
    )?;
    if updated == 0 {
        // Row missing — caller's handoff tx rolled back before seeding. Create
        // a fresh row with retry_count=1 so the JS retry loop can pick it up.
        tx.execute(
            "INSERT INTO pr_tracking ( \
               card_id, state, last_error, dispatch_generation, review_round, retry_count, \
               created_at, updated_at \
             ) VALUES (?1, 'create-pr', ?2, '', 0, 1, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)",
            rusqlite::params![card_id, error],
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

fn reseed_pr_tracking_raw(db: &Db, card_id: &str) -> String {
    if card_id.trim().is_empty() {
        return json!({"error": "card_id is required"}).to_string();
    }
    match reseed_pr_tracking_tx(db, card_id) {
        Ok(v) => v.to_string(),
        Err(e) => json!({"error": format!("{e}")}).to_string(),
    }
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
        rusqlite::params![generation, current_round, latest_head, card_id],
    )?;
    if updated == 0 {
        // No row yet — create a minimal one so the retry loop can act on it.
        tx.execute(
            "INSERT INTO pr_tracking ( \
               card_id, head_sha, state, dispatch_generation, review_round, retry_count, \
               created_at, updated_at \
             ) VALUES (?1, ?2, 'create-pr', ?3, ?4, 0, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)",
            rusqlite::params![card_id, latest_head, generation, current_round],
        )?;
    }

    tx.commit()?;

    Ok(json!({
        "ok": true,
        "generation": generation,
    }))
}
