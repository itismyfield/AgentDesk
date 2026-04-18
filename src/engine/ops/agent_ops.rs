use crate::db::Db;
use libsql_rusqlite::OptionalExtension;
use rquickjs::{Ctx, Function, Object, Result as JsResult};
use serde_json::json;

// ── Agent channel resolution ops (#304) ─────────────────────────
//
// Exposes Rust channel resolution logic to JS policies so they don't
// query legacy columns directly.

pub(super) fn register_agent_ops<'js>(ctx: &Ctx<'js>, db: Db) -> JsResult<()> {
    let ad: Object<'js> = ctx.globals().get("agentdesk")?;
    let agents_obj = Object::new(ctx.clone())?;

    let db_get = db.clone();
    agents_obj.set(
        "__getRaw",
        Function::new(ctx.clone(), move |agent_id: String| -> String {
            let result = (|| -> anyhow::Result<serde_json::Value> {
                let conn = db_get.read_conn()?;
                let agent = conn
                    .query_row(
                        "SELECT id, name, name_ko, department, provider, avatar_emoji, \
                                status, xp, description, system_prompt, \
                                discord_channel_id, discord_channel_alt, discord_channel_cc, discord_channel_cdx \
                         FROM agents WHERE id = ?1",
                        [&agent_id],
                        |row| {
                            let bindings = crate::db::agents::AgentChannelBindings {
                                provider: row.get(4)?,
                                discord_channel_id: row.get(10)?,
                                discord_channel_alt: row.get(11)?,
                                discord_channel_cc: row.get(12)?,
                                discord_channel_cdx: row.get(13)?,
                            };
                            Ok(json!({
                                "id": row.get::<_, String>(0)?,
                                "name": row.get::<_, String>(1)?,
                                "name_ko": row.get::<_, Option<String>>(2)?,
                                "department": row.get::<_, Option<String>>(3)?,
                                "provider": bindings.provider.clone(),
                                "avatar_emoji": row.get::<_, Option<String>>(5)?,
                                "status": row.get::<_, Option<String>>(6)?,
                                "xp": row.get::<_, Option<i64>>(7)?,
                                "description": row.get::<_, Option<String>>(8)?,
                                "system_prompt": row.get::<_, Option<String>>(9)?,
                                "discord_channel_id": bindings.discord_channel_id.clone(),
                                "discord_channel_alt": bindings.discord_channel_alt.clone(),
                                "discord_channel_cc": bindings.discord_channel_cc.clone(),
                                "discord_channel_cdx": bindings.discord_channel_cdx.clone(),
                                "primary_channel": bindings.primary_channel(),
                                "counter_model_channel": bindings.counter_model_channel(),
                                "all_channels": bindings.all_channels(),
                            }))
                        },
                    )
                    .optional()?;
                Ok(json!({ "agent": agent }))
            })();

            match result {
                Ok(value) => value.to_string(),
                Err(err) => json!({ "error": err.to_string() }).to_string(),
            }
        })?,
    )?;

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
    agentdesk.agents.get = function(agentId) {
        var result = JSON.parse(agentdesk.agents.__getRaw(agentId || ""));
        if (result.error) throw new Error(result.error);
        return result.agent || null;
    };
    agentdesk.agents.primaryChannel = function(agentId) {
        return agentdesk.agents.resolvePrimaryChannel(agentId);
    };
    agentdesk.agents.counterModelChannel = function(agentId) {
        return agentdesk.agents.resolveCounterModelChannel(agentId);
    };
    agentdesk.agents.resolvePrimaryChannel = function(agentId) {
        var ch = agentdesk.agents.__resolvePrimaryChannel(agentId);
        return ch || null;
    };
    agentdesk.agents.resolveCounterModelChannel = function(agentId) {
        var ch = agentdesk.agents.__resolveCounterModelChannel(agentId);
        return ch || null;
    };
    agentdesk.agents.resolveDispatchChannel = function(agentId, dispatchType) {
        var ch = agentdesk.agents.__resolveDispatchChannel(agentId, dispatchType || "");
        return ch || null;
    };
})();
"#,
    )?;

    Ok(())
}
