use crate::db::Db;
use rquickjs::{Ctx, Function, Object, Result as JsResult};

// ── DM reply tracking ops ────────────────────────────────────────
// agentdesk.dmReply.register(sourceAgent, userId, context, ttlSeconds?)
// agentdesk.dmReply.consume(userId)
// agentdesk.dmReply.pending(userId)

pub(super) fn register_dm_reply_ops<'js>(ctx: &Ctx<'js>, db: Db) -> JsResult<()> {
    let ad: Object<'js> = ctx.globals().get("agentdesk")?;
    let dm_obj = Object::new(ctx.clone())?;

    // __register_raw(source_agent, user_id, channel_id, context, ttl_seconds) → json
    let db_reg = db.clone();
    dm_obj.set(
        "__register_raw",
        Function::new(
            ctx.clone(),
            rquickjs::function::MutFn::from(
                move |source_agent: String,
                      user_id: String,
                      channel_id: String,
                      context: String,
                      ttl_seconds: i64|
                      -> String {
                    dm_reply_register_raw(
                        &db_reg,
                        &source_agent,
                        &user_id,
                        &channel_id,
                        &context,
                        ttl_seconds,
                    )
                },
            ),
        )?,
    )?;

    // __consume_raw(user_id) → json
    let db_con = db.clone();
    dm_obj.set(
        "__consume_raw",
        Function::new(
            ctx.clone(),
            rquickjs::function::MutFn::from(move |user_id: String| -> String {
                dm_reply_consume_raw(&db_con, &user_id)
            }),
        )?,
    )?;

    // __pending_raw(user_id) → json
    let db_pend = db.clone();
    dm_obj.set(
        "__pending_raw",
        Function::new(
            ctx.clone(),
            rquickjs::function::MutFn::from(move |user_id: String| -> String {
                dm_reply_pending_raw(&db_pend, &user_id)
            }),
        )?,
    )?;

    // __read_consumed_raw(user_id) → json (most recent consumed entry with _answer)
    let db_read = db.clone();
    dm_obj.set(
        "__read_consumed_raw",
        Function::new(
            ctx.clone(),
            rquickjs::function::MutFn::from(move |user_id: String| -> String {
                dm_reply_read_consumed_raw(&db_read, &user_id)
            }),
        )?,
    )?;

    ad.set("dmReply", dm_obj)?;

    ctx.eval::<(), _>(
        r#"
        agentdesk.dmReply.register = function(sourceAgent, userId, context, ttlSeconds) {
            var channelId = (context && context.channelId) ? String(context.channelId) : "";
            return JSON.parse(agentdesk.dmReply.__register_raw(
                sourceAgent || "",
                String(userId || ""),
                channelId,
                JSON.stringify(context || {}),
                ttlSeconds || 3600
            ));
        };
        agentdesk.dmReply.consume = function(userId) {
            return JSON.parse(agentdesk.dmReply.__consume_raw(String(userId || "")));
        };
        agentdesk.dmReply.pending = function(userId) {
            return JSON.parse(agentdesk.dmReply.__pending_raw(String(userId || "")));
        };
        agentdesk.dmReply.readConsumed = function(userId) {
            return JSON.parse(agentdesk.dmReply.__read_consumed_raw(String(userId || "")));
        };
        "#,
    )?;

    Ok(())
}

fn dm_reply_register_raw(
    db: &Db,
    source_agent: &str,
    user_id: &str,
    channel_id: &str,
    context: &str,
    ttl_seconds: i64,
) -> String {
    let conn = match db.separate_conn() {
        Ok(c) => c,
        Err(e) => return format!(r#"{{"error":"db connection: {e}"}}"#),
    };
    let expires_at = if ttl_seconds > 0 {
        format!("datetime('now', '+{ttl_seconds} seconds')")
    } else {
        "NULL".to_string()
    };
    let ch = if channel_id.is_empty() {
        None
    } else {
        Some(channel_id)
    };
    let sql = format!(
        "INSERT INTO pending_dm_replies (source_agent, user_id, channel_id, context, expires_at) \
         VALUES (?1, ?2, ?3, ?4, {expires_at})"
    );
    match conn.execute(&sql, rusqlite::params![source_agent, user_id, ch, context]) {
        Ok(_) => {
            let id = conn.last_insert_rowid();
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!(
                "  [{ts}] 📩 dmReply.register → user={user_id} agent={source_agent} (id={id})"
            );
            format!(r#"{{"ok":true,"id":{id}}}"#)
        }
        Err(e) => format!(r#"{{"error":"insert failed: {e}"}}"#),
    }
}

fn dm_reply_consume_raw(db: &Db, user_id: &str) -> String {
    let conn = match db.separate_conn() {
        Ok(c) => c,
        Err(e) => return format!(r#"{{"error":"db connection: {e}"}}"#),
    };
    // FIFO: consume the oldest pending entry for this user that hasn't expired
    let result: Result<(i64, String, String, Option<String>), _> = conn.query_row(
        "SELECT id, source_agent, context, channel_id FROM pending_dm_replies \
         WHERE user_id = ?1 AND status = 'pending' \
         AND (expires_at IS NULL OR expires_at > datetime('now')) \
         ORDER BY created_at ASC LIMIT 1",
        rusqlite::params![user_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    );
    match result {
        Ok((id, source_agent, context, channel_id)) => {
            // CAS: only mark consumed if still pending (guards against race)
            let updated = conn.execute(
                "UPDATE pending_dm_replies SET status = 'consumed', consumed_at = datetime('now') \
                 WHERE id = ?1 AND status = 'pending'",
                rusqlite::params![id],
            );
            match updated {
                Ok(0) => {
                    return r#"{"ok":false,"reason":"already_consumed"}"#.to_string();
                }
                Err(e) => {
                    return format!(r#"{{"error":"update failed: {e}"}}"#);
                }
                _ => {}
            }
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!("  [{ts}] ✉️ dmReply.consume → user={user_id} agent={source_agent} (id={id})");
            let ctx: serde_json::Value =
                serde_json::from_str(&context).unwrap_or(serde_json::json!({}));
            let mut resp = serde_json::json!({
                "ok": true,
                "id": id,
                "sourceAgent": source_agent,
                "context": ctx,
            });
            if let Some(ch) = channel_id {
                resp["channelId"] = serde_json::Value::String(ch);
            }
            serde_json::to_string(&resp).unwrap_or_else(|_| r#"{"error":"serialize"}"#.to_string())
        }
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            r#"{"ok":false,"reason":"no_pending"}"#.to_string()
        }
        Err(e) => format!(r#"{{"error":"query failed: {e}"}}"#),
    }
}

fn dm_reply_pending_raw(db: &Db, user_id: &str) -> String {
    let conn = match db.separate_conn() {
        Ok(c) => c,
        Err(e) => return format!(r#"{{"error":"db connection: {e}"}}"#),
    };
    let result: Result<(i64, String, String, Option<String>), _> = conn.query_row(
        "SELECT id, source_agent, context, channel_id FROM pending_dm_replies \
         WHERE user_id = ?1 AND status = 'pending' \
         AND (expires_at IS NULL OR expires_at > datetime('now')) \
         ORDER BY created_at ASC LIMIT 1",
        rusqlite::params![user_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    );
    match result {
        Ok((id, source_agent, context, channel_id)) => {
            let ctx: serde_json::Value =
                serde_json::from_str(&context).unwrap_or(serde_json::json!({}));
            let mut resp = serde_json::json!({
                "ok": true,
                "id": id,
                "sourceAgent": source_agent,
                "context": ctx,
            });
            if let Some(ch) = channel_id {
                resp["channelId"] = serde_json::Value::String(ch);
            }
            serde_json::to_string(&resp).unwrap_or_else(|_| r#"{"error":"serialize"}"#.to_string())
        }
        Err(rusqlite::Error::QueryReturnedNoRows) => r#"{"ok":false}"#.to_string(),
        Err(e) => format!(r#"{{"error":"query failed: {e}"}}"#),
    }
}

fn dm_reply_read_consumed_raw(db: &Db, user_id: &str) -> String {
    let conn = match db.separate_conn() {
        Ok(c) => c,
        Err(e) => return format!(r#"{{"error":"db connection: {e}"}}"#),
    };
    let result: Result<(i64, String, String, Option<String>), _> = conn.query_row(
        "SELECT id, source_agent, context, channel_id FROM pending_dm_replies \
         WHERE user_id = ?1 AND status = 'consumed' \
         ORDER BY consumed_at DESC LIMIT 1",
        rusqlite::params![user_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    );
    match result {
        Ok((id, source_agent, context, channel_id)) => {
            let ctx: serde_json::Value =
                serde_json::from_str(&context).unwrap_or(serde_json::json!({}));
            let mut resp = serde_json::json!({
                "ok": true,
                "id": id,
                "sourceAgent": source_agent,
                "context": ctx,
            });
            if let Some(ch) = channel_id {
                resp["channelId"] = serde_json::Value::String(ch);
            }
            serde_json::to_string(&resp).unwrap_or_else(|_| r#"{"error":"serialize"}"#.to_string())
        }
        Err(rusqlite::Error::QueryReturnedNoRows) => r#"{"ok":false}"#.to_string(),
        Err(e) => format!(r#"{{"error":"query failed: {e}"}}"#),
    }
}
