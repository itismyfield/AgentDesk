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
    let db_for_signal = db.clone();
    let bridge_for_signal = bridge.clone();

    runtime_obj.set(
        "__emitSignalRaw",
        Function::new(
            ctx.clone(),
            move |signal_name: String, evidence_json: String| -> String {
                crate::supervisor::emit_signal_json(
                    &db_for_signal,
                    &bridge_for_signal,
                    &signal_name,
                    &evidence_json,
                )
            },
        )?,
    )?;
    let db_for_retrospective = db.clone();
    runtime_obj.set(
        "__recordCardRetrospectiveRaw",
        Function::new(
            ctx.clone(),
            move |card_id: String, terminal_status: String| -> String {
                crate::services::retrospectives::record_card_retrospective_json(
                    &db_for_retrospective,
                    &card_id,
                    &terminal_status,
                )
            },
        )?,
    )?;

    ad.set("runtime", runtime_obj)?;

    ctx.eval::<(), _>(
        r#"
        (function() {
            var raw = agentdesk.runtime.__emitSignalRaw;
            var recordRetrospectiveRaw = agentdesk.runtime.__recordCardRetrospectiveRaw;
            agentdesk.runtime.emitSignal = function(signalName, evidence) {
                var result = JSON.parse(raw(signalName || "", JSON.stringify(evidence || {})));
                if (result.error) throw new Error(result.error);
                return result;
            };
            agentdesk.runtime.recordCardRetrospective = function(cardId, terminalStatus) {
                return JSON.parse(recordRetrospectiveRaw(cardId || "", terminalStatus || ""));
            };
        })();
        "#,
    )?;

    Ok(())
}
