use crate::services::escalation_cooldown::{self, Command};
use rquickjs::{Ctx, Function, Object, Result as JsResult};
use serde_json::json;
use sqlx::PgPool;

pub(super) fn register_escalation_ops(ctx: &Ctx<'_>, pg_pool: Option<PgPool>) -> JsResult<()> {
    let ad: Object<'_> = ctx.globals().get("agentdesk")?;
    let obj = Object::new(ctx.clone())?;
    obj.set(
        "__callRaw",
        Function::new(ctx.clone(), move |payload: String| -> String {
            let result = (|| {
                let command: Command =
                    serde_json::from_str(&payload).map_err(|error| error.to_string())?;
                let pool = pg_pool
                    .as_ref()
                    .ok_or("postgres backend is required for escalation")?;
                crate::utils::async_bridge::block_on_pg_result(
                    pool,
                    move |pool| async move {
                        escalation_cooldown::execute(&pool, command)
                            .await
                            .map_err(|error| error.to_string())
                    },
                    |error| error,
                )
            })();
            match result {
                Ok(value) => value.to_string(),
                Err(error) => json!({"error":error}).to_string(),
            }
        })?,
    )?;
    ad.set("escalation", obj)?;
    ctx.eval::<(), _>(r#"
        (function() {
            function call(payload) {
                var result = JSON.parse(agentdesk.escalation.__callRaw(JSON.stringify(payload)));
                if (result && result.error) throw new Error(result.error);
                return result;
            }
            agentdesk.escalation.enqueue = function(cardId, title, reason) {
                return call({op: "enqueue", card_id: cardId, title: title, reason: reason});
            };
            agentdesk.escalation.pending = function() { return call({op: "pending"}); };
            agentdesk.escalation.prepare = function(row, fingerprint) {
                return call({op: "prepare", card_id: row.card_id, value: row.value, fingerprint: fingerprint});
            };
            agentdesk.escalation.sent = function(cardId, fingerprint) {
                return call({op: "sent", card_id: cardId, fingerprint: fingerprint});
            };
        })();
    "#)?;
    Ok(())
}
