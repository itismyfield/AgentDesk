use crate::db::Db;
use libsql_rusqlite::OptionalExtension; // TODO(#839): drop sqlite fallback once policy-engine tests move to PG fixtures.
use rquickjs::{Ctx, Function, Object, Result as JsResult};
use serde_json::json;
use sqlx::{PgPool, Row as SqlxRow};

// ── Agent channel resolution ops (#304) ─────────────────────────
//
// Exposes Rust channel resolution logic to JS policies so they don't
// query legacy columns directly.

pub(super) fn register_agent_ops<'js>(
    ctx: &Ctx<'js>,
    db: Db,
    pg_pool: Option<PgPool>,
) -> JsResult<()> {
    let ad: Object<'js> = ctx.globals().get("agentdesk")?;
    let agents_obj = Object::new(ctx.clone())?;

    let db_get = db.clone();
    let pg_get = pg_pool.clone();
    agents_obj.set(
        "__getRaw",
        Function::new(ctx.clone(), move |agent_id: String| -> String {
            if let Some(pool) = pg_get.as_ref() {
                return agent_get_raw_pg(pool, &agent_id);
            }
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
    let pg_primary = pg_pool.clone();
    agents_obj.set(
        "__resolvePrimaryChannel",
        Function::new(ctx.clone(), move |agent_id: String| -> String {
            if let Some(pool) = pg_primary.as_ref() {
                return resolve_agent_primary_channel_pg_raw(pool, &agent_id);
            }
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
    let pg_counter = pg_pool.clone();
    agents_obj.set(
        "__resolveCounterModelChannel",
        Function::new(ctx.clone(), move |agent_id: String| -> String {
            if let Some(pool) = pg_counter.as_ref() {
                return resolve_agent_counter_channel_pg_raw(pool, &agent_id);
            }
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
    let pg_dispatch = pg_pool;
    agents_obj.set(
        "__resolveDispatchChannel",
        Function::new(
            ctx.clone(),
            move |agent_id: String, dispatch_type: String| -> String {
                if let Some(pool) = pg_dispatch.as_ref() {
                    return resolve_agent_dispatch_channel_pg_raw(
                        pool,
                        &agent_id,
                        if dispatch_type.is_empty() {
                            None
                        } else {
                            Some(dispatch_type.as_str())
                        },
                    );
                }
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

fn agent_get_raw_pg(pool: &PgPool, agent_id: &str) -> String {
    let agent_id = agent_id.to_string();
    let result = crate::utils::async_bridge::block_on_pg_result(
        pool,
        move |bridge_pool| async move {
            let row = sqlx::query(
                "SELECT id, name, name_ko, department, provider, avatar_emoji,
                        status, xp, description, system_prompt,
                        discord_channel_id, discord_channel_alt, discord_channel_cc, discord_channel_cdx
                 FROM agents
                 WHERE id = $1",
            )
            .bind(&agent_id)
            .fetch_optional(&bridge_pool)
            .await
            .map_err(|error| format!("load postgres agent {agent_id}: {error}"))?;

            let agent = row
                .map(|row| -> Result<serde_json::Value, String> {
                    let bindings = crate::db::agents::AgentChannelBindings {
                        provider: row
                            .try_get("provider")
                            .map_err(|error| format!("decode provider for {agent_id}: {error}"))?,
                        discord_channel_id: row.try_get("discord_channel_id").map_err(|error| {
                            format!("decode discord_channel_id for {agent_id}: {error}")
                        })?,
                        discord_channel_alt: row.try_get("discord_channel_alt").map_err(|error| {
                            format!("decode discord_channel_alt for {agent_id}: {error}")
                        })?,
                        discord_channel_cc: row.try_get("discord_channel_cc").map_err(|error| {
                            format!("decode discord_channel_cc for {agent_id}: {error}")
                        })?,
                        discord_channel_cdx: row.try_get("discord_channel_cdx").map_err(|error| {
                            format!("decode discord_channel_cdx for {agent_id}: {error}")
                        })?,
                    };
                    Ok(json!({
                        "id": row.try_get::<String, _>("id").map_err(|error| format!("decode id for {agent_id}: {error}"))?,
                        "name": row.try_get::<String, _>("name").map_err(|error| format!("decode name for {agent_id}: {error}"))?,
                        "name_ko": row.try_get::<Option<String>, _>("name_ko").map_err(|error| format!("decode name_ko for {agent_id}: {error}"))?,
                        "department": row.try_get::<Option<String>, _>("department").map_err(|error| format!("decode department for {agent_id}: {error}"))?,
                        "provider": bindings.provider.clone(),
                        "avatar_emoji": row.try_get::<Option<String>, _>("avatar_emoji").map_err(|error| format!("decode avatar_emoji for {agent_id}: {error}"))?,
                        "status": row.try_get::<Option<String>, _>("status").map_err(|error| format!("decode status for {agent_id}: {error}"))?,
                        "xp": row.try_get::<Option<i64>, _>("xp").map_err(|error| format!("decode xp for {agent_id}: {error}"))?,
                        "description": row.try_get::<Option<String>, _>("description").map_err(|error| format!("decode description for {agent_id}: {error}"))?,
                        "system_prompt": row.try_get::<Option<String>, _>("system_prompt").map_err(|error| format!("decode system_prompt for {agent_id}: {error}"))?,
                        "discord_channel_id": bindings.discord_channel_id.clone(),
                        "discord_channel_alt": bindings.discord_channel_alt.clone(),
                        "discord_channel_cc": bindings.discord_channel_cc.clone(),
                        "discord_channel_cdx": bindings.discord_channel_cdx.clone(),
                        "primary_channel": bindings.primary_channel(),
                        "counter_model_channel": bindings.counter_model_channel(),
                        "all_channels": bindings.all_channels(),
                    }))
                })
                .transpose()?;

            Ok(json!({ "agent": agent }).to_string())
        },
        |error| json!({ "error": error }).to_string(),
    );

    match result {
        Ok(value) => value,
        Err(error_json) => error_json,
    }
}

fn resolve_agent_primary_channel_pg_raw(pool: &PgPool, agent_id: &str) -> String {
    resolve_agent_channel_pg_raw(pool, agent_id, None, ChannelResolver::Primary)
}

fn resolve_agent_counter_channel_pg_raw(pool: &PgPool, agent_id: &str) -> String {
    resolve_agent_channel_pg_raw(pool, agent_id, None, ChannelResolver::Counter)
}

fn resolve_agent_dispatch_channel_pg_raw(
    pool: &PgPool,
    agent_id: &str,
    dispatch_type: Option<&str>,
) -> String {
    resolve_agent_channel_pg_raw(pool, agent_id, dispatch_type, ChannelResolver::Dispatch)
}

#[derive(Clone, Copy)]
enum ChannelResolver {
    Primary,
    Counter,
    Dispatch,
}

fn resolve_agent_channel_pg_raw(
    pool: &PgPool,
    agent_id: &str,
    dispatch_type: Option<&str>,
    resolver: ChannelResolver,
) -> String {
    let agent_id = agent_id.to_string();
    let dispatch_type = dispatch_type.map(str::to_string);
    match crate::utils::async_bridge::block_on_pg_result(
        pool,
        move |bridge_pool| async move {
            let resolved = match resolver {
                ChannelResolver::Primary => {
                    crate::db::agents::resolve_agent_primary_channel_pg(&bridge_pool, &agent_id)
                        .await
                        .map_err(|error| {
                            format!("resolve primary postgres channel {agent_id}: {error}")
                        })?
                }
                ChannelResolver::Counter => {
                    crate::db::agents::resolve_agent_counter_model_channel_pg(
                        &bridge_pool,
                        &agent_id,
                    )
                    .await
                    .map_err(|error| {
                        format!("resolve counter postgres channel {agent_id}: {error}")
                    })?
                }
                ChannelResolver::Dispatch => crate::db::agents::resolve_agent_dispatch_channel_pg(
                    &bridge_pool,
                    &agent_id,
                    dispatch_type.as_deref(),
                )
                .await
                .map_err(|error| {
                    format!("resolve dispatch postgres channel {agent_id}: {error}")
                })?,
            };
            Ok(resolved.unwrap_or_default())
        },
        |_| String::new(),
    ) {
        Ok(value) => value,
        Err(value) => value,
    }
}
