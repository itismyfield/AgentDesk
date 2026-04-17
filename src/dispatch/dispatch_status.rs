use anyhow::Result;
use rusqlite::OptionalExtension;
use serde_json::json;

use crate::db::Db;
use crate::engine::PolicyEngine;

use super::dispatch_context::validate_dispatch_completion_evidence_on_conn;
use super::query_dispatch_row;

/// Ensure a durable notify outbox row exists for a dispatch.
///
/// Used both by the authoritative dispatch creation transaction and by
/// fallback/backfill paths that must avoid duplicate notify entries.
pub(crate) fn ensure_dispatch_notify_outbox_on_conn(
    conn: &rusqlite::Connection,
    dispatch_id: &str,
    agent_id: &str,
    card_id: &str,
    title: &str,
) -> rusqlite::Result<bool> {
    conn.execute_batch("SAVEPOINT dispatch_notify_outbox")?;
    let result = (|| -> rusqlite::Result<bool> {
        let dispatch_status: Option<String> = conn
            .query_row(
                "SELECT status FROM task_dispatches WHERE id = ?1",
                [dispatch_id],
                |row| row.get(0),
            )
            .optional()?;
        if matches!(
            dispatch_status.as_deref(),
            Some("completed") | Some("failed") | Some("cancelled")
        ) {
            return Ok(false);
        }

        let inserted = conn.execute(
            "INSERT OR IGNORE INTO dispatch_outbox (dispatch_id, action, agent_id, card_id, title) \
             VALUES (?1, 'notify', ?2, ?3, ?4)",
            rusqlite::params![dispatch_id, agent_id, card_id, title],
        )?;
        Ok(inserted > 0)
    })();
    match result {
        Ok(value) => {
            conn.execute_batch("RELEASE dispatch_notify_outbox")?;
            Ok(value)
        }
        Err(err) => {
            let _ = conn.execute_batch(
                "ROLLBACK TO dispatch_notify_outbox; RELEASE dispatch_notify_outbox;",
            );
            Err(err)
        }
    }
}

/// Ensure a pending status-reaction outbox row exists for a dispatch.
///
/// At most one in-flight status sync is needed: when the worker drains it, the
/// Discord side-effect reads the latest dispatch status from `task_dispatches`.
/// Once an older row is already `done` or `failed`, a later transition should
/// enqueue a fresh row.
pub(crate) fn ensure_dispatch_status_reaction_outbox_on_conn(
    conn: &rusqlite::Connection,
    dispatch_id: &str,
) -> rusqlite::Result<bool> {
    let exists: bool = conn.query_row(
        "SELECT COUNT(*) > 0
         FROM dispatch_outbox
         WHERE dispatch_id = ?1
           AND action = 'status_reaction'
           AND status IN ('pending', 'processing')",
        [dispatch_id],
        |row| row.get(0),
    )?;
    if exists {
        return Ok(false);
    }

    conn.execute(
        "INSERT INTO dispatch_outbox (dispatch_id, action) VALUES (?1, 'status_reaction')",
        [dispatch_id],
    )?;
    Ok(true)
}

pub(crate) fn record_dispatch_status_event_on_conn(
    conn: &rusqlite::Connection,
    dispatch_id: &str,
    from_status: Option<&str>,
    to_status: &str,
    transition_source: &str,
    payload: Option<&serde_json::Value>,
) -> rusqlite::Result<()> {
    let (kanban_card_id, dispatch_type): (Option<String>, Option<String>) = conn
        .query_row(
            "SELECT kanban_card_id, dispatch_type FROM task_dispatches WHERE id = ?1",
            [dispatch_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?
        .unwrap_or((None, None));

    conn.execute(
        "INSERT INTO dispatch_events (
            dispatch_id,
            kanban_card_id,
            dispatch_type,
            from_status,
            to_status,
            transition_source,
            payload_json
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            dispatch_id,
            kanban_card_id,
            dispatch_type,
            from_status,
            to_status,
            transition_source,
            payload.map(|value| value.to_string()),
        ],
    )?;
    Ok(())
}

pub(crate) fn set_dispatch_status_on_conn(
    conn: &rusqlite::Connection,
    dispatch_id: &str,
    to_status: &str,
    result: Option<&serde_json::Value>,
    transition_source: &str,
    allowed_from: Option<&[&str]>,
    touch_completed_at: bool,
) -> Result<usize> {
    let current_status: Option<String> = conn
        .query_row(
            "SELECT status FROM task_dispatches WHERE id = ?1",
            [dispatch_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(current_status) = current_status else {
        return Ok(0);
    };

    if let Some(allowed_from) = allowed_from {
        if !allowed_from
            .iter()
            .any(|status| *status == current_status.as_str())
        {
            return Ok(0);
        }
    }

    conn.execute_batch("SAVEPOINT dispatch_status_transition")?;
    let update_result = (|| -> Result<usize> {
        let changed = match (result, touch_completed_at) {
            (Some(result), true) => conn.execute(
                "UPDATE task_dispatches
                 SET status = ?1,
                     result = ?2,
                     updated_at = datetime('now'),
                     completed_at = CASE
                         WHEN ?1 = 'completed' THEN COALESCE(completed_at, datetime('now'))
                         ELSE completed_at
                     END
                 WHERE id = ?3 AND status = ?4",
                rusqlite::params![to_status, result.to_string(), dispatch_id, current_status],
            )?,
            (Some(result), false) => conn.execute(
                "UPDATE task_dispatches
                 SET status = ?1,
                     result = ?2,
                     updated_at = datetime('now')
                 WHERE id = ?3 AND status = ?4",
                rusqlite::params![to_status, result.to_string(), dispatch_id, current_status],
            )?,
            (None, true) => conn.execute(
                "UPDATE task_dispatches
                 SET status = ?1,
                     updated_at = datetime('now'),
                     completed_at = CASE
                         WHEN ?1 = 'completed' THEN COALESCE(completed_at, datetime('now'))
                         ELSE completed_at
                     END
                 WHERE id = ?2 AND status = ?3",
                rusqlite::params![to_status, dispatch_id, current_status],
            )?,
            (None, false) => conn.execute(
                "UPDATE task_dispatches
                 SET status = ?1,
                     updated_at = datetime('now')
                 WHERE id = ?2 AND status = ?3",
                rusqlite::params![to_status, dispatch_id, current_status],
            )?,
        };

        if changed > 0 && current_status != to_status {
            record_dispatch_status_event_on_conn(
                conn,
                dispatch_id,
                Some(current_status.as_str()),
                to_status,
                transition_source,
                result,
            )?;

            if matches!(
                to_status,
                "dispatched" | "completed" | "failed" | "cancelled"
            ) {
                ensure_dispatch_status_reaction_outbox_on_conn(conn, dispatch_id)?;
            }
        }
        Ok(changed)
    })();

    match update_result {
        Ok(changed) => {
            conn.execute_batch("RELEASE dispatch_status_transition")?;
            Ok(changed)
        }
        Err(err) => {
            let _ = conn.execute_batch(
                "ROLLBACK TO dispatch_status_transition;
                 RELEASE dispatch_status_transition;",
            );
            Err(err)
        }
    }
}

/// Single authority for dispatch completion.
///
/// All dispatch completion paths — turn_bridge explicit, recovery, API PATCH,
/// session idle — MUST route through this function.  It performs:
///   1. DB status update  (task_dispatches → completed)
///   2. OnDispatchCompleted hook firing  (pipeline event hooks)
///   3. Side-effect draining  (intents, transitions, follow-up dispatches)
///   4. Safety-net re-fire of OnReviewEnter (#139)
pub fn finalize_dispatch(
    db: &Db,
    engine: &PolicyEngine,
    dispatch_id: &str,
    completion_source: &str,
    context: Option<&serde_json::Value>,
) -> Result<serde_json::Value> {
    let result = match context {
        Some(ctx) => {
            let mut merged = ctx.clone();
            if let Some(obj) = merged.as_object_mut() {
                obj.insert(
                    "completion_source".to_string(),
                    serde_json::Value::String(completion_source.to_string()),
                );
            }
            merged
        }
        None => json!({ "completion_source": completion_source }),
    };
    complete_dispatch_inner(db, engine, dispatch_id, &result)
}

/// #143: DB-only dispatch completion — marks status='completed' without firing hooks.
///
/// Used by specialized paths (review_verdict, pm-decision) that fire their own
/// domain-specific hooks instead of the generic OnDispatchCompleted.
/// Returns the number of rows updated (0 = already completed/cancelled/not found).
pub fn mark_dispatch_completed(
    db: &Db,
    dispatch_id: &str,
    result: &serde_json::Value,
) -> Result<usize> {
    let conn = db
        .separate_conn()
        .map_err(|e| anyhow::anyhow!("DB conn error: {e}"))?;
    let changed = set_dispatch_status_on_conn(
        &conn,
        dispatch_id,
        "completed",
        Some(result),
        "mark_dispatch_completed",
        Some(&["pending", "dispatched"]),
        true,
    )?;
    Ok(changed)
}

/// Legacy wrapper — delegates to [`finalize_dispatch`] for callers that already
/// have a fully-formed result JSON (e.g. API PATCH handler).
#[cfg_attr(not(test), allow(dead_code))]
pub fn complete_dispatch(
    db: &Db,
    engine: &PolicyEngine,
    dispatch_id: &str,
    result: &serde_json::Value,
) -> Result<serde_json::Value> {
    complete_dispatch_inner(db, engine, dispatch_id, result)
}

fn complete_dispatch_inner(
    db: &Db,
    engine: &PolicyEngine,
    dispatch_id: &str,
    result: &serde_json::Value,
) -> Result<serde_json::Value> {
    let dispatch_span =
        crate::logging::dispatch_span("complete_dispatch", Some(dispatch_id), None, None);
    let _guard = dispatch_span.enter();
    let conn = db
        .separate_conn()
        .map_err(|e| anyhow::anyhow!("DB lock error: {e}"))?;

    validate_dispatch_completion_evidence_on_conn(&conn, dispatch_id, result)?;

    // #699: phase-gate callers occasionally omit `verdict` even when every
    // declared `checks.*` entry passed. Auto-queue then reads the missing
    // verdict as failure and pauses the run. Inject `verdict = pass_verdict`
    // defensively so the run can progress.
    let result_owned = maybe_inject_phase_gate_verdict(&conn, dispatch_id, result);
    let result_ref = result_owned.as_ref().unwrap_or(result);

    let changed = set_dispatch_status_on_conn(
        &conn,
        dispatch_id,
        "completed",
        Some(result_ref),
        result_ref
            .get("completion_source")
            .and_then(|value| value.as_str())
            .unwrap_or("complete_dispatch"),
        Some(&["pending", "dispatched"]),
        true,
    )?;

    if changed == 0 {
        let exists: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM task_dispatches WHERE id = ?1",
                [dispatch_id],
                |row| row.get(0),
            )
            .unwrap_or(false);
        if exists {
            tracing::info!("skipping completion hooks because dispatch is already finalized");
            let dispatch = query_dispatch_row(&conn, dispatch_id)?;
            drop(conn);
            return Ok(dispatch);
        }
        return Err(anyhow::anyhow!("Dispatch not found: {dispatch_id}"));
    }

    let dispatch = query_dispatch_row(&conn, dispatch_id)?;

    let kanban_card_id: Option<String> = conn
        .query_row(
            "SELECT kanban_card_id FROM task_dispatches WHERE id = ?1",
            [dispatch_id],
            |row| row.get(0),
        )
        .ok();

    let _old_status: String = kanban_card_id
        .as_ref()
        .and_then(|cid| {
            conn.query_row(
                "SELECT status FROM kanban_cards WHERE id = ?1",
                [cid],
                |row| row.get(0),
            )
            .ok()
        })
        .unwrap_or_default();

    drop(conn);

    crate::kanban::fire_event_hooks(
        db,
        engine,
        "on_dispatch_completed",
        "OnDispatchCompleted",
        json!({
            "dispatch_id": dispatch_id,
            "kanban_card_id": kanban_card_id,
            "result": result,
        }),
    );

    crate::kanban::drain_hook_side_effects(db, engine);

    {
        let needs_review_dispatch = db
            .lock()
            .ok()
            .map(|conn| {
                let (card_status, repo_id, agent_id): (Option<String>, Option<String>, Option<String>) = conn
                    .query_row(
                        "SELECT status, repo_id, assigned_agent_id FROM kanban_cards WHERE id = ?1",
                        [&kanban_card_id],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                    )
                    .unwrap_or((None, None, None));
                let has_review_dispatch: bool = conn
                    .query_row(
                        "SELECT COUNT(*) > 0 FROM task_dispatches \
                         WHERE kanban_card_id = ?1 AND dispatch_type IN ('review', 'review-decision') \
                         AND status IN ('pending', 'dispatched')",
                        [&kanban_card_id],
                        |row| row.get(0),
                    )
                    .unwrap_or(false);
                let has_active_work: bool = conn
                    .query_row(
                        "SELECT COUNT(*) > 0 FROM task_dispatches \
                         WHERE kanban_card_id = ?1 AND dispatch_type IN ('implementation', 'rework') \
                         AND status IN ('pending', 'dispatched')",
                        [&kanban_card_id],
                        |row| row.get(0),
                    )
                    .unwrap_or(false);
                let is_review_state = card_status.as_deref().map_or(false, |s| {
                    let eff = crate::pipeline::resolve_for_card(&conn, repo_id.as_deref(), agent_id.as_deref());
                    eff.hooks_for_state(s)
                        .map_or(false, |h| h.on_enter.iter().any(|n| n == "OnReviewEnter"))
                });
                is_review_state && !has_review_dispatch && !has_active_work
            })
            .unwrap_or(false);

        if needs_review_dispatch {
            let cid = kanban_card_id.as_deref().unwrap_or("unknown");
            tracing::warn!(
                "[dispatch] Card {} in review-like state but no review dispatch — re-firing OnReviewEnter with blocking lock (#220)",
                cid
            );
            let _ = engine.fire_hook_by_name_blocking("OnReviewEnter", json!({ "card_id": cid }));
            crate::kanban::drain_hook_side_effects(db, engine);
        }
    }

    Ok(dispatch)
}

/// #699: inject `verdict = context.phase_gate.pass_verdict` into a phase-gate
/// dispatch result when every declared `checks.*` entry passed but the caller
/// forgot the explicit verdict field.
///
/// Returns `Some(enriched)` only when an injection happened — callers should
/// fall back to the original `result` otherwise. Never overrides an explicit
/// verdict/decision (even `"fail"`) and never injects when any check is not
/// `pass`.
pub(super) fn maybe_inject_phase_gate_verdict(
    conn: &rusqlite::Connection,
    dispatch_id: &str,
    result: &serde_json::Value,
) -> Option<serde_json::Value> {
    // Only act on phase-gate dispatches.
    let (dispatch_type, context_raw): (Option<String>, Option<String>) = conn
        .query_row(
            "SELECT dispatch_type, context FROM task_dispatches WHERE id = ?1",
            [dispatch_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .ok()?;
    if dispatch_type.as_deref() != Some("phase-gate") {
        return None;
    }

    // Explicit verdict/decision already present — never override, even for
    // explicit "fail" cases.
    let has_verdict = result
        .get("verdict")
        .and_then(|v| v.as_str())
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    let has_decision = result
        .get("decision")
        .and_then(|v| v.as_str())
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    if has_verdict || has_decision {
        return None;
    }

    // Require a `checks` object with at least one entry, and every entry's
    // `status` (or equivalent) must be "pass".
    let checks = result.get("checks").and_then(|v| v.as_object())?;
    if checks.is_empty() {
        return None;
    }
    for (_name, entry) in checks.iter() {
        if !check_entry_is_pass(entry) {
            return None;
        }
    }

    // Resolve `pass_verdict`: prefer `context.phase_gate.pass_verdict` stored
    // at dispatch creation; fall back to the system default.
    let pass_verdict = context_raw
        .as_deref()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
        .and_then(|ctx| {
            ctx.get("phase_gate")
                .and_then(|pg| pg.get("pass_verdict"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| "phase_gate_passed".to_string());

    let mut enriched = result.clone();
    if !enriched.is_object() {
        enriched = serde_json::Value::Object(serde_json::Map::new());
        if let Some(obj) = enriched.as_object_mut() {
            if let Some(src) = result.as_object() {
                for (k, v) in src.iter() {
                    obj.insert(k.clone(), v.clone());
                }
            }
        }
    }
    if let Some(obj) = enriched.as_object_mut() {
        obj.insert(
            "verdict".to_string(),
            serde_json::Value::String(pass_verdict.clone()),
        );
        obj.insert(
            "verdict_inferred".to_string(),
            serde_json::Value::Bool(true),
        );
    }

    tracing::info!(
        "[dispatch] #699 inferring phase-gate verdict '{}' for dispatch {} (all {} checks passed)",
        pass_verdict,
        dispatch_id,
        checks.len()
    );

    Some(enriched)
}

fn check_entry_is_pass(entry: &serde_json::Value) -> bool {
    // Accept either `{"status": "pass"}` (canonical) or a bare string "pass".
    if let Some(status) = entry.get("status").and_then(|v| v.as_str()) {
        return status.eq_ignore_ascii_case("pass") || status.eq_ignore_ascii_case("passed");
    }
    if let Some(outcome) = entry.get("result").and_then(|v| v.as_str()) {
        return outcome.eq_ignore_ascii_case("pass") || outcome.eq_ignore_ascii_case("passed");
    }
    if let Some(s) = entry.as_str() {
        return s.eq_ignore_ascii_case("pass") || s.eq_ignore_ascii_case("passed");
    }
    false
}
