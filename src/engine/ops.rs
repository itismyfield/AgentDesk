//! Bridge operations: Rust functions exposed to JS as `agentdesk.*`.
//!
//! Strategy: register simple Rust callbacks that accept String/i32 args,
//! then create JS wrappers that do the marshaling. This avoids rquickjs
//! lifetime issues with Value<'js> in MutFn closures.

mod agent_ops;
mod auto_queue_ops;
mod cards_ops;
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
mod queue_ops;
mod review_automation_ops;
mod review_ops;
mod runtime_ops;

pub(crate) use review_ops::ADVANCE_REVIEW_ROUND_HINT_KEY;

#[cfg(test)]
mod tests;

use crate::db::Db;
use crate::supervisor::BridgeHandle;
use rquickjs::{Ctx, Function, Object, Result as JsResult};

/// Register all `agentdesk.*` globals in the given JS context.
#[cfg_attr(not(test), allow(dead_code))]
pub fn register_globals(ctx: &Ctx<'_>, db: Db) -> JsResult<()> {
    register_globals_with_supervisor_and_pg(ctx, db, None, BridgeHandle::new())
}

#[allow(dead_code)]
pub fn register_globals_with_supervisor(
    ctx: &Ctx<'_>,
    db: Db,
    supervisor_bridge: BridgeHandle,
) -> JsResult<()> {
    register_globals_with_supervisor_and_pg(ctx, db, None, supervisor_bridge)
}

pub fn register_globals_with_supervisor_and_pg(
    ctx: &Ctx<'_>,
    db: Db,
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

    // ── agentdesk.config ─────────────────────────────────────────
    config_ops::register_config_ops(ctx, db.clone())?;

    // ── agentdesk.http ────────────────────────────────────────────
    http_ops::register_http_ops(ctx)?;

    // ── agentdesk.dispatch ────────────────────────────────────────
    dispatch_ops::register_dispatch_ops(ctx, pg_pool.clone())?;

    // ── agentdesk.kanban ────────────────────────────────────────
    kanban_ops::register_kanban_ops(ctx, db.clone(), pg_pool.clone())?;

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
    message_ops::register_message_ops(ctx, db, pg_pool.clone())?;

    // ── agentdesk.exec ──────────────────────────────────────────
    exec_ops::register_exec_ops(ctx)?;

    // ── agentdesk.pipeline ────────────────────────────────────────
    pipeline_ops::register_pipeline_ops(ctx, db_for_pipeline)?;

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
    kanban_ops::review_state_sync(db, json_str)
}

/// Best-effort auto-queue cleanup for terminal cards.
///
/// When a card finishes, its active dispatch entry should become `done` and any
/// stale pending copies in active or paused runs should be skipped so they do
/// not block other runs.
pub(crate) fn sync_auto_queue_terminal_on_conn(conn: &libsql_rusqlite::Connection, card_id: &str) {
    kanban_ops::sync_auto_queue_terminal_on_conn(conn, card_id)
}

/// Skip live auto-queue entries for a card after PMD explicitly backs the card out.
///
/// Only active/paused runs are touched. Generated or future runs stay intact so
/// PMD can intentionally re-queue the card later after fixing prerequisites.
pub(crate) fn skip_live_auto_queue_entries_for_card_on_conn(
    conn: &libsql_rusqlite::Connection,
    card_id: &str,
) -> libsql_rusqlite::Result<usize> {
    kanban_ops::skip_live_auto_queue_entries_for_card_on_conn(conn, card_id)
}

/// Same as `review_state_sync` but operates on an already-acquired connection.
/// Use this inside transactions or when a lock is already held (#158).
pub fn review_state_sync_on_conn(conn: &libsql_rusqlite::Connection, json_str: &str) -> String {
    kanban_ops::review_state_sync_on_conn(conn, json_str)
}
