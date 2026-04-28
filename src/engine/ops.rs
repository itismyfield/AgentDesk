//! Bridge operations: Rust functions exposed to JS as `agentdesk.*`.
//!
//! Strategy: register simple Rust callbacks that accept String/i32 args,
//! then create JS wrappers that do the marshaling. This avoids rquickjs
//! lifetime issues with Value<'js> in MutFn closures.

mod agent_ops;
mod auto_queue_ops;
mod cards_ops;
mod ci_recovery_ops;
mod config_ops;
mod db_ops;
mod dispatch_ops;
mod dm_reply_ops;
mod exec_ops;
mod http_ops;
mod kanban_ops;
mod kv_ops;
mod log_ops;
pub(crate) mod message_ops;
mod pipeline_ops;
mod quality_ops;
mod queue_ops;
mod review_automation_ops;
mod review_ops;
mod runtime_ops;

pub(crate) use db_ops::execute_policy_sql;
pub(crate) use review_ops::{ADVANCE_REVIEW_ROUND_HINT_KEY, ensure_js_error_json};

#[cfg(test)]
mod tests;

use crate::db::Db;
use crate::supervisor::BridgeHandle;
use rquickjs::{Ctx, Function, Object, Result as JsResult};

/// Register all `agentdesk.*` globals in the given JS context.
#[cfg_attr(not(test), allow(dead_code))]
pub fn register_globals(ctx: &Ctx<'_>, db: Db) -> JsResult<()> {
    register_globals_with_supervisor_and_pg(ctx, Some(db), None, BridgeHandle::new())
}

#[allow(dead_code)]
pub fn register_globals_with_supervisor(
    ctx: &Ctx<'_>,
    db: Db,
    supervisor_bridge: BridgeHandle,
) -> JsResult<()> {
    register_globals_with_supervisor_and_pg(ctx, Some(db), None, supervisor_bridge)
}

pub fn register_globals_with_supervisor_and_pg(
    ctx: &Ctx<'_>,
    db: Option<Db>,
    pg_pool: Option<sqlx::PgPool>,
    supervisor_bridge: BridgeHandle,
) -> JsResult<()> {
    let globals = ctx.globals();

    let ad = Object::new(ctx.clone())?;

    // ── agentdesk.registerPolicy (placeholder) ───────────────────
    let noop = Function::new(ctx.clone(), || -> JsResult<()> { Ok(()) })?;
    ad.set("registerPolicy", noop)?;

    // Set the global first so JS wrapper code can reference it
    globals.set("agentdesk", ad)?;

    // ── agentdesk.__pendingIntents — intent accumulator for deferred mutations (#121)
    ctx.eval::<(), _>(r#"agentdesk.__pendingIntents = [];"#)?;

    // ── agentdesk.__generateId — UUID v4 generation from Rust
    let gen_id = Function::new(ctx.clone(), || -> String {
        uuid::Uuid::new_v4().to_string()
    })?;
    {
        let ad: Object<'_> = ctx.globals().get("agentdesk")?;
        ad.set("__generateId", gen_id)?;
    }

    // ── agentdesk.db ─────────────────────────────────────────────
    db_ops::register_db_ops(ctx, db.clone(), pg_pool.clone())?;

    // ── agentdesk.cards ──────────────────────────────────────────
    cards_ops::register_card_ops(ctx, db.clone(), pg_pool.clone())?;

    // ── agentdesk.log ────────────────────────────────────────────
    log_ops::register_log_ops(ctx)?;

    // ── agentdesk.quality ───────────────────────────────────────
    quality_ops::register_quality_ops(ctx)?;

    // ── agentdesk.config ─────────────────────────────────────────
    config_ops::register_config_ops(ctx, db.clone(), pg_pool.clone())?;

    // ── agentdesk.http ────────────────────────────────────────────
    http_ops::register_http_ops(ctx)?;

    // ── agentdesk.dispatch ────────────────────────────────────────
    dispatch_ops::register_dispatch_ops(ctx, db.clone(), pg_pool.clone())?;

    // ── agentdesk.kanban ────────────────────────────────────────
    kanban_ops::register_kanban_ops(ctx, db.clone(), pg_pool.clone())?;

    // ── agentdesk.ciRecovery (#1007) ─────────────────────────────
    ci_recovery_ops::register_ci_recovery_ops(ctx, db.clone(), pg_pool.clone())?;

    // ── agentdesk.kv ─────────────────────────────────────────────
    kv_ops::register_kv_ops(ctx, db.clone(), pg_pool.clone())?;

    // ── agentdesk.review ─────────────────────────────────────────
    review_ops::register_review_ops(ctx, db.clone(), pg_pool.clone())?;

    // ── agentdesk.reviewAutomation ─────────────────────────────── #743
    review_automation_ops::register_review_automation_ops(ctx, db.clone(), pg_pool.clone())?;

    // ── agentdesk.queue ──────────────────────────────────────────
    queue_ops::register_queue_ops(ctx, db.clone(), pg_pool.clone())?;

    // ── agentdesk.autoQueue ─────────────────────────────────────
    auto_queue_ops::register_auto_queue_ops(
        ctx,
        db.clone(),
        pg_pool.clone(),
        supervisor_bridge.clone(),
    )?;

    // ── agentdesk.runtime ────────────────────────────────────────
    runtime_ops::register_runtime_ops(ctx, db.clone(), pg_pool.clone(), supervisor_bridge)?;

    // ── agentdesk.message ────────────────────────────────────────
    let db_for_pipeline = db.clone();
    let db_for_dm_reply = db.clone();
    let pg_for_dm_reply = pg_pool.clone();
    let db_for_agents = db.clone();
    message_ops::register_message_ops(ctx, db.clone(), pg_pool.clone())?;

    // ── agentdesk.exec ──────────────────────────────────────────
    exec_ops::register_exec_ops(ctx)?;

    // ── agentdesk.pipeline ────────────────────────────────────────
    pipeline_ops::register_pipeline_ops(ctx, db_for_pipeline, pg_pool.clone())?;

    // ── agentdesk.dmReply ────────────────────────────────────
    dm_reply_ops::register_dm_reply_ops(ctx, db_for_dm_reply, pg_for_dm_reply)?;

    // ── agentdesk.agents ─────────────────────────────────────────
    agent_ops::register_agent_ops(ctx, db_for_agents, pg_pool)?;

    Ok(())
}

/// Rust implementation of card_review_state sync (#158).
/// Single entrypoint for all review-state mutations.
/// Used by both the JS bridge and Rust route handlers.
pub fn review_state_sync(db: &Db, json_str: &str) -> String {
    let conn = match db.separate_conn() {
        Ok(c) => c,
        Err(e) => return format!(r#"{{"error":"db error: {}"}}"#, e),
    };
    review_state_sync_on_conn(&conn, json_str)
}

pub fn review_state_sync_with_backends(
    db: Option<&Db>,
    pg_pool: Option<&sqlx::PgPool>,
    json_str: &str,
) -> String {
    kanban_ops::review_state_sync_with_backends(db, pg_pool, json_str)
}

/// Best-effort auto-queue cleanup for terminal cards.
///
/// When a card finishes, its active dispatch entry should become `done` and any
/// stale pending copies in active or paused runs should be skipped so they do
/// not block other runs.
pub(crate) fn sync_auto_queue_terminal_on_conn(conn: &libsql_rusqlite::Connection, card_id: &str) {
    let dispatched_rows: Vec<String> = conn
        .prepare(
            "SELECT id FROM auto_queue_entries
             WHERE kanban_card_id = ?1 AND status = 'dispatched'",
        )
        .ok()
        .and_then(|mut stmt| {
            stmt.query_map([card_id], |row| row.get(0))
                .ok()
                .map(|rows| rows.filter_map(|row| row.ok()).collect())
        })
        .unwrap_or_default();
    for entry_id in dispatched_rows {
        if let Err(error) = crate::db::auto_queue::update_entry_status_on_conn(
            conn,
            &entry_id,
            crate::db::auto_queue::ENTRY_STATUS_DONE,
            "card_terminal",
            &crate::db::auto_queue::EntryStatusUpdateOptions::default(),
        ) {
            tracing::warn!(
                "[auto-queue] failed to mark entry {} done during terminal sync: {}",
                entry_id,
                error
            );
        }
    }

    let pending_rows: Vec<String> = conn
        .prepare(
            "SELECT id FROM auto_queue_entries
             WHERE kanban_card_id = ?1
               AND status = 'pending'
               AND run_id IN (
                   SELECT id FROM auto_queue_runs WHERE status IN ('active', 'paused')
               )",
        )
        .ok()
        .and_then(|mut stmt| {
            stmt.query_map([card_id], |row| row.get(0))
                .ok()
                .map(|rows| rows.filter_map(|row| row.ok()).collect())
        })
        .unwrap_or_default();
    for entry_id in pending_rows {
        if let Err(error) = crate::db::auto_queue::update_entry_status_on_conn(
            conn,
            &entry_id,
            crate::db::auto_queue::ENTRY_STATUS_SKIPPED,
            "card_terminal_pending_cleanup",
            &crate::db::auto_queue::EntryStatusUpdateOptions::default(),
        ) {
            tracing::warn!(
                "[auto-queue] failed to skip pending entry {} during terminal sync: {}",
                entry_id,
                error
            );
        }
    }
}

/// Skip live auto-queue entries for a card after PMD explicitly backs the card out.
///
/// Only active/paused runs are touched. Generated or future runs stay intact so
/// PMD can intentionally re-queue the card later after fixing prerequisites.
pub(crate) fn skip_live_auto_queue_entries_for_card_on_conn(
    conn: &libsql_rusqlite::Connection,
    card_id: &str,
) -> libsql_rusqlite::Result<usize> {
    let mut stmt = conn.prepare(
        "SELECT id FROM auto_queue_entries
         WHERE kanban_card_id = ?1
           AND status IN ('pending', 'dispatched')
           AND run_id IN (SELECT id FROM auto_queue_runs WHERE status IN ('active', 'paused'))",
    )?;
    let entry_ids: Vec<String> = stmt
        .query_map([card_id], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    drop(stmt);

    let mut changed = 0usize;
    for entry_id in entry_ids {
        if crate::db::auto_queue::update_entry_status_on_conn(
            conn,
            &entry_id,
            crate::db::auto_queue::ENTRY_STATUS_SKIPPED,
            "force_transition_cleanup",
            &crate::db::auto_queue::EntryStatusUpdateOptions::default(),
        )
        .map_err(|error| match error {
            crate::db::auto_queue::EntryStatusUpdateError::Sql(sql) => sql,
            other => libsql_rusqlite::Error::ToSqlConversionFailure(Box::new(
                std::io::Error::other(other.to_string()),
            )),
        })?
        .changed
        {
            changed += 1;
        }
    }

    Ok(changed)
}

/// Same as `review_state_sync` but operates on an already-acquired connection.
/// Use this inside transactions or when a lock is already held (#158).
pub fn review_state_sync_on_conn(conn: &libsql_rusqlite::Connection, json_str: &str) -> String {
    let params: serde_json::Value = match serde_json::from_str(json_str) {
        Ok(v) => v,
        Err(e) => return format!(r#"{{"error":"invalid JSON: {}"}}"#, e),
    };

    let card_id = params["card_id"].as_str().unwrap_or("");
    let state = params["state"].as_str().unwrap_or("");
    if card_id.is_empty() || state.is_empty() {
        return r#"{"error":"card_id and state are required"}"#.to_string();
    }

    // Special case: clear_verdict only NULLs last_verdict without changing state
    if state == "clear_verdict" {
        let result = conn.execute(
            "UPDATE card_review_state SET last_verdict = NULL, updated_at = datetime('now') WHERE card_id = ?1",
            libsql_rusqlite::params![card_id],
        );
        return match result {
            Ok(n) => format!(r#"{{"ok":true,"rows_affected":{n}}}"#),
            Err(e) => format!(r#"{{"error":"sql error: {}"}}"#, e),
        };
    }

    // Build dynamic SET clause based on provided fields
    let review_round = params["review_round"].as_i64();
    let last_verdict = params["last_verdict"].as_str();
    let last_decision = params["last_decision"].as_str();
    let pending_dispatch_id = params["pending_dispatch_id"].as_str();
    let approach_change_round = params["approach_change_round"].as_i64();
    let session_reset_round = params["session_reset_round"].as_i64();
    let review_entered_at = params["review_entered_at"].as_str();

    // UPSERT: INSERT OR REPLACE with all fields
    let result = conn.execute(
        "INSERT INTO card_review_state (card_id, state, review_round, last_verdict, last_decision, pending_dispatch_id, approach_change_round, session_reset_round, review_entered_at, updated_at) \
         VALUES (?1, ?2, COALESCE(?3, (SELECT COALESCE(review_round, 0) FROM kanban_cards WHERE id = ?1), 0), ?4, ?5, ?6, ?7, ?8, COALESCE(?9, CASE WHEN ?2 = 'reviewing' THEN datetime('now') ELSE NULL END), datetime('now')) \
         ON CONFLICT(card_id) DO UPDATE SET \
         state = ?2, \
         review_round = COALESCE(?3, (SELECT COALESCE(review_round, 0) FROM kanban_cards WHERE id = ?1), review_round), \
         last_verdict = COALESCE(?4, last_verdict), \
         last_decision = COALESCE(?5, last_decision), \
         pending_dispatch_id = CASE \
             WHEN ?6 IS NOT NULL THEN ?6 \
             WHEN ?2 = 'suggestion_pending' THEN pending_dispatch_id \
             ELSE NULL \
         END, \
         approach_change_round = COALESCE(?7, approach_change_round), \
         session_reset_round = COALESCE(?8, session_reset_round), \
         review_entered_at = COALESCE(?9, CASE WHEN ?2 = 'reviewing' THEN datetime('now') ELSE review_entered_at END), \
         updated_at = datetime('now')",
        libsql_rusqlite::params![
            card_id,
            state,
            review_round,
            last_verdict,
            last_decision,
            pending_dispatch_id,
            approach_change_round,
            session_reset_round,
            review_entered_at,
        ],
    );

    match result {
        Ok(n) => format!(r#"{{"ok":true,"rows_affected":{n}}}"#),
        Err(e) => format!(r#"{{"error":"sql error: {}"}}"#, e),
    }
}
