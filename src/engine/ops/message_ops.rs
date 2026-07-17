use rquickjs::{Ctx, Function, Object, Result as JsResult};
use sqlx::PgPool;

use crate::services::discord::bot_role::UtilityBotRole;

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
    let default_bot = UtilityBotRole::Announce.alias();
    ctx.eval::<(), _>(format!(
        r#"
        agentdesk.message.queue = function(target, content, bot, source) {{
            return JSON.parse(agentdesk.message.__queue_raw(
                target || "",
                content || "",
                bot || "{default_bot}",
                source || "system"
            ));
        }};
        "#
    ))?;

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
                    crate::services::message_outbox::enqueue_outbox_pg_returning_id_with_ttl(
                        &bridge_pool,
                        crate::services::message_outbox::OutboxMessage {
                            target: &target,
                            content: &content,
                            bot: &bot,
                            source: &source,
                            reason_code: None,
                            session_key: None,
                        },
                        0,
                    )
                    .await
                    .map_err(|error| format!("enqueue postgres message_outbox: {error}"))?
                    .ok_or_else(|| "enqueue postgres message_outbox returned no id".to_string())
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

#[cfg(test)]
mod tests {
    use super::queue_message;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn policy_dynamic_source_rejected_before_insert_pg() {
        let Some(pg_db) = crate::dispatch::test_support::DispatchPostgresTestDb::try_create(
            "agentdesk_policy_message_source",
            "policy message source validation tests",
        )
        .await
        else {
            return;
        };
        let pool = pg_db.connect_and_migrate().await;
        let blocking_pool = pool.clone();
        let error = tokio::task::spawn_blocking(move || {
            queue_message(
                Some(&blocking_pool),
                "channel:4424",
                "must not insert",
                "notify",
                "unregistered_policy_source",
            )
        })
        .await
        .expect("join policy queue_message test")
        .expect_err("forbidden dynamic source must be rejected");
        assert!(
            error.contains("not registered for LoopbackInternal"),
            "policy error must be actionable: {error}"
        );
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM message_outbox")
            .fetch_one(&pool)
            .await
            .expect("count policy message_outbox rows");
        assert_eq!(count, 0);
    }
}
