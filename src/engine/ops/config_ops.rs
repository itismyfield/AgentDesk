use rquickjs::{Ctx, Function, Object, Result as JsResult};
#[cfg(test)]
use rusqlite::OptionalExtension;
use sqlx::PgPool;

// ── Config ops ───────────────────────────────────────────────────

pub(super) fn register_config_ops<'js>(
    ctx: &Ctx<'js>,
    db: Option<crate::db::Db>,
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
                #[cfg(test)]
                if let Some(db) = db_c.as_ref() {
                    return config_get_raw_sqlite(db, &key);
                }
                let _ = &db_c;
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

#[cfg(test)]
fn config_get_raw_sqlite(db: &crate::db::Db, key: &str) -> String {
    let value = db
        .read_conn()
        .and_then(|conn| {
            conn.query_row(
                "SELECT value FROM kv_meta WHERE key = ?1",
                rusqlite::params![key],
                |row| row.get::<_, String>(0),
            )
            .optional()
        })
        .ok()
        .flatten();
    value
        .and_then(|value| serde_json::to_string(&value).ok())
        .unwrap_or_else(|| "null".to_string())
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
