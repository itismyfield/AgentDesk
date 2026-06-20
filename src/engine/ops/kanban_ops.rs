use crate::db::Db;
use rquickjs::{Ctx, Function, Object, Result as JsResult};
use sqlx::{PgPool, Postgres, QueryBuilder, Row as SqlxRow};

// ── Kanban ops ────────────────────────────────────────────────────
//
// agentdesk.kanban.setStatus(cardId, newStatus, force?) — updates card status
// and fires appropriate hooks (OnCardTransition, OnCardTerminal, OnReviewEnter).
// This replaces direct SQL UPDATEs in policies to ensure hooks always fire.

fn enters_review_state(pipeline: &crate::pipeline::PipelineConfig, status: &str) -> bool {
    pipeline
        .hooks_for_state(status)
        .is_some_and(|hooks| hooks.on_enter.iter().any(|name| name == "OnReviewEnter"))
}

async fn auto_queue_review_disabled_for_card_on_pg(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    card_id: &str,
) -> Result<bool, String> {
    sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(
            SELECT 1
            FROM auto_queue_entries e
            JOIN auto_queue_runs r ON r.id = e.run_id
            JOIN kanban_cards c ON c.id = e.kanban_card_id
            LEFT JOIN task_dispatches d ON d.id = e.dispatch_id
            WHERE e.kanban_card_id = $1
              AND r.status IN ('active', 'paused')
              AND COALESCE(r.review_mode, 'enabled') = 'disabled'
              AND (
                    e.status = 'dispatched'
                    OR (
                        e.status = 'done'
                        AND c.latest_dispatch_id = d.id
                        AND d.status = 'completed'
                        AND d.dispatch_type IN ('implementation', 'rework')
                    )
              )
        )",
    )
    .bind(card_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(|error| format!("load auto-queue review_mode for {card_id}: {error}"))
}

pub(super) fn register_kanban_ops<'js>(
    ctx: &Ctx<'js>,
    db: Option<Db>,
    pg_pool: Option<PgPool>,
) -> JsResult<()> {
    let ad: Object<'js> = ctx.globals().get("agentdesk")?;
    let kanban_obj = Object::new(ctx.clone())?;

    let pg_set = pg_pool.clone();
    kanban_obj.set(
        "__setStatusRaw",
        Function::new(
            ctx.clone(),
            move |card_id: String, new_status: String, force: Option<bool>| -> String {
                let Some(pool) = pg_set.as_ref() else {
                    return r#"{"error":"postgres backend is required for kanban.setStatus"}"#
                        .to_string();
                };
                set_status_raw_pg(pool, &card_id, &new_status, force.unwrap_or(false))
            },
        )?,
    )?;

    let pg_reopen = pg_pool.clone();
    kanban_obj.set(
        "__reopenRaw",
        Function::new(
            ctx.clone(),
            move |card_id: String, new_status: String| -> String {
                let Some(pool) = pg_reopen.as_ref() else {
                    return r#"{"error":"postgres backend is required for kanban.reopen"}"#
                        .to_string();
                };
                reopen_raw_pg(pool, &card_id, &new_status)
            },
        )?,
    )?;

    let pg_get = pg_pool.clone();
    kanban_obj.set(
        "__getCardRaw",
        Function::new(ctx.clone(), move |card_id: String| -> String {
            let Some(pool) = pg_get.as_ref() else {
                return r#"{"error":"postgres backend is required for kanban.getCard"}"#
                    .to_string();
            };
            get_card_raw_pg(pool, &card_id)
        })?,
    )?;

    let pg_clear_latest = pg_pool.clone();
    kanban_obj.set(
        "__clearLatestDispatchRaw",
        Function::new(
            ctx.clone(),
            move |card_id: String, expected_dispatch_id: Option<String>| -> String {
                let Some(pool) = pg_clear_latest.as_ref() else {
                    return r#"{"error":"postgres backend is required for kanban.clearLatestDispatch"}"#
                        .to_string();
                };
                clear_latest_dispatch_raw_pg(pool, &card_id, expected_dispatch_id.as_deref())
            },
        )?,
    )?;

    // #155: setReviewStatus — controlled path for review_status + clock updates.
    // Replaces direct SQL UPDATEs so the ExecuteSQL guard can block bare review_status writes.
    let pg_review = pg_pool.clone();
    let _db_review = db;
    kanban_obj.set(
        "__setReviewStatusRaw",
        Function::new(
            ctx.clone(),
            move |card_id: String, opts_json: String| -> String {
                if let Some(pool) = pg_review.as_ref() {
                    return set_review_status_raw_pg(pool, &card_id, &opts_json);
                }
                r#"{"error":"postgres backend is required for kanban.setReviewStatus"}"#.to_string()
            },
        )?,
    )?;

    ad.set("kanban", kanban_obj)?;

    // JS wrapper that parses JSON and accumulates transitions for post-hook processing.
    // setStatus only updates the DB — transition hooks (OnCardTransition, OnReviewEnter,
    // OnCardTerminal) cannot fire from within a hook because the engine is not reentrant.
    // Instead, transitions are collected in __pendingTransitions and the Rust caller
    // processes them after the hook returns via drain_pending_transitions().
    let _: rquickjs::Value = ctx.eval(
        r#"
        (function() {
            agentdesk.kanban.__pendingTransitions = [];
            agentdesk.kanban.setStatus = function(cardId, newStatus, force) {
                var result = JSON.parse(
                    agentdesk.kanban.__setStatusRaw(cardId, newStatus, !!force)
                );
                if (result.error) throw new Error(result.error);
                if (result.changed) {
                    agentdesk.kanban.__pendingTransitions.push({
                        card_id: result.card_id,
                        from: result.from,
                        to: result.to
                    });
                    if (result.warning) {
                        agentdesk.log.warn("[setStatus] " + result.card_id + " " + result.from + " -> " + result.to + " — " + result.warning);
                    }
                    agentdesk.log.info("[setStatus] " + result.card_id + " " + result.from + " -> " + result.to + " (pendingLen=" + agentdesk.kanban.__pendingTransitions.length + ")");
                } else {
                    agentdesk.log.info("[setStatus] " + cardId + " -> " + newStatus + " (no-change)");
                }
                return result;
            };
            agentdesk.kanban.reopen = function(cardId, newStatus) {
                var result = JSON.parse(agentdesk.kanban.__reopenRaw(cardId, newStatus));
                if (result.error) throw new Error(result.error);
                if (result.changed) {
                    agentdesk.kanban.__pendingTransitions.push({
                        card_id: result.card_id,
                        from: result.from,
                        to: result.to
                    });
                    agentdesk.log.info("[reopen] " + result.card_id + " " + result.from + " -> " + result.to + " (pendingLen=" + agentdesk.kanban.__pendingTransitions.length + ")");
                } else {
                    agentdesk.log.info("[reopen] " + cardId + " -> " + newStatus + " (no-change)");
                }
                return result;
            };
            agentdesk.kanban.getCard = function(cardId) {
                var result = JSON.parse(agentdesk.kanban.__getCardRaw(cardId));
                if (result.error) return null;
                return result;
            };
            agentdesk.kanban.clearLatestDispatch = function(cardId, expectedDispatchId) {
                var result = JSON.parse(
                    agentdesk.kanban.__clearLatestDispatchRaw(cardId, expectedDispatchId || null)
                );
                if (result.error) throw new Error(result.error);
                return result;
            };
            agentdesk.kanban.setReviewStatus = function(cardId, reviewStatus, opts) {
                var o = opts || {};
                o.review_status = reviewStatus;
                var result = JSON.parse(
                    agentdesk.kanban.__setReviewStatusRaw(cardId, JSON.stringify(o))
                );
                if (result.error) throw new Error(result.error);
                return result;
            };
        })();
    "#,
    )?;

    Ok(())
}

fn set_status_raw_pg(pool: &PgPool, card_id: &str, new_status: &str, force: bool) -> String {
    use crate::engine::transition::{
        self, CardState, ForceIntent, GateSnapshot, TransitionContext, TransitionOutcome,
    };

    let card_id = card_id.to_string();
    let new_status = new_status.to_string();
    match run_async_bridge_pg(pool, move |pool| async move {
        let mut tx = pool
            .begin()
            .await
            .map_err(|error| format!("open postgres kanban status transaction: {error}"))?;

        // #3603: `set_status_raw_pg` (the JS `agentdesk.kanban.setStatus`
        // bridge) now delegates transition validation to the FSM reducer
        // `decide_pipeline_transition` (transition.rs), mirroring the canonical
        // intent path `transition_core::transition_status_with_opts_pg_inner`
        // 1:1. The previous body partially re-implemented the reducer and
        // diverged: it only checked `has_active_dispatch`/`review_verdict_pass`
        // (fail-open on `review_verdict_rework`, unwired/unknown gate types) and
        // it never enforced ForceOnly-non-force / no-transition-rule /
        // invalid-target guards — it issued a direct UPDATE instead. Collecting
        // the full `GateSnapshot` here and routing through the reducer removes
        // that whole divergence class. The PG transaction stays open across the
        // load → decide → execute steps; pipeline resolution uses the tx-bound
        // resolver (`resolve_pipeline_on_pg_tx`) to avoid the cross-pool
        // deadlock fixed in #1342.
        let row = sqlx::query(
            "SELECT status, review_status, latest_dispatch_id, repo_id, assigned_agent_id, review_entered_at::text AS review_entered_at
             FROM kanban_cards
             WHERE id = $1",
        )
        .bind(&card_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|error| format!("load postgres kanban card {card_id}: {error}"))?
        .ok_or_else(|| "card not found".to_string())?;

        let old_status: String = row
            .try_get("status")
            .map_err(|error| format!("decode old status for {card_id}: {error}"))?;
        let review_status: Option<String> = row
            .try_get("review_status")
            .map_err(|error| format!("decode review_status for {card_id}: {error}"))?;
        let latest_dispatch_id: Option<String> = row
            .try_get("latest_dispatch_id")
            .map_err(|error| format!("decode latest_dispatch_id for {card_id}: {error}"))?;
        let repo_id: Option<String> = row
            .try_get("repo_id")
            .map_err(|error| format!("decode repo_id for {card_id}: {error}"))?;
        let assigned_agent_id: Option<String> = row
            .try_get("assigned_agent_id")
            .map_err(|error| format!("decode assigned_agent_id for {card_id}: {error}"))?;
        let review_entered_at: Option<String> = row
            .try_get("review_entered_at")
            .map_err(|error| format!("decode review_entered_at for {card_id}: {error}"))?;

        let effective =
            resolve_pipeline_on_pg_tx(&mut tx, repo_id.as_deref(), assigned_agent_id.as_deref())
                .await?;

        if old_status == new_status {
            // Mirror the reference no-op: a forced same-status call still
            // validates that the target exists in the effective pipeline.
            if force && !effective.is_valid_state(&new_status) {
                return Err(format!(
                    "target status '{new_status}' is not defined in the effective pipeline"
                ));
            }
            return Ok(serde_json::json!({
                "ok": true,
                "changed": false,
                "status": new_status,
            }));
        }

        // Gate-input collection — identical semantics to the reference path
        // (transition_core.rs). The `has_active_dispatch` gate counts a
        // completed implementation/rework dispatch as "active" only when the
        // transition is a review-enter whose rule actually carries a
        // `has_active_dispatch` gate (completed work satisfies the gate).
        let transition_rule = effective.find_transition(&old_status, &new_status);
        let is_review_enter = enters_review_state(&effective, &new_status);
        let active_gate_allows_completed_work = is_review_enter
            && transition_rule.is_some_and(|transition| {
                transition.gates.iter().any(|gate_name| {
                    effective
                        .gates
                        .get(gate_name.as_str())
                        .is_some_and(|gate| gate.check.as_deref() == Some("has_active_dispatch"))
                })
            });

        let has_active_dispatch = sqlx::query_scalar::<_, bool>(
            "SELECT COUNT(*) > 0
             FROM task_dispatches
             WHERE kanban_card_id = $1
               AND (
                    status IN ('pending', 'dispatched')
                    OR (
                        $2::text IS NOT NULL
                        AND id = $2::text
                        AND status = 'completed'
                        AND dispatch_type IN ('implementation', 'rework')
                        AND $3::boolean
                    )
               )",
        )
        .bind(&card_id)
        .bind(latest_dispatch_id.as_deref())
        .bind(active_gate_allows_completed_work)
        .fetch_one(&mut *tx)
        .await
        .map_err(|error| format!("load active dispatch gate for {card_id}: {error}"))?;

        // Window the latest review verdict to the current round (verdicts
        // stamped at/after `review_entered_at`). Collect BOTH pass and rework
        // so the reducer can evaluate either gate (the old body only looked at
        // `pass`, and only when the target was terminal — that is the
        // `review_verdict_rework` fail-open #3603 R1/R2 removes). When
        // `review_entered_at` is NULL the `$2 IS NOT NULL` predicate yields no
        // row, so both flags are false (fail-closed), matching the intent path.
        let latest_review_verdict = sqlx::query_scalar::<_, Option<String>>(
            "SELECT result::jsonb ->> 'verdict'
             FROM task_dispatches
             WHERE kanban_card_id = $1
               AND dispatch_type = 'review'
               AND status = 'completed'
               AND $2::timestamptz IS NOT NULL
               AND COALESCE(completed_at, updated_at) >= $2::timestamptz
             ORDER BY COALESCE(completed_at, updated_at) DESC, id DESC
             LIMIT 1",
        )
        .bind(&card_id)
        .bind(review_entered_at.as_deref())
        .fetch_optional(&mut *tx)
        .await
        .map_err(|error| format!("load latest review verdict for {card_id}: {error}"))?
        .flatten();

        let ctx = TransitionContext {
            card: CardState {
                id: card_id.clone(),
                status: old_status.clone(),
                review_status: review_status.clone(),
                latest_dispatch_id: latest_dispatch_id.clone(),
            },
            pipeline: effective.clone(),
            gates: GateSnapshot {
                has_active_dispatch,
                review_verdict_pass: matches!(
                    latest_review_verdict.as_deref(),
                    Some("pass") | Some("approved")
                ),
                review_verdict_rework: matches!(
                    latest_review_verdict.as_deref(),
                    Some("rework") | Some("improve") | Some("reject")
                ),
            },
        };

        let force_intent = if force {
            ForceIntent::OperatorOverride
        } else {
            ForceIntent::None
        };
        let decision = transition::decide_status_transition_with_caller(
            &ctx,
            &new_status,
            "kanban::set_status_raw_pg",
            force_intent,
            "kanban::set_status_raw_pg",
        );

        if let TransitionOutcome::Blocked(reason) = &decision.outcome {
            // The JS bridge surfaces a blocked transition as `{ "error": ... }`
            // (the `Err` path below). Audit-log intents from the reducer are
            // dropped here rather than committed: the prior body never wrote an
            // audit row on a blocked call, so we preserve that to keep the diff
            // a pure validation change.
            tracing::warn!(
                "[kanban] Blocked postgres setStatus {} → {} for card {}: {}",
                old_status,
                new_status,
                card_id,
                reason
            );
            return Err(reason.clone());
        }

        if decision.outcome == TransitionOutcome::NoOp {
            return Ok(serde_json::json!({
                "ok": true,
                "changed": false,
                "status": new_status,
            }));
        }

        // Preserve the `auto_queue_review_disabled` skip (a side-effect the
        // intent path does not carry). The reducer has just approved the
        // transition; if this is a non-forced review-enter for a card whose
        // auto-queue review is disabled, skip the move entirely — exactly as
        // before.
        if !force
            && is_review_enter
            && auto_queue_review_disabled_for_card_on_pg(&mut tx, &card_id).await?
        {
            return Ok(serde_json::json!({
                "ok": true,
                "changed": false,
                "status": old_status,
                "skipped": "auto_queue_review_disabled",
            }));
        }

        for intent in &decision.intents {
            crate::engine::transition_executor_pg::execute_pg_transition_intent(&mut tx, intent)
                .await?;
        }

        // Terminal cleanup: route through the canonical helper (matches the
        // reference path), replacing the prior inline raw cancel UPDATE. This
        // cancels live dispatches through `cancel_dispatch_and_reset_auto_queue`
        // (semaphore release, auto_queue reset, dispatch_events audit, outbox,
        // thread teardown) and preserves verdictless review dispatches.
        if effective.is_terminal(&new_status) {
            crate::engine::transition_executor_pg::cancel_live_dispatches_for_terminal_card_pg(
                &mut tx, &card_id,
            )
            .await?;
        }

        tx.commit().await.map_err(|error| {
            format!("commit postgres kanban status update for {card_id}: {error}")
        })?;

        Ok(serde_json::json!({
            "ok": true,
            "changed": true,
            "from": old_status,
            "to": new_status,
            "card_id": card_id,
        }))
    }) {
        Ok(response) => response.to_string(),
        Err(error) => serde_json::json!({ "error": error }).to_string(),
    }
}

fn reopen_raw_pg(pool: &PgPool, card_id: &str, new_status: &str) -> String {
    let card_id = card_id.to_string();
    let new_status = new_status.to_string();
    match run_async_bridge_pg(pool, move |pool| async move {
        let mut tx = pool
            .begin()
            .await
            .map_err(|error| format!("open postgres kanban reopen transaction: {error}"))?;

        let row = sqlx::query(
            "SELECT status, repo_id, assigned_agent_id
             FROM kanban_cards
             WHERE id = $1",
        )
        .bind(&card_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|error| format!("load postgres kanban card {card_id}: {error}"))?
        .ok_or_else(|| "card not found".to_string())?;

        let old_status: String = row
            .try_get("status")
            .map_err(|error| format!("decode old status for {card_id}: {error}"))?;
        let repo_id: Option<String> = row
            .try_get("repo_id")
            .map_err(|error| format!("decode repo_id for {card_id}: {error}"))?;
        let assigned_agent_id: Option<String> = row
            .try_get("assigned_agent_id")
            .map_err(|error| format!("decode assigned_agent_id for {card_id}: {error}"))?;

        let effective =
            resolve_pipeline_on_pg_tx(&mut tx, repo_id.as_deref(), assigned_agent_id.as_deref())
                .await?;

        if !effective.is_terminal(&old_status) {
            return Err(format!(
                "reopen requires terminal card (current: {old_status})"
            ));
        }
        if effective.is_terminal(&new_status) {
            return Err(format!(
                "reopen target must be non-terminal (target: {new_status})"
            ));
        }
        if old_status == new_status {
            return Ok(serde_json::json!({
                "ok": true,
                "changed": false,
                "status": new_status,
            }));
        }

        let clock_extra = match effective.clock_for_state(&new_status) {
            Some(clock) if clock.mode.as_deref() == Some("coalesce") => {
                format!(", {} = COALESCE({}, NOW())", clock.set, clock.set)
            }
            Some(clock) => format!(", {} = NOW()", clock.set),
            None => String::new(),
        };
        let sql = format!(
            "UPDATE kanban_cards SET status = $1, completed_at = NULL, updated_at = NOW(){} WHERE id = $2",
            clock_extra
        );
        sqlx::query(&sql)
            .bind(&new_status)
            .bind(&card_id)
            .execute(&mut *tx)
            .await
            .map_err(|error| format!("update kanban card {card_id} reopen: {error}"))?;

        // Move done auto-queue entries back to dispatched on reopen.
        let entry_ids = sqlx::query_scalar::<_, String>(
            "SELECT id FROM auto_queue_entries
             WHERE kanban_card_id = $1 AND status = 'done'",
        )
        .bind(&card_id)
        .fetch_all(&mut *tx)
        .await
        .map_err(|error| format!("load auto-queue done entries for {card_id}: {error}"))?;
        for entry_id in entry_ids {
            sqlx::query(
                "UPDATE auto_queue_entries
                 SET status = 'dispatched',
                     updated_at = NOW()
                 WHERE id = $1",
            )
            .bind(&entry_id)
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                format!("reset auto-queue entry {entry_id} to dispatched on reopen: {error}")
            })?;
        }

        let has_hooks = effective
            .hooks_for_state(&new_status)
            .is_some_and(|h| !h.on_enter.is_empty() || !h.on_exit.is_empty());
        let is_review_enter = effective
            .hooks_for_state(&new_status)
            .is_some_and(|h| h.on_enter.iter().any(|n| n == "OnReviewEnter"));
        if !has_hooks {
            crate::github::sync::sync_review_state_on_pg(&mut tx, &card_id, "idle").await?;
        } else if is_review_enter {
            crate::github::sync::sync_review_state_on_pg(&mut tx, &card_id, "reviewing").await?;
        }

        tx.commit()
            .await
            .map_err(|error| format!("commit postgres kanban reopen for {card_id}: {error}"))?;

        // crate::kanban::correct_tn_to_fn_on_reopen still requires the SQLite Db handle
        // to bridge into the legacy reopen audit path. The PG-only path defers that
        // bookkeeping; a follow-up will port the audit to PG once #839 closes.

        Ok(serde_json::json!({
            "ok": true,
            "changed": true,
            "from": old_status,
            "to": new_status,
            "card_id": card_id,
            "reopened": true,
        }))
    }) {
        Ok(response) => response.to_string(),
        Err(error) => serde_json::json!({ "error": error }).to_string(),
    }
}

fn get_card_raw_pg(pool: &PgPool, card_id: &str) -> String {
    let card_id = card_id.to_string();
    match run_async_bridge_pg(pool, move |pool| async move {
        let row = sqlx::query(
            "SELECT id, status, assigned_agent_id, title, review_status, review_round, latest_dispatch_id
             FROM kanban_cards
             WHERE id = $1",
        )
        .bind(&card_id)
        .fetch_optional(&pool)
        .await
        .map_err(|error| format!("load postgres kanban card {card_id}: {error}"))?;
        let Some(row) = row else {
            return Ok(None);
        };
        Ok(Some(serde_json::json!({
            "id": row.try_get::<String, _>("id").map_err(|error| format!("decode id for {card_id}: {error}"))?,
            "status": row.try_get::<String, _>("status").map_err(|error| format!("decode status for {card_id}: {error}"))?,
            "assigned_agent_id": row.try_get::<Option<String>, _>("assigned_agent_id").map_err(|error| format!("decode assigned_agent_id for {card_id}: {error}"))?,
            "title": row.try_get::<Option<String>, _>("title").map_err(|error| format!("decode title for {card_id}: {error}"))?,
            "review_status": row.try_get::<Option<String>, _>("review_status").map_err(|error| format!("decode review_status for {card_id}: {error}"))?,
            "review_round": row.try_get::<Option<i64>, _>("review_round").map_err(|error| format!("decode review_round for {card_id}: {error}"))?,
            "latest_dispatch_id": row.try_get::<Option<String>, _>("latest_dispatch_id").map_err(|error| format!("decode latest_dispatch_id for {card_id}: {error}"))?,
        })))
    }) {
        Ok(Some(card)) => card.to_string(),
        Ok(None) => r#"{"error":"card not found"}"#.to_string(),
        Err(error) => serde_json::json!({ "error": error }).to_string(),
    }
}

fn clear_latest_dispatch_raw_pg(
    pool: &PgPool,
    card_id: &str,
    expected_dispatch_id: Option<&str>,
) -> String {
    let card_id = card_id.to_string();
    let expected_dispatch_id = expected_dispatch_id.map(str::to_string);
    match run_async_bridge_pg(pool, move |pool| async move {
        let current_latest = sqlx::query_scalar::<_, Option<String>>(
            "SELECT latest_dispatch_id FROM kanban_cards WHERE id = $1",
        )
        .bind(&card_id)
        .fetch_optional(&pool)
        .await
        .map_err(|error| format!("load latest dispatch for {card_id}: {error}"))?
        .flatten();
        if let Some(expected) = expected_dispatch_id.as_deref()
            && current_latest.as_deref() != Some(expected)
        {
            return Ok(serde_json::json!({
                "ok": true,
                "rows_affected": 0,
                "skipped": "latest_mismatch",
            }));
        }

        let rows_affected = sqlx::query(
            "UPDATE kanban_cards
             SET latest_dispatch_id = NULL,
                 updated_at = NOW()
             WHERE id = $1
               AND latest_dispatch_id IS NOT NULL",
        )
        .bind(&card_id)
        .execute(&pool)
        .await
        .map_err(|error| format!("clear latest dispatch for {card_id}: {error}"))?
        .rows_affected();
        Ok(serde_json::json!({
            "ok": true,
            "rows_affected": rows_affected,
        }))
    }) {
        Ok(response) => response.to_string(),
        Err(error) => serde_json::json!({ "error": error }).to_string(),
    }
}

fn set_review_status_raw_pg(pool: &PgPool, card_id: &str, opts_json: &str) -> String {
    let card_id = card_id.to_string();
    let opts: serde_json::Value = match serde_json::from_str(opts_json) {
        Ok(value) => value,
        Err(error) => return format!(r#"{{"error":"bad opts: {}"}}"#, error),
    };

    match run_async_bridge_pg(pool, move |pool| async move {
        let mut tx = pool
            .begin()
            .await
            .map_err(|error| format!("open postgres review status transaction: {error}"))?;

        let mut query = QueryBuilder::<Postgres>::new("UPDATE kanban_cards SET updated_at = NOW()");
        if let Some(review_status) = opts.get("review_status") {
            if review_status.is_null() {
                query.push(", review_status = NULL");
            } else if let Some(status) = review_status.as_str() {
                query.push(", review_status = ");
                query.push_bind(status.to_string());
            }
        }
        if let Some(value) = opts.get("suggestion_pending_at") {
            if value.is_null() {
                query.push(", suggestion_pending_at = NULL");
            } else if value.as_str() == Some("now") {
                query.push(", suggestion_pending_at = NOW()");
            }
        }
        if let Some(value) = opts.get("review_entered_at") {
            if value.is_null() {
                query.push(", review_entered_at = NULL");
            } else if value.as_str() == Some("now") {
                query.push(", review_entered_at = NOW()");
            }
        }
        if let Some(value) = opts.get("awaiting_dod_at") {
            if value.is_null() {
                query.push(", awaiting_dod_at = NULL");
            } else if value.as_str() == Some("now") {
                query.push(", awaiting_dod_at = NOW()");
            }
        }
        if let Some(value) = opts.get("blocked_reason") {
            if value.is_null() {
                query.push(", blocked_reason = NULL");
            } else if let Some(reason) = value.as_str() {
                query.push(", blocked_reason = ");
                query.push_bind(reason.to_string());
            }
        }

        query.push(" WHERE id = ");
        query.push_bind(card_id.clone());
        if let Some(exclude_status) = opts.get("exclude_status").and_then(|value| value.as_str()) {
            query.push(" AND status != ");
            query.push_bind(exclude_status.to_string());
        }
        query
            .build()
            .execute(&mut *tx)
            .await
            .map_err(|error| format!("update review status for {card_id}: {error}"))?;

        if let Some(review_status) = opts.get("review_status") {
            let review_state = if review_status.is_null() {
                Some("idle")
            } else {
                review_status.as_str()
            };
            if let Some(review_state) = review_state {
                crate::github::sync::sync_review_state_on_pg(&mut tx, &card_id, review_state)
                    .await?;
            }
        }

        tx.commit().await.map_err(|error| {
            format!("commit postgres review status update for {card_id}: {error}")
        })?;
        Ok(serde_json::json!({ "ok": true }))
    }) {
        Ok(response) => response.to_string(),
        Err(error) => serde_json::json!({ "error": error }).to_string(),
    }
}

/// Resolve the effective pipeline for a card while a write transaction is
/// open on `tx`, reusing the transaction's connection so the caller does not
/// need to release `tx` first.
///
/// `set_status_raw_pg` / `reopen_raw_pg` hold a write transaction across the
/// pipeline resolution, and the JS bridge runs against the per-call PG pool
/// produced by `crate::utils::async_bridge::run_pg_bridge_thread`, whose
/// `max_connections` is inherited from the source pool. In tests the source
/// pool is `TEST_POSTGRES_POOL_MAX_CONNECTIONS = 1`, so attempting to acquire
/// a second connection while `tx` is still open deadlocks via
/// `acquire_timeout`. Reusing the transaction's connection avoids that nested
/// acquire entirely. (#1342 ci-red follow-up; #1329)
async fn resolve_pipeline_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    repo_id: Option<&str>,
    agent_id: Option<&str>,
) -> Result<crate::pipeline::PipelineConfig, String> {
    crate::pipeline::ensure_loaded();

    let repo_override = if let Some(repo_id) = repo_id {
        sqlx::query_scalar::<_, Option<String>>(
            "SELECT pipeline_config FROM github_repos WHERE id = $1",
        )
        .bind(repo_id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(|error| format!("load repo pipeline override for {repo_id}: {error}"))?
        .flatten()
        .map(|json| crate::pipeline::parse_override(&json))
        .transpose()
        .map_err(|error| format!("parse repo pipeline override for {repo_id}: {error}"))?
        .flatten()
    } else {
        None
    };

    let agent_override = if let Some(agent_id) = agent_id {
        sqlx::query_scalar::<_, Option<String>>("SELECT pipeline_config FROM agents WHERE id = $1")
            .bind(agent_id)
            .fetch_optional(&mut **tx)
            .await
            .map_err(|error| format!("load agent pipeline override for {agent_id}: {error}"))?
            .flatten()
            .map(|json| crate::pipeline::parse_override(&json))
            .transpose()
            .map_err(|error| format!("parse agent pipeline override for {agent_id}: {error}"))?
            .flatten()
    } else {
        None
    };

    Ok(crate::pipeline::resolve(
        repo_override.as_ref(),
        agent_override.as_ref(),
    ))
}

fn run_async_bridge_pg<F, T>(
    pool: &PgPool,
    future_factory: impl FnOnce(PgPool) -> F + Send + 'static,
) -> Result<T, String>
where
    F: std::future::Future<Output = Result<T, String>> + Send + 'static,
    T: Send + 'static,
{
    crate::utils::async_bridge::block_on_pg_result(pool, future_factory, |error| error)
}

pub(super) fn review_state_sync_with_backends(
    db: Option<&Db>,
    pg_pool: Option<&PgPool>,
    json_str: &str,
) -> String {
    if let Some(pool) = pg_pool {
        return review_state_sync_pg(pool, json_str);
    }
    let _ = db;
    r#"{"error":"postgres backend is required for review_state_sync"}"#.to_string()
}

pub(super) fn review_state_sync_pg(pool: &PgPool, json_str: &str) -> String {
    let params: serde_json::Value = match serde_json::from_str(json_str) {
        Ok(v) => v,
        Err(e) => return format!(r#"{{"error":"invalid JSON: {}"}}"#, e),
    };

    let card_id = params["card_id"].as_str().unwrap_or("");
    let state = params["state"].as_str().unwrap_or("");
    if card_id.is_empty() || state.is_empty() {
        return r#"{"error":"card_id and state are required"}"#.to_string();
    }

    let card_id = card_id.to_string();
    let state = state.to_string();
    let review_round = params["review_round"].as_i64();
    let last_verdict = params["last_verdict"].as_str().map(str::to_string);
    let last_decision = params["last_decision"].as_str().map(str::to_string);
    let pending_dispatch_id = params["pending_dispatch_id"].as_str().map(str::to_string);
    let approach_change_round = params["approach_change_round"].as_i64();
    let session_reset_round = params["session_reset_round"].as_i64();
    let review_entered_at = params["review_entered_at"].as_str().map(str::to_string);

    let result = crate::utils::async_bridge::block_on_pg_result(
        pool,
        move |bridge_pool| async move {
            if state == "clear_verdict" {
                let rows_affected = sqlx::query(
                    "UPDATE card_review_state
                     SET last_verdict = NULL,
                         updated_at = NOW()
                     WHERE card_id = $1",
                )
                .bind(&card_id)
                .execute(&bridge_pool)
                .await
                .map_err(|error| format!("clear postgres review verdict for {card_id}: {error}"))?
                .rows_affected();
                return Ok(format!(r#"{{"ok":true,"rows_affected":{rows_affected}}}"#));
            }

            let rows_affected = sqlx::query(
                "INSERT INTO card_review_state (
                    card_id,
                    state,
                    review_round,
                    last_verdict,
                    last_decision,
                    pending_dispatch_id,
                    approach_change_round,
                    session_reset_round,
                    review_entered_at,
                    updated_at
                 ) VALUES (
                    $1,
                    $2,
                    COALESCE(
                        $3,
                        (SELECT COALESCE(review_round, 0)::BIGINT FROM kanban_cards WHERE id = $1),
                        0
                    ),
                    $4,
                    $5,
                    $6,
                    $7,
                    $8,
                    COALESCE(
                        CASE
                            WHEN $9 = 'now' THEN NOW()
                            ELSE $9::timestamptz
                        END,
                        CASE
                            WHEN $2 = 'reviewing' THEN NOW()
                            ELSE NULL
                        END
                    ),
                    NOW()
                 )
                 ON CONFLICT(card_id) DO UPDATE SET
                    state = EXCLUDED.state,
                    review_round = COALESCE(EXCLUDED.review_round, card_review_state.review_round),
                    last_verdict = COALESCE(EXCLUDED.last_verdict, card_review_state.last_verdict),
                    last_decision = COALESCE(EXCLUDED.last_decision, card_review_state.last_decision),
                    pending_dispatch_id = CASE
                        WHEN EXCLUDED.pending_dispatch_id IS NOT NULL THEN EXCLUDED.pending_dispatch_id
                        WHEN EXCLUDED.state = 'suggestion_pending' THEN card_review_state.pending_dispatch_id
                        ELSE NULL
                    END,
                    approach_change_round = COALESCE(
                        EXCLUDED.approach_change_round,
                        card_review_state.approach_change_round
                    ),
                    session_reset_round = COALESCE(
                        EXCLUDED.session_reset_round,
                        card_review_state.session_reset_round
                    ),
                    review_entered_at = COALESCE(
                        EXCLUDED.review_entered_at,
                        CASE
                            WHEN EXCLUDED.state = 'reviewing' THEN NOW()
                            ELSE card_review_state.review_entered_at
                        END
                    ),
                    updated_at = NOW()",
            )
            .bind(&card_id)
            .bind(&state)
            .bind(review_round)
            .bind(last_verdict)
            .bind(last_decision)
            .bind(pending_dispatch_id)
            .bind(approach_change_round)
            .bind(session_reset_round)
            .bind(review_entered_at)
            .execute(&bridge_pool)
            .await
            .map_err(|error| format!("upsert postgres review state for {card_id}: {error}"))?
            .rows_affected();
            Ok(format!(r#"{{"ok":true,"rows_affected":{rows_affected}}}"#))
        },
        |error| format!(r#"{{"error":"{}"}}"#, error),
    );

    match result {
        Ok(value) => value,
        Err(raw) => crate::engine::ops::ensure_js_error_json(raw),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pg_test_db() -> crate::dispatch::test_support::DispatchPostgresTestDb {
        crate::dispatch::test_support::DispatchPostgresTestDb::create(
            "agentdesk_kanban_set_status",
            "set_status_raw_pg review verdict windowing tests",
        )
        .await
    }

    /// Seed a card sitting in `review`, ready to attempt `review -> done`
    /// (which is gated by `review_verdict_pass` in the default pipeline).
    /// `review_entered_at` is set to `NOW() + offset_secs` so tests can place
    /// the review-round boundary before/after seeded verdicts.
    async fn seed_review_card(pool: &sqlx::PgPool, card_id: &str, entered_offset_secs: i64) {
        sqlx::query(
            "INSERT INTO kanban_cards (id, title, status, review_round, review_entered_at, created_at, updated_at)
             VALUES ($1, 'verdict window test', 'review', 1, NOW() + make_interval(secs => $2::double precision), NOW(), NOW())",
        )
        .bind(card_id)
        .bind(entered_offset_secs as f64)
        .execute(pool)
        .await
        .unwrap_or_else(|err| panic!("seed review card {card_id}: {err}"));
    }

    async fn seed_review_card_null_entered(pool: &sqlx::PgPool, card_id: &str) {
        sqlx::query(
            "INSERT INTO kanban_cards (id, title, status, review_round, review_entered_at, created_at, updated_at)
             VALUES ($1, 'verdict window test', 'review', 1, NULL, NOW(), NOW())",
        )
        .bind(card_id)
        .execute(pool)
        .await
        .unwrap_or_else(|err| panic!("seed review card {card_id}: {err}"));
    }

    /// Seed a completed review dispatch whose verdict is stamped at
    /// `NOW() + completed_offset_secs` (via both `completed_at` and `updated_at`,
    /// matching the `COALESCE(completed_at, updated_at)` window predicate).
    async fn seed_review_verdict(
        pool: &sqlx::PgPool,
        dispatch_id: &str,
        card_id: &str,
        verdict: &str,
        completed_offset_secs: i64,
    ) {
        sqlx::query(
            "INSERT INTO task_dispatches
                (id, kanban_card_id, dispatch_type, status, result, completed_at, updated_at, created_at)
             VALUES ($1, $2, 'review', 'completed', $3,
                     NOW() + make_interval(secs => $4::double precision),
                     NOW() + make_interval(secs => $4::double precision),
                     NOW())",
        )
        .bind(dispatch_id)
        .bind(card_id)
        .bind(format!(r#"{{"verdict":"{verdict}"}}"#))
        .bind(completed_offset_secs as f64)
        .execute(pool)
        .await
        .unwrap_or_else(|err| panic!("seed review verdict {dispatch_id}: {err}"));
    }

    /// True when the response is a `review_verdict_pass` gate block. After the
    /// #3603 delegation the error text is produced by the FSM reducer
    /// (`decide_pipeline_transition` → `evaluate_gates`), which wraps the gate
    /// failure as `… failed gate '<gate>': BLOCKED: no review pass verdict …`.
    fn is_review_pass_block(response: &str) -> bool {
        let value: serde_json::Value =
            serde_json::from_str(response).unwrap_or_else(|err| panic!("parse response: {err}"));
        value
            .get("error")
            .and_then(|e| e.as_str())
            .is_some_and(|e| e.contains("failed gate") && e.contains("review pass verdict"))
    }

    fn is_transition_ok_to(response: &str, expected_status: &str) -> bool {
        let value: serde_json::Value =
            serde_json::from_str(response).unwrap_or_else(|err| panic!("parse response: {err}"));
        value.get("error").is_none()
            && value.get("ok").and_then(|v| v.as_bool()) == Some(true)
            && value.get("changed").and_then(|v| v.as_bool()) == Some(true)
            && value.get("to").and_then(|v| v.as_str()) == Some(expected_status)
    }

    /// True when the JS-bridge response reports an error (Blocked transition).
    fn is_error(response: &str) -> bool {
        let value: serde_json::Value =
            serde_json::from_str(response).unwrap_or_else(|err| panic!("parse response: {err}"));
        value.get("error").is_some()
    }

    /// True when the response is a successful, status-changing transition
    /// (`{ ok: true, changed: true }`), regardless of the target value.
    fn is_changed(response: &str) -> bool {
        let value: serde_json::Value =
            serde_json::from_str(response).unwrap_or_else(|err| panic!("parse response: {err}"));
        value.get("error").is_none() && value.get("changed").and_then(|v| v.as_bool()) == Some(true)
    }

    /// Stale verdict: the only `pass` was stamped BEFORE `review_entered_at`
    /// (previous round). The current round has no verdict, so `review -> done`
    /// must be blocked (fail-closed), matching the canonical reducer window.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn set_status_raw_pg_blocks_stale_pass_verdict_before_review_entered_at() {
        let db = pg_test_db().await;
        let pool = db.connect_and_migrate().await;
        crate::pipeline::ensure_loaded();

        let card_id = "card-stale-pass";
        // review entered "now"; pass verdict 1h earlier -> outside the window.
        seed_review_card(&pool, card_id, 0).await;
        seed_review_verdict(&pool, "disp-stale-pass", card_id, "pass", -3600).await;

        let response = set_status_raw_pg(&pool, card_id, "done", false);
        assert!(
            is_review_pass_block(&response),
            "stale pass before review_entered_at must block review->done, got: {response}"
        );

        pool.close().await;
        db.drop().await;
    }

    /// Fresh verdict: a `pass` stamped AFTER `review_entered_at` (current round)
    /// satisfies the gate, so `review -> done` succeeds.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn set_status_raw_pg_allows_fresh_pass_verdict_after_review_entered_at() {
        let db = pg_test_db().await;
        let pool = db.connect_and_migrate().await;
        crate::pipeline::ensure_loaded();

        let card_id = "card-fresh-pass";
        // review entered 1h ago; pass verdict "now" -> inside the window.
        seed_review_card(&pool, card_id, -3600).await;
        seed_review_verdict(&pool, "disp-fresh-pass", card_id, "pass", 0).await;

        let response = set_status_raw_pg(&pool, card_id, "done", false);
        assert!(
            is_transition_ok_to(&response, "done"),
            "fresh pass after review_entered_at must allow review->done, got: {response}"
        );

        pool.close().await;
        db.drop().await;
    }

    /// NULL `review_entered_at` must behave like the reducer window
    /// (`$2::timestamptz IS NOT NULL` => no row): the gate blocks even when a
    /// historical pass exists.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn set_status_raw_pg_blocks_when_review_entered_at_is_null() {
        let db = pg_test_db().await;
        let pool = db.connect_and_migrate().await;
        crate::pipeline::ensure_loaded();

        let card_id = "card-null-entered";
        seed_review_card_null_entered(&pool, card_id).await;
        seed_review_verdict(&pool, "disp-null-pass", card_id, "pass", -10).await;

        let response = set_status_raw_pg(&pool, card_id, "done", false);
        assert!(
            is_review_pass_block(&response),
            "null review_entered_at must block review->done, got: {response}"
        );

        pool.close().await;
        db.drop().await;
    }

    /// The window applies regardless of verdict value: with a stale `pass`
    /// (previous round) and a fresh `rework` (current round), the in-window
    /// latest verdict is `rework`, so the `review_verdict_pass` gate is not
    /// satisfied and `review -> done` is blocked. This proves the rework path
    /// is windowed identically (the stale pass cannot leak through).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn set_status_raw_pg_windows_rework_over_stale_pass() {
        let db = pg_test_db().await;
        let pool = db.connect_and_migrate().await;
        crate::pipeline::ensure_loaded();

        let card_id = "card-rework-window";
        // review entered 1h ago. Stale pass 2h ago (outside), fresh rework now.
        seed_review_card(&pool, card_id, -3600).await;
        seed_review_verdict(&pool, "disp-stale-pass-2", card_id, "pass", -7200).await;
        seed_review_verdict(&pool, "disp-fresh-rework", card_id, "rework", 0).await;

        let response = set_status_raw_pg(&pool, card_id, "done", false);
        assert!(
            is_review_pass_block(&response),
            "fresh rework over stale pass must block review->done, got: {response}"
        );

        pool.close().await;
        db.drop().await;
    }

    // ── #3603 delegation: matrix + differential equivalence ──────────
    //
    // These tests prove `set_status_raw_pg` (the delegating JS bridge) is
    // semantically equivalent to the canonical intent reference
    // `crate::kanban::transition_status_with_opts_pg_only`
    // (transition_core.rs), and pin the specific divergences #3603 closes.

    use crate::engine::PolicyEngine;
    use crate::engine::transition::ForceIntent;

    /// Seed a card in an arbitrary non-terminal state with no review metadata.
    async fn seed_card(pool: &sqlx::PgPool, card_id: &str, status: &str) {
        sqlx::query(
            "INSERT INTO kanban_cards (id, title, status, created_at, updated_at)
             VALUES ($1, '3603 matrix', $2, NOW(), NOW())",
        )
        .bind(card_id)
        .bind(status)
        .execute(pool)
        .await
        .unwrap_or_else(|err| panic!("seed card {card_id}: {err}"));
    }

    /// Seed an in-flight (pending) dispatch so `has_active_dispatch` is satisfied.
    async fn seed_active_dispatch(pool: &sqlx::PgPool, dispatch_id: &str, card_id: &str) {
        sqlx::query(
            "INSERT INTO task_dispatches
                (id, kanban_card_id, dispatch_type, status, created_at, updated_at)
             VALUES ($1, $2, 'implementation', 'pending', NOW(), NOW())",
        )
        .bind(dispatch_id)
        .bind(card_id)
        .execute(pool)
        .await
        .unwrap_or_else(|err| panic!("seed active dispatch {dispatch_id}: {err}"));
    }

    fn test_engine(pool: &sqlx::PgPool) -> PolicyEngine {
        PolicyEngine::new_with_pg(&crate::config::Config::default(), Some(pool.clone()))
            .expect("build test PolicyEngine")
    }

    /// Run the reference intent path and reduce its `Result<TransitionResult>`
    /// to the same `(changed, is_error)` shape the JS bridge returns, so the two
    /// can be compared directly.
    async fn reference_outcome(
        pool: &sqlx::PgPool,
        engine: &PolicyEngine,
        card_id: &str,
        new_status: &str,
        force: bool,
    ) -> (bool, bool) {
        let force_intent = if force {
            ForceIntent::OperatorOverride
        } else {
            ForceIntent::None
        };
        match crate::kanban::transition_status_with_opts_pg_only(
            pool,
            engine,
            card_id,
            new_status,
            "kanban::set_status_raw_pg",
            force_intent,
        )
        .await
        {
            Ok(result) => (result.changed, false),
            Err(_) => (false, true),
        }
    }

    fn bridge_outcome(response: &str) -> (bool, bool) {
        (is_changed(response), is_error(response))
    }

    /// ★ Differential equivalence. For each cell of the production status ×
    /// target × gate-state × force matrix, seed two IDENTICAL cards and assert
    /// the delegating bridge (`set_status_raw_pg`) and the intent reference
    /// (`transition_status_with_opts_pg_only`) agree on `(changed, is_error)`.
    /// This is the direct proof that the partial re-implementation no longer
    /// diverges from the reducer.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn set_status_raw_pg_matches_intent_reference_across_matrix() {
        let db = pg_test_db().await;
        let pool = db.connect_and_migrate().await;
        crate::pipeline::ensure_loaded();
        let engine = test_engine(&pool);

        // (label, seed-closure, target, force)
        // Each seed closure populates `<id>-b` (bridge) and `<id>-r` (reference)
        // identically so the two paths see the same state.
        struct Case {
            label: &'static str,
            target: &'static str,
            force: bool,
            // active dispatch present?
            active: bool,
            // base status to seed
            status: &'static str,
            // review-round verdict, if any (verdict, in_window)
            verdict: Option<(&'static str, bool)>,
        }

        let cases = [
            // requested → in_progress (gated: active_dispatch)
            Case {
                label: "requested→in_progress, active, !force",
                target: "in_progress",
                force: false,
                active: true,
                status: "requested",
                verdict: None,
            },
            Case {
                label: "requested→in_progress, no-active, !force",
                target: "in_progress",
                force: false,
                active: false,
                status: "requested",
                verdict: None,
            },
            Case {
                label: "requested→in_progress, no-active, force",
                target: "in_progress",
                force: true,
                active: false,
                status: "requested",
                verdict: None,
            },
            // in_progress → review (gated: active_dispatch, review-enter)
            Case {
                label: "in_progress→review, active, !force",
                target: "review",
                force: false,
                active: true,
                status: "in_progress",
                verdict: None,
            },
            Case {
                label: "in_progress→review, no-active, !force",
                target: "review",
                force: false,
                active: false,
                status: "in_progress",
                verdict: None,
            },
            // in_progress → requested (NO RULE — #3603 R3)
            Case {
                label: "in_progress→requested, !force (no-rule)",
                target: "requested",
                force: false,
                active: false,
                status: "in_progress",
                verdict: None,
            },
            Case {
                label: "in_progress→requested, force (no-rule bypass)",
                target: "requested",
                force: true,
                active: false,
                status: "in_progress",
                verdict: None,
            },
            // review → done (gated: review_verdict_pass)
            Case {
                label: "review→done, window pass, !force",
                target: "done",
                force: false,
                active: false,
                status: "review",
                verdict: Some(("pass", true)),
            },
            Case {
                label: "review→done, window rework, !force",
                target: "done",
                force: false,
                active: false,
                status: "review",
                verdict: Some(("rework", true)),
            },
            Case {
                label: "review→done, no window verdict, !force",
                target: "done",
                force: false,
                active: false,
                status: "review",
                verdict: None,
            },
            Case {
                label: "review→done, stale pass only, !force",
                target: "done",
                force: false,
                active: false,
                status: "review",
                verdict: Some(("pass", false)),
            },
            Case {
                label: "review→done, no verdict, force",
                target: "done",
                force: true,
                active: false,
                status: "review",
                verdict: None,
            },
            // review → in_progress (gated: review_verdict_rework — #3603 R1)
            Case {
                label: "review→in_progress, window rework, !force",
                target: "in_progress",
                force: false,
                active: false,
                status: "review",
                verdict: Some(("rework", true)),
            },
            Case {
                label: "review→in_progress, window pass, !force",
                target: "in_progress",
                force: false,
                active: false,
                status: "review",
                verdict: Some(("pass", true)),
            },
            Case {
                label: "review→in_progress, no window verdict, !force",
                target: "in_progress",
                force: false,
                active: false,
                status: "review",
                verdict: None,
            },
            Case {
                label: "review→in_progress, no verdict, force",
                target: "in_progress",
                force: true,
                active: false,
                status: "review",
                verdict: None,
            },
        ];

        for (idx, case) in cases.iter().enumerate() {
            for variant in ["b", "r"] {
                let id = format!("m{idx}-{variant}");
                if case.status == "review" {
                    // review_entered_at = now-1h so an in-window verdict is "now"
                    // and a stale verdict is "-2h".
                    seed_review_card(&pool, &id, -3600).await;
                } else {
                    seed_card(&pool, &id, case.status).await;
                }
                if case.active {
                    seed_active_dispatch(&pool, &format!("md{idx}-{variant}"), &id).await;
                }
                if let Some((verdict, in_window)) = case.verdict {
                    let offset = if in_window { 0 } else { -7200 };
                    seed_review_verdict(&pool, &format!("mv{idx}-{variant}"), &id, verdict, offset)
                        .await;
                }
            }

            let bridge_id = format!("m{idx}-b");
            let reference_id = format!("m{idx}-r");
            let bridge = bridge_outcome(&set_status_raw_pg(
                &pool,
                &bridge_id,
                case.target,
                case.force,
            ));
            let reference =
                reference_outcome(&pool, &engine, &reference_id, case.target, case.force).await;

            assert_eq!(
                bridge, reference,
                "DIVERGENCE [{}]: bridge {:?} != reference {:?} (changed, is_error)",
                case.label, bridge, reference
            );
        }

        pool.close().await;
        db.drop().await;
    }

    /// ★ review → in_progress with an in-window `rework` verdict and force=false
    /// is ALLOWED — the rework gate passes, so #3603's hardening does not block
    /// the normal rework recovery flow.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn set_status_raw_pg_allows_review_to_in_progress_with_window_rework() {
        let db = pg_test_db().await;
        let pool = db.connect_and_migrate().await;
        crate::pipeline::ensure_loaded();

        let card_id = "card-rework-allow";
        seed_review_card(&pool, card_id, -3600).await;
        seed_review_verdict(&pool, "disp-rework-allow", card_id, "rework", 0).await;

        let response = set_status_raw_pg(&pool, card_id, "in_progress", false);
        assert!(
            is_transition_ok_to(&response, "in_progress"),
            "in-window rework must allow review->in_progress, got: {response}"
        );

        pool.close().await;
        db.drop().await;
    }

    /// ★ review → in_progress with NO in-window verdict and force=false is
    /// BLOCKED — the `review_verdict_rework` gate fails closed. The OLD partial
    /// re-implementation never evaluated this gate and would have issued a
    /// direct UPDATE (fail-open, #3603 divergence R1).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn set_status_raw_pg_blocks_review_to_in_progress_without_window_verdict() {
        let db = pg_test_db().await;
        let pool = db.connect_and_migrate().await;
        crate::pipeline::ensure_loaded();

        let card_id = "card-rework-block";
        // review entered now; a STALE rework 2h ago is out of window.
        seed_review_card(&pool, card_id, 0).await;
        seed_review_verdict(&pool, "disp-stale-rework", card_id, "rework", -7200).await;

        let response = set_status_raw_pg(&pool, card_id, "in_progress", false);
        assert!(
            is_error(&response),
            "no in-window rework verdict must block review->in_progress, got: {response}"
        );

        pool.close().await;
        db.drop().await;
    }

    /// ★ in_progress → requested with force=false is BLOCKED — there is no
    /// transition rule for it, and the reducer fails closed (#3603 R3). The OLD
    /// body issued a direct UPDATE regardless of rule existence. (Reconciliation
    /// now calls this transition with force=true, so this block is not exposed
    /// on the live recovery path — see reconciliation.js:195.)
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn set_status_raw_pg_blocks_no_rule_in_progress_to_requested_unforced() {
        let db = pg_test_db().await;
        let pool = db.connect_and_migrate().await;
        crate::pipeline::ensure_loaded();

        let card_id = "card-norule-block";
        seed_card(&pool, card_id, "in_progress").await;

        let response = set_status_raw_pg(&pool, card_id, "requested", false);
        assert!(
            is_error(&response),
            "no-rule in_progress->requested must block when unforced, got: {response}"
        );

        pool.close().await;
        db.drop().await;
    }

    /// ★ in_progress → requested with force=true is ALLOWED — the reducer's
    /// no-rule bypass arm carries forced transitions through. This is the
    /// behaviour reconciliation.js:195 relies on after #3603 (force=true).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn set_status_raw_pg_allows_no_rule_in_progress_to_requested_forced() {
        let db = pg_test_db().await;
        let pool = db.connect_and_migrate().await;
        crate::pipeline::ensure_loaded();

        let card_id = "card-norule-allow";
        seed_card(&pool, card_id, "in_progress").await;

        let response = set_status_raw_pg(&pool, card_id, "requested", true);
        assert!(
            is_transition_ok_to(&response, "requested"),
            "forced no-rule in_progress->requested must be allowed, got: {response}"
        );

        pool.close().await;
        db.drop().await;
    }

    /// Normal forward transitions remain un-regressed: requested→in_progress
    /// with an active dispatch is allowed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn set_status_raw_pg_allows_requested_to_in_progress_with_active_dispatch() {
        let db = pg_test_db().await;
        let pool = db.connect_and_migrate().await;
        crate::pipeline::ensure_loaded();

        let card_id = "card-fwd-active";
        seed_card(&pool, card_id, "requested").await;
        seed_active_dispatch(&pool, "disp-fwd-active", card_id).await;

        let response = set_status_raw_pg(&pool, card_id, "in_progress", false);
        assert!(
            is_transition_ok_to(&response, "in_progress"),
            "requested->in_progress with active dispatch must be allowed, got: {response}"
        );

        pool.close().await;
        db.drop().await;
    }
}
