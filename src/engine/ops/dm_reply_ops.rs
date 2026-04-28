use crate::db::Db;
use crate::services::discord_dm_reply_store::PendingDmReplyRecord;
use rquickjs::{Ctx, Function, Object, Result as JsResult};
use sqlx::{PgPool, Row};
use std::future::Future;

// ── DM reply tracking ops ────────────────────────────────────────
// agentdesk.dmReply.register(sourceAgent, userId, context, ttlSeconds?)
// agentdesk.dmReply.consume(userId)
// agentdesk.dmReply.pending(userId)

pub(super) fn register_dm_reply_ops<'js>(
    ctx: &Ctx<'js>,
    _db: Option<Db>,
    pg_pool: Option<PgPool>,
) -> JsResult<()> {
    let ad: Object<'js> = ctx.globals().get("agentdesk")?;
    let dm_obj = Object::new(ctx.clone())?;

    // __register_raw(source_agent, user_id, channel_id, context, ttl_seconds) → json
    let pg_reg = pg_pool.clone();
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
                        pg_reg.clone(),
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
    let pg_con = pg_pool.clone();
    dm_obj.set(
        "__consume_raw",
        Function::new(
            ctx.clone(),
            rquickjs::function::MutFn::from(move |user_id: String| -> String {
                dm_reply_consume_raw(pg_con.clone(), &user_id)
            }),
        )?,
    )?;

    // __pending_raw(user_id) → json
    let pg_pend = pg_pool.clone();
    dm_obj.set(
        "__pending_raw",
        Function::new(
            ctx.clone(),
            rquickjs::function::MutFn::from(move |user_id: String| -> String {
                dm_reply_pending_raw(pg_pend.clone(), &user_id)
            }),
        )?,
    )?;

    // __read_consumed_raw(user_id) → json (most recent consumed entry with _answer)
    let pg_read = pg_pool.clone();
    dm_obj.set(
        "__read_consumed_raw",
        Function::new(
            ctx.clone(),
            rquickjs::function::MutFn::from(move |user_id: String| -> String {
                dm_reply_read_consumed_raw(pg_read.clone(), &user_id)
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
    pg_pool: Option<PgPool>,
    source_agent: &str,
    user_id: &str,
    channel_id: &str,
    context: &str,
    ttl_seconds: i64,
) -> String {
    let Some(pg_pool) = pg_pool else {
        return r#"{"error":"postgres backend is unavailable"}"#.to_string();
    };

    let source_agent_trimmed = source_agent.trim().to_string();
    let user_id_trimmed = user_id.trim().to_string();
    if source_agent_trimmed.is_empty() || user_id_trimmed.is_empty() {
        return r#"{"error":"source_agent and user_id are required"}"#.to_string();
    }
    let channel_id_owned: Option<String> =
        (!channel_id.trim().is_empty()).then(|| channel_id.trim().to_string());
    let context_owned = context.to_string();
    let log_source_agent = source_agent_trimmed.clone();
    let log_user_id = user_id_trimmed.clone();
    let log_channel_id = channel_id_owned.clone();

    let result = run_async_bridge_pg(&pg_pool, move |pool| async move {
        register_pending_dm_reply_pg(
            &pool,
            &source_agent_trimmed,
            &user_id_trimmed,
            channel_id_owned.as_deref(),
            &context_owned,
            ttl_seconds,
        )
        .await
    });

    match result {
        Ok(id) => {
            tracing::info!(
                user_id = log_user_id.as_str(),
                agent_id = log_source_agent.as_str(),
                channel_id = ?log_channel_id.as_deref(),
                reply_id = id,
                "registered pending DM reply"
            );
            format!(r#"{{"ok":true,"id":{id}}}"#)
        }
        Err(e) => format!(r#"{{"error":"{e}"}}"#),
    }
}

fn dm_reply_consume_raw(pg_pool: Option<PgPool>, user_id: &str) -> String {
    let Some(pg_pool) = pg_pool else {
        return r#"{"error":"postgres backend is unavailable"}"#.to_string();
    };

    let user_id_owned = user_id.to_string();
    let log_user_id = user_id_owned.clone();
    match run_async_bridge_pg(&pg_pool, move |pool| async move {
        consume_pending_dm_reply_pg(&pool, &user_id_owned).await
    }) {
        Ok(ConsumePendingDmReplyResult::Consumed(record)) => {
            tracing::info!(
                user_id = log_user_id.as_str(),
                agent_id = record.source_agent.as_str(),
                channel_id = ?record.channel_id.as_deref(),
                reply_id = record.id,
                "consumed pending DM reply"
            );
            dm_reply_record_json(record, true)
        }
        Ok(ConsumePendingDmReplyResult::AlreadyConsumed) => {
            r#"{"ok":false,"reason":"already_consumed"}"#.to_string()
        }
        Ok(ConsumePendingDmReplyResult::NoPending) => {
            r#"{"ok":false,"reason":"no_pending"}"#.to_string()
        }
        Err(error) => format!(r#"{{"error":"{error}"}}"#),
    }
}

fn dm_reply_pending_raw(pg_pool: Option<PgPool>, user_id: &str) -> String {
    let Some(pg_pool) = pg_pool else {
        return r#"{"error":"postgres backend is unavailable"}"#.to_string();
    };

    let user_id_owned = user_id.to_string();
    match run_async_bridge_pg(&pg_pool, move |pool| async move {
        load_oldest_pending_dm_reply_pg(&pool, &user_id_owned).await
    }) {
        Ok(Some(record)) => dm_reply_record_json(record, true),
        Ok(None) => r#"{"ok":false}"#.to_string(),
        Err(error) => format!(r#"{{"error":"{error}"}}"#),
    }
}

fn dm_reply_read_consumed_raw(pg_pool: Option<PgPool>, user_id: &str) -> String {
    let Some(pg_pool) = pg_pool else {
        return r#"{"error":"postgres backend is unavailable"}"#.to_string();
    };

    let user_id_owned = user_id.to_string();
    match run_async_bridge_pg(&pg_pool, move |pool| async move {
        load_most_recent_consumed_dm_reply_pg(&pool, &user_id_owned).await
    }) {
        Ok(Some(record)) => dm_reply_record_json(record, true),
        Ok(None) => r#"{"ok":false}"#.to_string(),
        Err(error) => format!(r#"{{"error":"{error}"}}"#),
    }
}

enum ConsumePendingDmReplyResult {
    Consumed(PendingDmReplyRecord),
    NoPending,
    AlreadyConsumed,
}

async fn register_pending_dm_reply_pg(
    pool: &PgPool,
    source_agent: &str,
    user_id: &str,
    channel_id: Option<&str>,
    context_json: &str,
    ttl_seconds: i64,
) -> Result<i64, String> {
    let id = if ttl_seconds > 0 {
        sqlx::query_scalar::<_, i64>(
            "INSERT INTO pending_dm_replies (
                source_agent, user_id, channel_id, context, expires_at
             )
             VALUES ($1, $2, $3, CAST($4 AS jsonb), NOW() + ($5 * INTERVAL '1 second'))
             RETURNING id",
        )
        .bind(source_agent)
        .bind(user_id)
        .bind(channel_id)
        .bind(context_json)
        .bind(ttl_seconds)
        .fetch_one(pool)
        .await
        .map_err(|error| format!("insert failed: {error}"))?
    } else {
        sqlx::query_scalar::<_, i64>(
            "INSERT INTO pending_dm_replies (
                source_agent, user_id, channel_id, context, expires_at
             )
             VALUES ($1, $2, $3, CAST($4 AS jsonb), NULL)
             RETURNING id",
        )
        .bind(source_agent)
        .bind(user_id)
        .bind(channel_id)
        .bind(context_json)
        .fetch_one(pool)
        .await
        .map_err(|error| format!("insert failed: {error}"))?
    };
    Ok(id)
}

async fn load_oldest_pending_dm_reply_pg(
    pool: &PgPool,
    user_id: &str,
) -> Result<Option<PendingDmReplyRecord>, String> {
    let row = sqlx::query(
        "SELECT id, source_agent, context::text AS context_json, channel_id
         FROM pending_dm_replies
         WHERE user_id = $1
           AND status = 'pending'
           AND (expires_at IS NULL OR expires_at > NOW())
         ORDER BY created_at ASC
         LIMIT 1",
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("query failed: {error}"))?;
    Ok(row.map(|row| PendingDmReplyRecord {
        id: row.get("id"),
        source_agent: row.get("source_agent"),
        context_json: row.get("context_json"),
        channel_id: row.get("channel_id"),
    }))
}

async fn load_most_recent_consumed_dm_reply_pg(
    pool: &PgPool,
    user_id: &str,
) -> Result<Option<PendingDmReplyRecord>, String> {
    let row = sqlx::query(
        "SELECT id, source_agent, context::text AS context_json, channel_id
         FROM pending_dm_replies
         WHERE user_id = $1
           AND status = 'consumed'
         ORDER BY consumed_at DESC NULLS LAST, id DESC
         LIMIT 1",
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("query failed: {error}"))?;
    Ok(row.map(|row| PendingDmReplyRecord {
        id: row.get("id"),
        source_agent: row.get("source_agent"),
        context_json: row.get("context_json"),
        channel_id: row.get("channel_id"),
    }))
}

async fn mark_pending_dm_reply_consumed_pg(
    pool: &PgPool,
    reply_id: i64,
    updated_context_json: &str,
) -> Result<bool, String> {
    let updated = sqlx::query(
        "UPDATE pending_dm_replies
         SET status = 'consumed',
             consumed_at = NOW(),
             context = CAST($1 AS jsonb)
         WHERE id = $2
           AND status = 'pending'",
    )
    .bind(updated_context_json)
    .bind(reply_id)
    .execute(pool)
    .await
    .map_err(|error| format!("update failed: {error}"))?;
    Ok(updated.rows_affected() > 0)
}

async fn consume_pending_dm_reply_pg(
    pool: &PgPool,
    user_id: &str,
) -> Result<ConsumePendingDmReplyResult, String> {
    let Some(record) = load_oldest_pending_dm_reply_pg(pool, user_id).await? else {
        return Ok(ConsumePendingDmReplyResult::NoPending);
    };

    let updated = mark_pending_dm_reply_consumed_pg(pool, record.id, &record.context_json).await?;
    if !updated {
        return Ok(ConsumePendingDmReplyResult::AlreadyConsumed);
    }

    Ok(ConsumePendingDmReplyResult::Consumed(record))
}

fn dm_reply_record_json(record: PendingDmReplyRecord, ok: bool) -> String {
    let ctx: serde_json::Value =
        serde_json::from_str(&record.context_json).unwrap_or(serde_json::json!({}));
    let mut resp = serde_json::json!({
        "ok": ok,
        "id": record.id,
        "sourceAgent": record.source_agent,
        "context": ctx,
    });
    if let Some(channel_id) = record.channel_id {
        resp["channelId"] = serde_json::Value::String(channel_id);
    }
    serde_json::to_string(&resp).unwrap_or_else(|_| r#"{"error":"serialize"}"#.to_string())
}

fn run_async_bridge_pg<F, T>(
    pool: &PgPool,
    future_factory: impl FnOnce(PgPool) -> F + Send + 'static,
) -> Result<T, String>
where
    F: Future<Output = Result<T, String>> + Send + 'static,
    T: Send + 'static,
{
    crate::utils::async_bridge::block_on_pg_result(pool, future_factory, |error| error)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDatabase {
        admin_url: String,
        database_name: String,
        database_url: String,
    }

    impl TestDatabase {
        async fn create() -> Self {
            let admin_url = admin_database_url();
            let database_name = format!("agentdesk_pg_{}", uuid::Uuid::new_v4().simple());
            let database_url = format!("{}/{}", base_database_url(), database_name);
            crate::db::postgres::create_test_database(
                &admin_url,
                &database_name,
                "dm reply pg tests",
            )
            .await
            .expect("create postgres test db");

            Self {
                admin_url,
                database_name,
                database_url,
            }
        }

        async fn migrate(&self) -> sqlx::PgPool {
            crate::db::postgres::connect_test_pool_and_migrate(
                &self.database_url,
                "dm reply pg tests",
            )
            .await
            .expect("migrate postgres test db")
        }

        async fn drop(self) {
            crate::db::postgres::drop_test_database(
                &self.admin_url,
                &self.database_name,
                "dm reply pg tests",
            )
            .await
            .expect("drop postgres test db");
        }
    }

    fn base_database_url() -> String {
        if let Ok(base) = std::env::var("POSTGRES_TEST_DATABASE_URL_BASE") {
            let trimmed = base.trim();
            if !trimmed.is_empty() {
                return trimmed.trim_end_matches('/').to_string();
            }
        }

        let user = std::env::var("PGUSER")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| {
                std::env::var("USER")
                    .ok()
                    .filter(|value| !value.trim().is_empty())
            })
            .unwrap_or_else(|| "postgres".to_string());
        let password = std::env::var("PGPASSWORD")
            .ok()
            .filter(|value| !value.trim().is_empty());
        let host = std::env::var("PGHOST")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "localhost".to_string());
        let port = std::env::var("PGPORT")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "5432".to_string());

        match password {
            Some(password) => format!("postgresql://{user}:{password}@{host}:{port}"),
            None => format!("postgresql://{user}@{host}:{port}"),
        }
    }

    fn admin_database_url() -> String {
        let admin_db = std::env::var("POSTGRES_TEST_ADMIN_DB")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "postgres".to_string());
        format!("{}/{}", base_database_url(), admin_db)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dm_reply_bridge_pg_round_trip() {
        let test_db = TestDatabase::create().await;
        let pg_pool = test_db.migrate().await;
        let rt = rquickjs::Runtime::new().expect("quickjs runtime");
        let ctx = rquickjs::Context::full(&rt).expect("quickjs context");

        ctx.with(|ctx| {
            let globals = ctx.globals();
            let ad = Object::new(ctx.clone()).expect("agentdesk object");
            globals.set("agentdesk", ad).expect("install agentdesk");
            register_dm_reply_ops(&ctx, None, Some(pg_pool.clone())).expect("register dmReply ops");

            let raw: String = ctx
                .eval(
                    r#"
                    JSON.stringify((function() {
                        var registered = agentdesk.dmReply.register(
                            "family-counsel",
                            "pg-user-1",
                            { channelId: "1473922824350601297", question: "건강검진 했어?" },
                            3600
                        );
                        var pending = agentdesk.dmReply.pending("pg-user-1");
                        var consumed = agentdesk.dmReply.consume("pg-user-1");
                        var readConsumed = agentdesk.dmReply.readConsumed("pg-user-1");
                        return {
                            registered: registered,
                            pending: pending,
                            consumed: consumed,
                            readConsumed: readConsumed
                        };
                    })())
                    "#,
                )
                .expect("run dmReply bridge");
            let parsed: serde_json::Value =
                serde_json::from_str(&raw).expect("parse bridge response");

            assert_eq!(parsed["registered"]["ok"], true);
            assert_eq!(parsed["pending"]["ok"], true);
            assert_eq!(parsed["pending"]["sourceAgent"], "family-counsel");
            assert_eq!(parsed["pending"]["channelId"], "1473922824350601297");
            assert_eq!(parsed["consumed"]["ok"], true);
            assert_eq!(parsed["readConsumed"]["ok"], true);
            assert_eq!(
                parsed["readConsumed"]["context"]["question"],
                "건강검진 했어?"
            );
        });

        let status: String = sqlx::query_scalar(
            "SELECT status FROM pending_dm_replies WHERE user_id = $1 ORDER BY id DESC LIMIT 1",
        )
        .bind("pg-user-1")
        .fetch_one(&pg_pool)
        .await
        .expect("query pending_dm_replies status");
        assert_eq!(status, "consumed");

        pg_pool.close().await;
        test_db.drop().await;
    }
}
