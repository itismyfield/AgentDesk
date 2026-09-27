use rquickjs::{Ctx, Function, Object, Result as JsResult};

// ── Log ops ──────────────────────────────────────────────────────

// #4075 review: intentionally outside agentdesk so policy JS logs stay out of production logs until a sensitivity sweep.
pub(crate) const POLICY_LOG_TARGET: &str = "policy";
pub(super) fn register_log_ops<'js>(ctx: &Ctx<'js>) -> JsResult<()> {
    let ad: Object<'js> = ctx.globals().get("agentdesk")?;
    let log_obj = Object::new(ctx.clone())?;

    log_obj.set(
        "info",
        Function::new(ctx.clone(), |msg: String| {
            tracing::info!(target: POLICY_LOG_TARGET, message = %msg, "policy log");
        })?,
    )?;

    log_obj.set(
        "warn",
        Function::new(ctx.clone(), |msg: String| {
            tracing::warn!(target: POLICY_LOG_TARGET, "{}", msg);
        })?,
    )?;

    log_obj.set(
        "error",
        Function::new(ctx.clone(), |msg: String| {
            tracing::error!(target: POLICY_LOG_TARGET, "{}", msg);
        })?,
    )?;

    log_obj.set(
        "debug",
        Function::new(ctx.clone(), |msg: String| {
            tracing::debug!(target: POLICY_LOG_TARGET, "{}", msg);
        })?,
    )?;

    ad.set("log", log_obj)?;
    Ok(())
}
