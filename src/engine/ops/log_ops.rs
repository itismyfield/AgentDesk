use rquickjs::{Ctx, Function, Object, Result as JsResult};

// ── Log ops ──────────────────────────────────────────────────────

pub(super) fn register_log_ops<'js>(ctx: &Ctx<'js>) -> JsResult<()> {
    let ad: Object<'js> = ctx.globals().get("agentdesk")?;
    let log_obj = Object::new(ctx.clone())?;

    log_obj.set(
        "info",
        Function::new(ctx.clone(), |msg: String| {
            tracing::info!(target: "policy", message = %msg, "policy log");
        })?,
    )?;

    log_obj.set(
        "warn",
        Function::new(ctx.clone(), |msg: String| {
            tracing::warn!(target: "policy", "{}", msg);
        })?,
    )?;

    log_obj.set(
        "error",
        Function::new(ctx.clone(), |msg: String| {
            tracing::error!(target: "policy", "{}", msg);
        })?,
    )?;

    log_obj.set(
        "debug",
        Function::new(ctx.clone(), |msg: String| {
            tracing::debug!(target: "policy", "{}", msg);
        })?,
    )?;

    ad.set("log", log_obj)?;
    Ok(())
}
