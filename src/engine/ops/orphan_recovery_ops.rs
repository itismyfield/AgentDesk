use rquickjs::{Ctx, Function, Object, Result as JsResult};

pub(super) fn register_orphan_recovery_ops<'js>(ctx: &Ctx<'js>) -> JsResult<()> {
    let ad: Object = ctx.globals().get("agentdesk")?;
    let runtime: Object = ad.get("runtime")?;
    runtime.set(
        "maintainOrphanDispatches",
        Function::new(ctx.clone(), |ctx: Ctx<'js>| {
            crate::supervisor::orphan_recovery::maintain_policy(&ctx)
        })?,
    )
}
