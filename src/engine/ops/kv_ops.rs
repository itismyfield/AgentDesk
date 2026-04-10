use crate::db::Db;
use rquickjs::{Ctx, Function, Object, Result as JsResult};

// ── KV ops (#126) ─────────────────────────────────────────────────
//
// agentdesk.kv.set(key, value, ttlSeconds) — set with optional TTL
// agentdesk.kv.get(key) → value or null (filters expired)
// agentdesk.kv.delete(key) — delete a key

pub(super) fn register_kv_ops<'js>(ctx: &Ctx<'js>, db: Db) -> JsResult<()> {
    let ad: Object<'js> = ctx.globals().get("agentdesk")?;
    let kv_obj = Object::new(ctx.clone())?;

    // __kvSetRaw(key, value, ttlSeconds) — Rust raw impl, always 3 args
    let db_set = db.clone();
    kv_obj.set(
        "__setRaw",
        Function::new(
            ctx.clone(),
            move |key: String, value: String, ttl_seconds: i64| -> String {
                let conn = match db_set.separate_conn() {
                    Ok(c) => c,
                    Err(e) => return format!(r#"{{"error":"{}"}}"#, e),
                };
                let result = if ttl_seconds > 0 {
                    conn.execute(
                        &format!(
                            "INSERT OR REPLACE INTO kv_meta (key, value, expires_at) VALUES (?1, ?2, datetime('now', '+{} seconds'))",
                            ttl_seconds
                        ),
                        rusqlite::params![key, value],
                    )
                } else {
                    conn.execute(
                        "INSERT OR REPLACE INTO kv_meta (key, value, expires_at) VALUES (?1, ?2, NULL)",
                        rusqlite::params![key, value],
                    )
                };
                match result {
                    Ok(_) => r#"{"ok":true}"#.to_string(),
                    Err(e) => format!(r#"{{"error":"{}"}}"#, e),
                }
            },
        )?,
    )?;

    // __kvGetRaw(key) → JSON: {"found":true,"value":"..."} or {"found":false}
    let db_get = db.clone();
    kv_obj.set(
        "__getRaw",
        Function::new(ctx.clone(), move |key: String| -> String {
            let conn = match db_get.separate_conn() {
                Ok(c) => c,
                Err(_) => return r#"{"found":false}"#.to_string(),
            };
            match conn.query_row(
                "SELECT value FROM kv_meta WHERE key = ?1 AND (expires_at IS NULL OR expires_at > datetime('now'))",
                [&key],
                |row| row.get::<_, String>(0),
            ) {
                Ok(v) => format!(r#"{{"found":true,"value":{}}}"#, serde_json::json!(v)),
                Err(_) => r#"{"found":false}"#.to_string(),
            }
        })?,
    )?;

    // kv.delete(key)
    let db_del = db.clone();
    kv_obj.set(
        "delete",
        Function::new(ctx.clone(), move |key: String| -> String {
            let conn = match db_del.separate_conn() {
                Ok(c) => c,
                Err(e) => return format!(r#"{{"error":"{}"}}"#, e),
            };
            match conn.execute("DELETE FROM kv_meta WHERE key = ?1", [&key]) {
                Ok(_) => r#"{"ok":true}"#.to_string(),
                Err(e) => format!(r#"{{"error":"{}"}}"#, e),
            }
        })?,
    )?;

    ad.set("kv", kv_obj)?;

    // JS wrappers for optional TTL and null semantics
    ctx.eval::<(), _>(
        r#"
        (function() {
            var raw = agentdesk.kv;
            agentdesk.kv.set = function(key, value, ttlSeconds) {
                return JSON.parse(raw.__setRaw(key, value, ttlSeconds || 0));
            };
            agentdesk.kv.get = function(key) {
                var r = JSON.parse(raw.__getRaw(key));
                return r.found ? r.value : null;
            };
        })();
    "#,
    )?;

    // ── agentdesk.reviewState — typed bridge for card_review_state mutations (#158) ──
    // Replaces direct SQL INSERT/UPDATE on card_review_state from JS policies.
    // All review-state mutations go through this single entrypoint.
    {
        let db_rs = db.clone();
        let sync_raw = Function::new(ctx.clone(), move |json_str: String| -> String {
            crate::engine::ops::review_state_sync(&db_rs, &json_str)
        })?;

        let _: rquickjs::Value = ctx.eval(
            r#"
            (function() {
                agentdesk.reviewState = {
                    __syncRaw: null,
                    sync: function(cardId, state, opts) {
                        opts = opts || {};
                        var payload = JSON.stringify({
                            card_id: cardId,
                            state: state,
                            review_round: opts.review_round || null,
                            last_verdict: opts.last_verdict || null,
                            last_decision: opts.last_decision || null,
                            pending_dispatch_id: opts.pending_dispatch_id || null,
                            approach_change_round: opts.approach_change_round || null,
                            session_reset_round: opts.session_reset_round || null,
                            review_entered_at: opts.review_entered_at || null
                        });
                        var result = JSON.parse(agentdesk.reviewState.__syncRaw(payload));
                        if (result.error) throw new Error(result.error);
                        return result;
                    }
                };
            })();
        "#,
        )?;

        let rs_obj: rquickjs::Value = ctx.eval("agentdesk.reviewState")?;
        let rs_obj: Object = rs_obj.into_object().unwrap();
        rs_obj.set("__syncRaw", sync_raw)?;
    }

    Ok(())
}
