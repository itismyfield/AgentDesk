use crate::db::Db;
use rquickjs::{Ctx, Function, Object, Result as JsResult};
use serde_json::json;

pub(super) fn register_queue_ops<'js>(ctx: &Ctx<'js>, db: Db) -> JsResult<()> {
    let ad: Object<'js> = ctx.globals().get("agentdesk")?;
    let queue_obj = Object::new(ctx.clone())?;

    let db_status = db;
    queue_obj.set(
        "__statusRaw",
        Function::new(ctx.clone(), move || -> String {
            queue_status_raw(&db_status)
        })?,
    )?;

    ad.set("queue", queue_obj)?;

    ctx.eval::<(), _>(
        r#"
        (function() {
            agentdesk.queue.status = function() {
                var result = JSON.parse(agentdesk.queue.__statusRaw());
                if (result.error) throw new Error(result.error);
                return result;
            };
        })();
        "#,
    )?;

    Ok(())
}

fn queue_status_raw(db: &Db) -> String {
    let result = (|| -> anyhow::Result<serde_json::Value> {
        let conn = db.read_conn()?;
        let has_auto_runs = table_exists(&conn, "auto_queue_runs")?;
        let has_auto_entries = table_exists(&conn, "auto_queue_entries")?;

        Ok(json!({
            "dispatches": {
                "pending": count(&conn, "SELECT COUNT(*) FROM task_dispatches WHERE status = 'pending'")?,
                "dispatched": count(&conn, "SELECT COUNT(*) FROM task_dispatches WHERE status = 'dispatched'")?,
            },
            "legacy_dispatch_queue": {
                "queued": count(&conn, "SELECT COUNT(*) FROM dispatch_queue")?,
            },
            "message_outbox": {
                "pending": count(&conn, "SELECT COUNT(*) FROM message_outbox WHERE status = 'pending'")?,
                "failed": count(&conn, "SELECT COUNT(*) FROM message_outbox WHERE status = 'failed'")?,
            },
            "dispatch_outbox": {
                "pending": count(&conn, "SELECT COUNT(*) FROM dispatch_outbox WHERE status = 'pending'")?,
                "processing": count(&conn, "SELECT COUNT(*) FROM dispatch_outbox WHERE status = 'processing'")?,
                "failed": count(&conn, "SELECT COUNT(*) FROM dispatch_outbox WHERE status = 'failed'")?,
            },
            "auto_queue": {
                "active_runs": if has_auto_runs {
                    count(&conn, "SELECT COUNT(*) FROM auto_queue_runs WHERE status = 'active'")?
                } else {
                    0
                },
                "paused_runs": if has_auto_runs {
                    count(&conn, "SELECT COUNT(*) FROM auto_queue_runs WHERE status = 'paused'")?
                } else {
                    0
                },
                "pending_entries": if has_auto_entries {
                    count(&conn, "SELECT COUNT(*) FROM auto_queue_entries WHERE status = 'pending'")?
                } else {
                    0
                },
                "dispatched_entries": if has_auto_entries {
                    count(&conn, "SELECT COUNT(*) FROM auto_queue_entries WHERE status = 'dispatched'")?
                } else {
                    0
                },
                "done_entries": if has_auto_entries {
                    count(&conn, "SELECT COUNT(*) FROM auto_queue_entries WHERE status = 'done'")?
                } else {
                    0
                },
            }
        }))
    })();

    match result {
        Ok(value) => value.to_string(),
        Err(err) => json!({ "error": err.to_string() }).to_string(),
    }
}

fn count(conn: &rusqlite::Connection, sql: &str) -> anyhow::Result<i64> {
    conn.query_row(sql, [], |row| row.get(0))
        .map_err(anyhow::Error::from)
}

fn table_exists(conn: &rusqlite::Connection, table: &str) -> anyhow::Result<bool> {
    conn.query_row(
        "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [table],
        |row| row.get(0),
    )
    .map_err(anyhow::Error::from)
}
