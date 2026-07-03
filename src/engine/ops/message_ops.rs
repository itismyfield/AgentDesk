use rquickjs::{Ctx, Function, Object, Result as JsResult};
use sqlx::PgPool;

// ── Message queue ops ─────────────────────────────────────────────
// agentdesk.message.queue(target, content, bot?, source?)
// Enqueues a message for async delivery — avoids self-referential HTTP deadlock (#120)

pub(super) fn register_message_ops<'js>(ctx: &Ctx<'js>, pg_pool: Option<PgPool>) -> JsResult<()> {
    let ad: Object<'js> = ctx.globals().get("agentdesk")?;
    let msg_obj = Object::new(ctx.clone())?;

    // __queue_raw(target, content, bot, source) → json_string
    let pg_clone = pg_pool.clone();
    let queue_raw = Function::new(
        ctx.clone(),
        rquickjs::function::MutFn::from(
            move |target: String, content: String, bot: String, source: String| -> String {
                message_queue_raw(pg_clone.as_ref(), &target, &content, &bot, &source)
            },
        ),
    )?;
    msg_obj.set("__queue_raw", queue_raw)?;

    ad.set("message", msg_obj)?;

    // JS wrapper: agentdesk.message.queue(target, content, bot?, source?)
    ctx.eval::<(), _>(
        r#"
        agentdesk.message.queue = function(target, content, bot, source) {
            return JSON.parse(agentdesk.message.__queue_raw(
                target || "",
                content || "",
                bot || "announce",
                source || "system"
            ));
        };
        "#,
    )?;

    Ok(())
}

pub(crate) fn queue_message(
    pg_pool: Option<&PgPool>,
    target: &str,
    content: &str,
    bot: &str,
    source: &str,
) -> Result<i64, String> {
    if let Some(pool) = pg_pool {
        return crate::utils::async_bridge::block_on_pg_result(
            pool,
            {
                let target = target.to_string();
                let content = content.to_string();
                let bot = bot.to_string();
                let source = source.to_string();
                move |bridge_pool| async move {
                    sqlx::query_scalar::<_, i64>(
                        "INSERT INTO message_outbox (target, content, bot, source)
                         VALUES ($1, $2, $3, $4)
                         RETURNING id",
                    )
                    .bind(&target)
                    .bind(&content)
                    .bind(&bot)
                    .bind(&source)
                    .fetch_one(&bridge_pool)
                    .await
                    .map_err(|e| format!("insert postgres message_outbox: {e}"))
                }
            },
            |error| error,
        );
    }

    Err("sqlite backend is unavailable".to_string())
}

fn message_queue_raw(
    pg_pool: Option<&PgPool>,
    target: &str,
    content: &str,
    bot: &str,
    source: &str,
) -> String {
    match queue_message(pg_pool, target, content, bot, source) {
        Ok(id) => {
            tracing::info!(
                target,
                bot,
                source,
                message_id = id,
                "queued message from JS bridge"
            );
            format!(r#"{{"ok":true,"id":{id}}}"#)
        }
        // #2045 Finding 8 (P2): properly escape arbitrary error text so the
        // JS wrapper's `JSON.parse` cannot trip on backslashes / newlines
        // that Postgres or sqlx error messages may include.
        Err(error) => crate::engine::ops::ensure_js_error_json(error),
    }
}
