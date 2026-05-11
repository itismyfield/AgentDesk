use crate::db::Db;
use rquickjs::{Ctx, Function, Object, Result as JsResult};
use sqlx::PgPool;

// ── Message queue ops ─────────────────────────────────────────────
// agentdesk.message.queue(target, content, bot?, source?)
// Enqueues a message for async delivery — avoids self-referential HTTP deadlock (#120)

pub(super) fn register_message_ops<'js>(ctx: &Ctx<'js>, pg_pool: Option<PgPool>) -> JsResult<()> {
    let ad: Object<'js> = ctx.globals().get("agentdesk")?;
    let msg_obj = Object::new(ctx.clone())?;

    // __queue_raw(target, content, bot, source) → json_string
    let pg_clone = pg_pool.clone();
    let queue_raw = Function::new(
        ctx.clone(),
        rquickjs::function::MutFn::from(
            move |target: String, content: String, bot: String, source: String| -> String {
                message_queue_raw(pg_clone.as_ref(), &target, &content, &bot, &source)
            },
        ),
    )?;
    msg_obj.set("__queue_raw", queue_raw)?;

    ad.set("message", msg_obj)?;

    // JS wrapper: agentdesk.message.queue(target, content, bot?, source?)
    ctx.eval::<(), _>(
        r#"
        agentdesk.message.queue = function(target, content, bot, source) {
            return JSON.parse(agentdesk.message.__queue_raw(
                target || "",
                content || "",
                bot || "announce",
                source || "system"
            ));
        };
        "#,
    )?;

    Ok(())
}

pub(crate) fn queue_message(
    _db: Option<&Db>,
    pg_pool: Option<&PgPool>,
    target: &str,
    content: &str,
    bot: &str,
    source: &str,
) -> Result<i64, String> {
    if let Some(pool) = pg_pool {
        return crate::utils::async_bridge::block_on_pg_result(
            pool,
            {
                let target = target.to_string();
                let content = content.to_string();
                let bot = bot.to_string();
                let source = source.to_string();
                move |bridge_pool| async move {
                    sqlx::query_scalar::<_, i64>(
                        "INSERT INTO message_outbox (target, content, bot, source)
                         VALUES ($1, $2, $3, $4)
                         RETURNING id",
                    )
                    .bind(&target)
                    .bind(&content)
                    .bind(&bot)
                    .bind(&source)
                    .fetch_one(&bridge_pool)
                    .await
                    .map_err(|e| format!("insert postgres message_outbox: {e}"))
                }
            },
            |error| error,
        );
    }

    Err("sqlite backend is unavailable".to_string())
}

fn message_queue_raw(
    pg_pool: Option<&PgPool>,
    target: &str,
    content: &str,
    bot: &str,
    source: &str,
) -> String {
    match queue_message(None, pg_pool, target, content, bot, source) {
        Ok(id) => {
            tracing::info!(
                target,
                bot,
                source,
                message_id = id,
                "queued message from JS bridge"
            );
            format!(r#"{{"ok":true,"id":{id}}}"#)
        }
        // #2045 Finding 8 (P2): properly escape arbitrary error text so the
        // JS wrapper's `JSON.parse` cannot trip on backslashes / newlines
        // that Postgres or sqlx error messages may include.
        Err(error) => crate::engine::ops::ensure_js_error_json(error),
    }
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
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
            let database_name = format!("agentdesk_message_ops_{}", uuid::Uuid::new_v4().simple());
            let database_url = format!("{}/{}", base_database_url(), database_name);
            crate::db::postgres::create_test_database(
                &admin_url,
                &database_name,
                "message ops pg tests",
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
                "message ops pg tests",
            )
            .await
            .expect("migrate postgres test db")
        }

        async fn drop(self) {
            crate::db::postgres::drop_test_database(
                &self.admin_url,
                &self.database_name,
                "message ops pg tests",
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
    async fn queue_message_pg_inserts_into_postgres_without_touching_sqlite() {
        let test_db = TestDatabase::create().await;
        let pool = test_db.migrate().await;
        let sqlite_db = crate::db::test_db();

        let id = queue_message(
            Some(&sqlite_db),
            Some(&pool),
            "channel:alerts",
            "hello from pg",
            "announce",
            "policy-test",
        )
        .expect("queue message through postgres");

        let row: (i64, String, String, String, String) = sqlx::query_as(
            "SELECT id, target, content, bot, source
             FROM message_outbox
             WHERE id = $1",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .expect("load postgres message_outbox row");
        assert_eq!(row.0, id);
        assert_eq!(row.1, "channel:alerts");
        assert_eq!(row.2, "hello from pg");
        assert_eq!(row.3, "announce");
        assert_eq!(row.4, "policy-test");

        let sqlite_count: i64 = sqlite_db
            .read_conn()
            .expect("sqlite read conn")
            .query_row("SELECT COUNT(*) FROM message_outbox", [], |row| row.get(0))
            .expect("count sqlite message_outbox rows");
        assert_eq!(sqlite_count, 0);

        pool.close().await;
        test_db.drop().await;
    }
}
