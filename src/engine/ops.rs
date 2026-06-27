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
pub(crate) mod turn_ops;

pub(crate) use db_ops::execute_policy_sql;
pub(crate) use review_ops::{ADVANCE_REVIEW_ROUND_HINT_KEY, ensure_js_error_json};

use crate::supervisor::BridgeHandle;
use rquickjs::{Ctx, Function, Object, Result as JsResult};

pub fn register_globals_with_supervisor_and_pg(
    ctx: &Ctx<'_>,
    pg_pool: Option<sqlx::PgPool>,
    supervisor_bridge: BridgeHandle,
) -> JsResult<()> {
    register_globals_pg_only(ctx, pg_pool, supervisor_bridge)
}

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
    kv_ops::register_kv_ops(ctx, pg_pool.clone())?;
    review_ops::register_review_ops(ctx, None, pg_pool.clone())?;
    review_automation_ops::register_review_automation_ops(ctx, pg_pool.clone())?;
    queue_ops::register_queue_ops(ctx, pg_pool.clone())?;
    auto_queue_ops::register_auto_queue_ops(ctx, None, pg_pool.clone(), supervisor_bridge.clone())?;
    runtime_ops::register_runtime_ops(ctx, pg_pool.clone(), supervisor_bridge)?;
    message_ops::register_message_ops(ctx, pg_pool.clone())?;
    exec_ops::register_exec_ops(ctx)?;
    pipeline_ops::register_pipeline_ops(ctx, pg_pool.clone())?;
    dm_reply_ops::register_dm_reply_ops(ctx, pg_pool.clone())?;
    agent_ops::register_agent_ops(ctx, pg_pool.clone())?;
    turn_ops::register_turn_ops(ctx, pg_pool)?;

    Ok(())
}

pub fn review_state_sync_with_backends(
    db: Option<&crate::db::Db>,
    pg_pool: Option<&sqlx::PgPool>,
    json_str: &str,
) -> String {
    kanban_ops::review_state_sync_with_backends(db, pg_pool, json_str)
}
