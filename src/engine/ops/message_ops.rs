use crate::db::Db;
use rquickjs::{Ctx, Function, Object, Result as JsResult};

// ── Message queue ops ─────────────────────────────────────────────
// agentdesk.message.queue(target, content, bot?, source?)
// Enqueues a message for async delivery — avoids self-referential HTTP deadlock (#120)

pub(super) fn register_message_ops<'js>(ctx: &Ctx<'js>, db: Db) -> JsResult<()> {
    let ad: Object<'js> = ctx.globals().get("agentdesk")?;
    let msg_obj = Object::new(ctx.clone())?;

    // __queue_raw(target, content, bot, source) → json_string
    let db_clone = db.clone();
    let queue_raw = Function::new(
        ctx.clone(),
        rquickjs::function::MutFn::from(
            move |target: String, content: String, bot: String, source: String| -> String {
                message_queue_raw(&db_clone, &target, &content, &bot, &source)
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

fn message_queue_raw(db: &Db, target: &str, content: &str, bot: &str, source: &str) -> String {
    let conn = match db.separate_conn() {
        Ok(c) => c,
        Err(e) => return format!(r#"{{"error":"db connection: {e}"}}"#),
    };
    match conn.execute(
        "INSERT INTO message_outbox (target, content, bot, source) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![target, content, bot, source],
    ) {
        Ok(_) => {
            let id = conn.last_insert_rowid();
            tracing::info!(
                target,
                bot,
                source,
                message_id = id,
                "queued message from JS bridge"
            );
            format!(r#"{{"ok":true,"id":{id}}}"#)
        }
        Err(e) => format!(r#"{{"error":"insert failed: {e}"}}"#),
    }
}
