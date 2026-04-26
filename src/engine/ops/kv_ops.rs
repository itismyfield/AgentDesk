use crate::db::Db;
use rquickjs::{Ctx, Function, Object, Result as JsResult};
use sqlx::PgPool;

// ── KV ops (#126) ─────────────────────────────────────────────────
//
// agentdesk.kv.set(key, value, ttlSeconds) — set with optional TTL
// agentdesk.kv.get(key) → value or null (filters expired)
// agentdesk.kv.delete(key) — delete a key

pub(super) fn register_kv_ops<'js>(
    ctx: &Ctx<'js>,
    _db: Option<Db>,
    pg_pool: Option<PgPool>,
) -> JsResult<()> {
    let ad: Object<'js> = ctx.globals().get("agentdesk")?;
    let kv_obj = Object::new(ctx.clone())?;

    // __kvSetRaw(key, value, ttlSeconds) — Rust raw impl, always 3 args
    let pg_set = pg_pool.clone();
    kv_obj.set(
        "__setRaw",
        Function::new(
            ctx.clone(),
            move |key: String, value: String, ttl_seconds: i64| -> String {
                if let Some(pool) = pg_set.as_ref() {
                    return kv_set_raw_pg(pool, &key, &value, ttl_seconds);
                }
                r#"{"error":"sqlite backend is unavailable"}"#.to_string()
            },
        )?,
    )?;

    // __kvGetRaw(key) → JSON: {"found":true,"value":"..."} or {"found":false}
    let pg_get = pg_pool.clone();
    kv_obj.set(
        "__getRaw",
        Function::new(ctx.clone(), move |key: String| -> String {
            if let Some(pool) = pg_get.as_ref() {
                return kv_get_raw_pg(pool, &key);
            }
            r#"{"found":false}"#.to_string()
        })?,
    )?;

    // kv.delete(key)
    let pg_del = pg_pool.clone();
    kv_obj.set(
        "delete",
        Function::new(ctx.clone(), move |key: String| -> String {
            if let Some(pool) = pg_del.as_ref() {
                return kv_delete_raw_pg(pool, &key);
            }
            r#"{"error":"sqlite backend is unavailable"}"#.to_string()
        })?,
    )?;

    ad.set("kv", kv_obj)?;

    // JS wrappers for optional TTL and null semantics
    ctx.eval::<(), _>(
        r#"
        (function() {
            agentdesk.kv.set = function(key, value, ttlSeconds) {
                return JSON.parse(agentdesk.kv.__setRaw(key, value, ttlSeconds || 0));
            };
            agentdesk.kv.get = function(key) {
                var r = JSON.parse(agentdesk.kv.__getRaw(key));
                return r.found ? r.value : null;
            };
        })();
    "#,
    )?;

    // ── agentdesk.reviewState — typed bridge for card_review_state mutations (#158) ──
    // Replaces direct SQL INSERT/UPDATE on card_review_state from JS policies.
    // All review-state mutations go through this single entrypoint.
    {
        let pg_rs = pg_pool.clone();
        let sync_raw = Function::new(ctx.clone(), move |json_str: String| -> String {
            crate::engine::ops::review_state_sync_with_backends(None, pg_rs.as_ref(), &json_str)
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

fn kv_set_raw_pg(pool: &PgPool, key: &str, value: &str, ttl_seconds: i64) -> String {
    let key = key.to_string();
    let value = value.to_string();
    match crate::utils::async_bridge::block_on_pg_result(
        pool,
        move |bridge_pool| async move {
            let rows_affected = if ttl_seconds > 0 {
                sqlx::query(
                    "INSERT INTO kv_meta (key, value, expires_at)
                     VALUES ($1, $2, NOW() + ($3 * INTERVAL '1 second'))
                     ON CONFLICT (key) DO UPDATE
                     SET value = EXCLUDED.value,
                         expires_at = EXCLUDED.expires_at",
                )
                .bind(&key)
                .bind(&value)
                .bind(ttl_seconds)
                .execute(&bridge_pool)
                .await
            } else {
                sqlx::query(
                    "INSERT INTO kv_meta (key, value, expires_at)
                     VALUES ($1, $2, NULL)
                     ON CONFLICT (key) DO UPDATE
                     SET value = EXCLUDED.value,
                         expires_at = EXCLUDED.expires_at",
                )
                .bind(&key)
                .bind(&value)
                .execute(&bridge_pool)
                .await
            }
            .map_err(|error| format!("upsert postgres kv_meta {key}: {error}"))?
            .rows_affected();
            let _ = rows_affected;
            Ok(r#"{"ok":true}"#.to_string())
        },
        |error| format!(r#"{{"error":"{error}"}}"#),
    ) {
        Ok(result) => result,
        Err(raw) => crate::engine::ops::ensure_js_error_json(raw),
    }
}

fn kv_get_raw_pg(pool: &PgPool, key: &str) -> String {
    let key = key.to_string();
    match crate::utils::async_bridge::block_on_pg_result(
        pool,
        move |bridge_pool| async move {
            let value = sqlx::query_scalar::<_, String>(
                "SELECT value
                 FROM kv_meta
                 WHERE key = $1
                   AND (expires_at IS NULL OR expires_at > NOW())",
            )
            .bind(&key)
            .fetch_optional(&bridge_pool)
            .await
            .map_err(|error| format!("load postgres kv_meta {key}: {error}"))?;
            Ok(match value {
                Some(value) => format!(r#"{{"found":true,"value":{}}}"#, serde_json::json!(value)),
                None => r#"{"found":false}"#.to_string(),
            })
        },
        |error| format!(r#"{{"error":"{error}"}}"#),
    ) {
        Ok(result) => result,
        Err(raw) => crate::engine::ops::ensure_js_error_json(raw),
    }
}

fn kv_delete_raw_pg(pool: &PgPool, key: &str) -> String {
    let key = key.to_string();
    match crate::utils::async_bridge::block_on_pg_result(
        pool,
        move |bridge_pool| async move {
            sqlx::query("DELETE FROM kv_meta WHERE key = $1")
                .bind(&key)
                .execute(&bridge_pool)
                .await
                .map_err(|error| format!("delete postgres kv_meta {key}: {error}"))?;
            Ok(r#"{"ok":true}"#.to_string())
        },
        |error| format!(r#"{{"error":"{error}"}}"#),
    ) {
        Ok(result) => result,
        Err(raw) => crate::engine::ops::ensure_js_error_json(raw),
    }
}
