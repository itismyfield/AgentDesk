use crate::db::Db;
use rquickjs::{Ctx, Function, Object, Result as JsResult};

// ── Agent channel resolution ops (#304) ─────────────────────────
//
// Exposes Rust channel resolution logic to JS policies so they don't
// query legacy columns directly.

pub(super) fn register_agent_ops<'js>(ctx: &Ctx<'js>, db: Db) -> JsResult<()> {
    let ad: Object<'js> = ctx.globals().get("agentdesk")?;
    let agents_obj = Object::new(ctx.clone())?;

    // __resolvePrimaryChannel(agentId) -> channelId | ""
    let db_primary = db.clone();
    agents_obj.set(
        "__resolvePrimaryChannel",
        Function::new(ctx.clone(), move |agent_id: String| -> String {
            match db_primary.separate_conn() {
                Ok(conn) => {
                    match crate::db::agents::resolve_agent_primary_channel_on_conn(&conn, &agent_id)
                    {
                        Ok(Some(ch)) => ch,
                        _ => String::new(),
                    }
                }
                Err(_) => String::new(),
            }
        })?,
    )?;

    // __resolveCounterModelChannel(agentId) -> channelId | ""
    let db_counter = db.clone();
    agents_obj.set(
        "__resolveCounterModelChannel",
        Function::new(ctx.clone(), move |agent_id: String| -> String {
            match db_counter.separate_conn() {
                Ok(conn) => {
                    match crate::db::agents::resolve_agent_counter_model_channel_on_conn(
                        &conn, &agent_id,
                    ) {
                        Ok(Some(ch)) => ch,
                        _ => String::new(),
                    }
                }
                Err(_) => String::new(),
            }
        })?,
    )?;

    // __resolveDispatchChannel(agentId, dispatchType) -> channelId | ""
    let db_dispatch = db;
    agents_obj.set(
        "__resolveDispatchChannel",
        Function::new(
            ctx.clone(),
            move |agent_id: String, dispatch_type: String| -> String {
                match db_dispatch.separate_conn() {
                    Ok(conn) => {
                        let dtype = if dispatch_type.is_empty() {
                            None
                        } else {
                            Some(dispatch_type.as_str())
                        };
                        match crate::db::agents::resolve_agent_dispatch_channel_on_conn(
                            &conn, &agent_id, dtype,
                        ) {
                            Ok(Some(ch)) => ch,
                            _ => String::new(),
                        }
                    }
                    Err(_) => String::new(),
                }
            },
        )?,
    )?;

    ad.set("agents", agents_obj)?;

    // JS convenience wrappers
    ctx.eval::<(), _>(
        r#"
(function() {
    var a = agentdesk.agents;
    a.resolvePrimaryChannel = function(agentId) {
        var ch = a.__resolvePrimaryChannel(agentId);
        return ch || null;
    };
    a.resolveCounterModelChannel = function(agentId) {
        var ch = a.__resolveCounterModelChannel(agentId);
        return ch || null;
    };
    a.resolveDispatchChannel = function(agentId, dispatchType) {
        var ch = a.__resolveDispatchChannel(agentId, dispatchType || "");
        return ch || null;
    };
})();
"#,
    )?;

    Ok(())
}
