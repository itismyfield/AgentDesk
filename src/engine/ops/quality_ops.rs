use rquickjs::{Ctx, Function, Object, Result as JsResult};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Debug, Deserialize)]
struct QualityEmitPayload {
    event_type: Option<String>,
    #[serde(default)]
    source_event_id: Option<String>,
    #[serde(default)]
    correlation_id: Option<String>,
    #[serde(default)]
    agent_id: Option<String>,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    channel_id: Option<String>,
    #[serde(default)]
    card_id: Option<String>,
    #[serde(default)]
    dispatch_id: Option<String>,
    #[serde(default = "empty_payload")]
    payload: Value,
}

fn empty_payload() -> Value {
    json!({})
}

pub(super) fn register_quality_ops(ctx: &Ctx<'_>) -> JsResult<()> {
    let ad: Object<'_> = ctx.globals().get("agentdesk")?;
    let quality_obj = Object::new(ctx.clone())?;

    let emit_raw = Function::new(ctx.clone(), move |event_json: String| -> String {
        quality_emit_raw(&event_json)
    })?;
    quality_obj.set("__emit_raw", emit_raw)?;
    ad.set("quality", quality_obj)?;

    let _: rquickjs::Value = ctx.eval(
        r#"
        (function() {
            agentdesk.quality.emit = function(event) {
                var result = JSON.parse(
                    agentdesk.quality.__emit_raw(JSON.stringify(event || {}))
                );
                if (result.error) {
                    if (agentdesk.log && agentdesk.log.warn) {
                        agentdesk.log.warn("[quality] " + result.error);
                    }
                    return false;
                }
                return true;
            };
        })();
        undefined;
    "#,
    )?;

    Ok(())
}

fn quality_emit_raw(event_json: &str) -> String {
    let payload = match serde_json::from_str::<QualityEmitPayload>(event_json) {
        Ok(payload) => payload,
        Err(error) => {
            return json!({
                "error": format!("parse quality event: {error}"),
            })
            .to_string();
        }
    };
    let Some(event_type) = payload.event_type else {
        return json!({
            "error": "quality event_type is required",
        })
        .to_string();
    };

    crate::services::observability::emit_agent_quality_event(
        crate::services::observability::AgentQualityEvent {
            source_event_id: payload.source_event_id,
            correlation_id: payload.correlation_id,
            agent_id: payload.agent_id,
            provider: payload.provider,
            channel_id: payload.channel_id,
            card_id: payload.card_id,
            dispatch_id: payload.dispatch_id,
            event_type,
            payload: payload.payload,
        },
    );

    json!({"ok": true}).to_string()
}
