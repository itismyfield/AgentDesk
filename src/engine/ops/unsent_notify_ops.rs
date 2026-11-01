use rquickjs::{Ctx, Function, Object, Result as JsResult};
use serde_json::json;
use sqlx::PgPool;

pub(super) fn register_unsent_notify_ops(ctx: &Ctx<'_>, pg_pool: Option<PgPool>) -> JsResult<()> {
    let ad: Object<'_> = ctx.globals().get("agentdesk")?;
    let timeouts: Object<'_> = ad.get("timeouts")?;
    timeouts.set(
        "__maintainUnsentNotificationsRaw",
        Function::new(ctx.clone(), move || -> String {
            let Some(pool) = pg_pool.as_ref() else {
                return json!({"error":"postgres backend is required for agentdesk.timeouts"})
                    .to_string();
            };
            let result = crate::utils::async_bridge::block_on_pg_result(
                pool,
                |pool| async move {
                    crate::services::dispatches::notify_recovery::maintain_unsent_notifications_pg(
                        &pool,
                    )
                    .await
                    .map_err(|error| error.to_string())
                },
                |error| error,
            );
            match result {
                Ok(()) => json!({"ok":true}).to_string(),
                Err(error) => json!({"error":error}).to_string(),
            }
        })?,
    )?;
    ctx.eval::<(), _>(
        r#"agentdesk.timeouts.maintainUnsentNotifications = function() {
            var result = JSON.parse(agentdesk.timeouts.__maintainUnsentNotificationsRaw());
            if (result.error) throw new Error(result.error);
            return result;
        };"#,
    )
}
