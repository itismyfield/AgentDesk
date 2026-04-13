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
    let bridge_should_defer_signal = bridge.clone();
    runtime_obj.set(
        "__shouldDeferSignalRaw",
        Function::new(ctx.clone(), move || -> bool {
            should_defer_signal(&bridge_should_defer_signal)
        })?,
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
            var shouldDeferSignal = agentdesk.runtime.__shouldDeferSignalRaw;
            var recordRetrospectiveRaw = agentdesk.runtime.__recordCardRetrospectiveRaw;
            agentdesk.runtime.emitSignal = function(signalName, evidence) {
                var normalizedSignal = signalName || "";
                var normalizedEvidence = evidence || {};
                if (shouldDeferSignal()) {
                    agentdesk.__pendingIntents = agentdesk.__pendingIntents || [];
                    agentdesk.__pendingIntents.push({
                        type: "emit_supervisor_signal",
                        signal_name: normalizedSignal,
                        evidence: normalizedEvidence
                    });
                    return {
                        ok: true,
                        deferred: true,
                        signal: normalizedSignal,
                        executed: false,
                        note: "deferred until hook completion"
                    };
                }
                var result = JSON.parse(raw(normalizedSignal, JSON.stringify(normalizedEvidence)));
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

fn should_defer_signal(bridge: &BridgeHandle) -> bool {
    bridge
        .upgrade_engine()
        .map(|engine| engine.is_actor_thread())
        .unwrap_or(false)
}
