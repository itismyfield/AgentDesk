use crate::db::Db;
use crate::supervisor::BridgeHandle;
use rquickjs::{Ctx, Function, Object, Result as JsResult};

pub(super) fn register_runtime_ops<'js>(
    ctx: &Ctx<'js>,
    db: Db,
    bridge: BridgeHandle,
) -> JsResult<()> {
    let ad: Object<'js> = ctx.globals().get("agentdesk")?;
    let runtime_obj = Object::new(ctx.clone())?;

    runtime_obj.set(
        "__emitSignalRaw",
        Function::new(
            ctx.clone(),
            move |signal_name: String, evidence_json: String| -> String {
                crate::supervisor::emit_signal_json(&db, &bridge, &signal_name, &evidence_json)
            },
        )?,
    )?;

    ad.set("runtime", runtime_obj)?;

    ctx.eval::<(), _>(
        r#"
        (function() {
            var raw = agentdesk.runtime.__emitSignalRaw;
            agentdesk.runtime.emitSignal = function(signalName, evidence) {
                var result = JSON.parse(raw(signalName || "", JSON.stringify(evidence || {})));
                if (result.error) throw new Error(result.error);
                return result;
            };
        })();
        "#,
    )?;

    Ok(())
}
