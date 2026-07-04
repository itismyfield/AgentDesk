use rquickjs::{Ctx, Function, Object, Result as JsResult};

// ── Log ops ──────────────────────────────────────────────────────

// #4075 review: intentionally outside agentdesk so policy JS logs stay out of production logs until a sensitivity sweep.
pub(crate) const POLICY_LOG_TARGET: &str = "policy";
pub(crate) const TIMEOUT_SHADOW_LOG_TARGET: &str = "agentdesk::timeout_shadow";
const TIMEOUT_SHADOW_LOG_PREFIX: &str = "[timeout_shadow] ";
pub(super) fn register_log_ops<'js>(ctx: &Ctx<'js>) -> JsResult<()> {
    let ad: Object<'js> = ctx.globals().get("agentdesk")?;
    let log_obj = Object::new(ctx.clone())?;

    log_obj.set(
        "info",
        Function::new(ctx.clone(), |msg: String| {
            emit_policy_info_log(msg);
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

fn emit_policy_info_log(msg: String) {
    if policy_info_target(&msg) == TIMEOUT_SHADOW_LOG_TARGET {
        tracing::info!(target: TIMEOUT_SHADOW_LOG_TARGET, message = %msg, "policy log");
    } else {
        tracing::info!(target: POLICY_LOG_TARGET, message = %msg, "policy log");
    }
}

fn policy_info_target(msg: &str) -> &'static str {
    if msg.starts_with(TIMEOUT_SHADOW_LOG_PREFIX) {
        TIMEOUT_SHADOW_LOG_TARGET
    } else {
        POLICY_LOG_TARGET
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_info_uses_bare_target_except_timeout_shadow_prefix() {
        assert_eq!(
            policy_info_target("[timeout_shadow] {\"target\":\"agentdesk::timeout_shadow\"}"),
            TIMEOUT_SHADOW_LOG_TARGET
        );
        assert_eq!(
            policy_info_target("[timeout] ordinary policy log"),
            "policy"
        );
    }
}
