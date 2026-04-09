use crate::db::Db;
use rquickjs::{Ctx, Function, Object, Result as JsResult};

// ── Config ops ───────────────────────────────────────────────────

pub(super) fn register_config_ops<'js>(ctx: &Ctx<'js>, db: Db) -> JsResult<()> {
    let ad: Object<'js> = ctx.globals().get("agentdesk")?;
    let config_obj = Object::new(ctx.clone())?;

    // __config_get_raw(key) → JSON string: "null" or "\"value\""
    let db_c = db;
    config_obj.set(
        "__get_raw",
        Function::new(
            ctx.clone(),
            rquickjs::function::MutFn::from(move |key: String| -> String {
                let conn = match db_c.separate_conn() {
                    Ok(c) => c,
                    Err(_) => return "null".to_string(),
                };
                match conn.query_row("SELECT value FROM kv_meta WHERE key = ?1", [&key], |row| {
                    row.get::<_, String>(0)
                }) {
                    Ok(val) => serde_json::to_string(&val).unwrap_or_else(|_| "null".to_string()),
                    Err(_) => "null".to_string(),
                }
            }),
        )?,
    )?;

    ad.set("config", config_obj)?;

    // JS wrapper
    let _: rquickjs::Value = ctx.eval(
        r#"
        (function() {
            var rawGet = agentdesk.config.__get_raw;
            agentdesk.config.get = function(key) {
                return JSON.parse(rawGet(key));
            };
        })();
        undefined;
    "#,
    )?;

    Ok(())
}
