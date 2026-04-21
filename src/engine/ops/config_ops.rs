use crate::db::Db;
use rquickjs::{Ctx, Function, Object, Result as JsResult};
use sqlx::PgPool;

// ── Config ops ───────────────────────────────────────────────────

pub(super) fn register_config_ops<'js>(
    ctx: &Ctx<'js>,
    db: Option<Db>,
    pg_pool: Option<PgPool>,
) -> JsResult<()> {
    let ad: Object<'js> = ctx.globals().get("agentdesk")?;
    let config_obj = Object::new(ctx.clone())?;

    // __config_get_raw(key) → JSON string: "null" or "\"value\""
    let db_c = db;
    let pg_c = pg_pool;
    config_obj.set(
        "__get_raw",
        Function::new(
            ctx.clone(),
            rquickjs::function::MutFn::from(move |key: String| -> String {
                if let Some(pool) = pg_c.as_ref() {
                    return config_get_raw_pg(pool, &key);
                }
                if let Some(db_c) = db_c.as_ref() {
                    let conn = match db_c.separate_conn() {
                        Ok(c) => c,
                        Err(_) => return "null".to_string(),
                    };
                    return match conn.query_row(
                        "SELECT value FROM kv_meta WHERE key = ?1",
                        [&key],
                        |row| row.get::<_, String>(0),
                    ) {
                        Ok(val) => {
                            serde_json::to_string(&val).unwrap_or_else(|_| "null".to_string())
                        }
                        Err(_) => "null".to_string(),
                    };
                }
                "null".to_string()
            }),
        )?,
    )?;

    ad.set("config", config_obj)?;

    // JS wrapper
    let _: rquickjs::Value = ctx.eval(
        r#"
        (function() {
            agentdesk.config.get = function(key) {
                return JSON.parse(agentdesk.config.__get_raw(key));
            };
        })();
        undefined;
    "#,
    )?;

    Ok(())
}

fn config_get_raw_pg(pool: &PgPool, key: &str) -> String {
    let key = key.to_string();
    crate::utils::async_bridge::block_on_pg_result(
        pool,
        move |bridge_pool| async move {
            let value = sqlx::query_scalar::<_, String>("SELECT value FROM kv_meta WHERE key = $1")
                .bind(&key)
                .fetch_optional(&bridge_pool)
                .await
                .map_err(|error| format!("load postgres kv_meta {key}: {error}"))?;
            Ok(value
                .and_then(|value| serde_json::to_string(&value).ok())
                .unwrap_or_else(|| "null".to_string()))
        },
        |_error| "null".to_string(),
    )
    .unwrap_or_else(|value| value)
}
