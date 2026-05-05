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

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests;

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
use crate::db::Db;
use crate::supervisor::BridgeHandle;
use rquickjs::{Ctx, Function, Object, Result as JsResult};

/// Register all `agentdesk.*` globals in the given JS context.
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub fn register_globals(ctx: &Ctx<'_>, db: Db) -> JsResult<()> {
    register_globals_with_test_backends(ctx, Some(db), None, BridgeHandle::new())
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
#[allow(dead_code)]
pub fn register_globals_with_supervisor(
    ctx: &Ctx<'_>,
    db: Db,
    supervisor_bridge: BridgeHandle,
) -> JsResult<()> {
    register_globals_with_test_backends(ctx, Some(db), None, supervisor_bridge)
}

pub fn register_globals_with_supervisor_and_pg(
    ctx: &Ctx<'_>,
    pg_pool: Option<sqlx::PgPool>,
    supervisor_bridge: BridgeHandle,
) -> JsResult<()> {
    #[cfg(all(test, feature = "legacy-sqlite-tests"))]
    {
        register_globals_with_test_backends(ctx, None, pg_pool, supervisor_bridge)
    }

    #[cfg(not(all(test, feature = "legacy-sqlite-tests")))]
    {
        register_globals_pg_only(ctx, pg_pool, supervisor_bridge)
    }
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(crate) fn register_globals_with_supervisor_and_test_backends(
    ctx: &Ctx<'_>,
    db: Option<Db>,
    pg_pool: Option<sqlx::PgPool>,
    supervisor_bridge: BridgeHandle,
) -> JsResult<()> {
    register_globals_with_test_backends(ctx, db, pg_pool, supervisor_bridge)
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
fn register_globals_with_test_backends(
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
    ci_recovery_ops::register_ci_recovery_ops(ctx, pg_pool.clone())?;

    // ── agentdesk.kv ─────────────────────────────────────────────
    kv_ops::register_kv_ops(ctx, db.clone(), pg_pool.clone())?;

    // ── agentdesk.review ─────────────────────────────────────────
    review_ops::register_review_ops(ctx, db.clone(), pg_pool.clone())?;

    // ── agentdesk.reviewAutomation ─────────────────────────────── #743
    review_automation_ops::register_review_automation_ops(ctx, pg_pool.clone())?;

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
    runtime_ops::register_runtime_ops(ctx, pg_pool.clone(), supervisor_bridge)?;

    // ── agentdesk.message ────────────────────────────────────────
    let pg_for_dm_reply = pg_pool.clone();
    let db_for_agents = db.clone();
    message_ops::register_message_ops(ctx, pg_pool.clone())?;

    // ── agentdesk.exec ──────────────────────────────────────────
    exec_ops::register_exec_ops(ctx)?;

    // ── agentdesk.pipeline ────────────────────────────────────────
    pipeline_ops::register_pipeline_ops(ctx, pg_pool.clone())?;

    // ── agentdesk.dmReply ────────────────────────────────────
    dm_reply_ops::register_dm_reply_ops(ctx, pg_for_dm_reply)?;

    // ── agentdesk.agents ─────────────────────────────────────────
    agent_ops::register_agent_ops(ctx, db_for_agents, pg_pool)?;

    Ok(())
}

#[cfg(not(all(test, feature = "legacy-sqlite-tests")))]
fn register_globals_pg_only(
    ctx: &Ctx<'_>,
    pg_pool: Option<sqlx::PgPool>,
    supervisor_bridge: BridgeHandle,
) -> JsResult<()> {
    let globals = ctx.globals();

    let ad = Object::new(ctx.clone())?;

    let noop = Function::new(ctx.clone(), || -> JsResult<()> { Ok(()) })?;
    ad.set("registerPolicy", noop)?;

    globals.set("agentdesk", ad)?;
    ctx.eval::<(), _>(r#"agentdesk.__pendingIntents = [];"#)?;

    let gen_id = Function::new(ctx.clone(), || -> String {
        uuid::Uuid::new_v4().to_string()
    })?;
    {
        let ad: Object<'_> = ctx.globals().get("agentdesk")?;
        ad.set("__generateId", gen_id)?;
    }

    db_ops::register_db_ops(ctx, None, pg_pool.clone())?;
    cards_ops::register_card_ops(ctx, None, pg_pool.clone())?;
    log_ops::register_log_ops(ctx)?;
    quality_ops::register_quality_ops(ctx)?;
    config_ops::register_config_ops(ctx, None, pg_pool.clone())?;
    http_ops::register_http_ops(ctx)?;
    dispatch_ops::register_dispatch_ops(ctx, pg_pool.clone())?;
    kanban_ops::register_kanban_ops(ctx, None, pg_pool.clone())?;
    ci_recovery_ops::register_ci_recovery_ops(ctx, pg_pool.clone())?;
    kv_ops::register_kv_ops(ctx, None, pg_pool.clone())?;
    review_ops::register_review_ops(ctx, None, pg_pool.clone())?;
    review_automation_ops::register_review_automation_ops(ctx, pg_pool.clone())?;
    queue_ops::register_queue_ops(ctx, pg_pool.clone())?;
    auto_queue_ops::register_auto_queue_ops(ctx, None, pg_pool.clone(), supervisor_bridge.clone())?;
    runtime_ops::register_runtime_ops(ctx, pg_pool.clone(), supervisor_bridge)?;
    message_ops::register_message_ops(ctx, pg_pool.clone())?;
    exec_ops::register_exec_ops(ctx)?;
    pipeline_ops::register_pipeline_ops(ctx, pg_pool.clone())?;
    dm_reply_ops::register_dm_reply_ops(ctx, pg_pool.clone())?;
    agent_ops::register_agent_ops(ctx, pg_pool)?;

    Ok(())
}

/// Test-only wrapper for card_review_state sync (#158).
/// PG-backed tests should pass a pool through `review_state_sync_with_backends`.
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub fn review_state_sync(db: &Db, json_str: &str) -> String {
    review_state_sync_with_backends(Some(db), None, json_str)
}

pub fn review_state_sync_with_backends(
    db: Option<&crate::db::Db>,
    pg_pool: Option<&sqlx::PgPool>,
    json_str: &str,
) -> String {
    kanban_ops::review_state_sync_with_backends(db, pg_pool, json_str)
}
