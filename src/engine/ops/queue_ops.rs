use crate::db::Db;
use rquickjs::{Ctx, Function, Object, Result as JsResult};
use serde_json::json;
use sqlx::PgPool;

pub(super) fn register_queue_ops<'js>(
    ctx: &Ctx<'js>,
    db: Option<Db>,
    pg_pool: Option<PgPool>,
) -> JsResult<()> {
    #[cfg(not(test))]
    let _ = &db;
    let ad: Object<'js> = ctx.globals().get("agentdesk")?;
    let queue_obj = Object::new(ctx.clone())?;

    #[cfg(test)]
    let db_status = db;
    let pg_status = pg_pool;
    queue_obj.set(
        "__statusRaw",
        Function::new(ctx.clone(), move || -> String {
            if let Some(pool) = pg_status.as_ref() {
                return queue_status_raw_pg(pool);
            }
            #[cfg(test)]
            if let Some(db_status) = db_status.as_ref() {
                return queue_status_raw_sqlite_test(db_status);
            }
            json!({ "error": "sqlite backend is unavailable" }).to_string()
        })?,
    )?;

    ad.set("queue", queue_obj)?;

    ctx.eval::<(), _>(
        r#"
        (function() {
            agentdesk.queue.status = function() {
                var result = JSON.parse(agentdesk.queue.__statusRaw());
                if (result.error) throw new Error(result.error);
                return result;
            };
        })();
        "#,
    )?;

    Ok(())
}

fn queue_status_raw_pg(pool: &PgPool) -> String {
    let result = crate::utils::async_bridge::block_on_pg_result(
        pool,
        move |bridge_pool| async move {
            let has_auto_runs = table_exists_pg(&bridge_pool, "auto_queue_runs").await?;
            let has_auto_entries = table_exists_pg(&bridge_pool, "auto_queue_entries").await?;

            Ok(json!({
                "dispatches": {
                    "pending": count_pg(&bridge_pool, "SELECT COUNT(*) FROM task_dispatches WHERE status = 'pending'").await?,
                    "dispatched": count_pg(&bridge_pool, "SELECT COUNT(*) FROM task_dispatches WHERE status = 'dispatched'").await?,
                },
                "legacy_dispatch_queue": {
                    "queued": count_pg(&bridge_pool, "SELECT COUNT(*) FROM dispatch_queue").await?,
                },
                "message_outbox": {
                    "pending": count_pg(&bridge_pool, "SELECT COUNT(*) FROM message_outbox WHERE status = 'pending'").await?,
                    "failed": count_pg(&bridge_pool, "SELECT COUNT(*) FROM message_outbox WHERE status = 'failed'").await?,
                },
                "dispatch_outbox": {
                    "pending": count_pg(&bridge_pool, "SELECT COUNT(*) FROM dispatch_outbox WHERE status = 'pending'").await?,
                    "processing": count_pg(&bridge_pool, "SELECT COUNT(*) FROM dispatch_outbox WHERE status = 'processing'").await?,
                    "failed": count_pg(&bridge_pool, "SELECT COUNT(*) FROM dispatch_outbox WHERE status = 'failed'").await?,
                },
                "auto_queue": {
                    "active_runs": if has_auto_runs {
                        count_pg(&bridge_pool, "SELECT COUNT(*) FROM auto_queue_runs WHERE status = 'active'").await?
                    } else {
                        0
                    },
                    "paused_runs": if has_auto_runs {
                        count_pg(&bridge_pool, "SELECT COUNT(*) FROM auto_queue_runs WHERE status = 'paused'").await?
                    } else {
                        0
                    },
                    "pending_entries": if has_auto_entries {
                        count_pg(&bridge_pool, "SELECT COUNT(*) FROM auto_queue_entries WHERE status = 'pending'").await?
                    } else {
                        0
                    },
                    "dispatched_entries": if has_auto_entries {
                        count_pg(&bridge_pool, "SELECT COUNT(*) FROM auto_queue_entries WHERE status = 'dispatched'").await?
                    } else {
                        0
                    },
                    "done_entries": if has_auto_entries {
                        count_pg(&bridge_pool, "SELECT COUNT(*) FROM auto_queue_entries WHERE status = 'done'").await?
                    } else {
                        0
                    },
                }
            })
            .to_string())
        },
        |error| json!({ "error": error }).to_string(),
    );

    match result {
        Ok(value) => value,
        Err(raw) => crate::engine::ops::ensure_js_error_json(raw),
    }
}

#[cfg(test)]
fn queue_status_raw_sqlite_test(db: &Db) -> String {
    let result = (|| -> anyhow::Result<serde_json::Value> {
        let conn = db.read_conn()?;
        let count = |sql: &str| -> anyhow::Result<i64> {
            conn.query_row(sql, [], |row| row.get(0))
                .map_err(anyhow::Error::from)
        };
        let table_exists = |table: &str| -> anyhow::Result<bool> {
            conn.query_row(
                "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type = 'table' AND name = ?1",
                [table],
                |row| row.get(0),
            )
            .map_err(anyhow::Error::from)
        };
        let has_auto_runs = table_exists("auto_queue_runs")?;
        let has_auto_entries = table_exists("auto_queue_entries")?;

        Ok(json!({
            "dispatches": {
                "pending": count("SELECT COUNT(*) FROM task_dispatches WHERE status = 'pending'")?,
                "dispatched": count("SELECT COUNT(*) FROM task_dispatches WHERE status = 'dispatched'")?,
            },
            "legacy_dispatch_queue": {
                "queued": count("SELECT COUNT(*) FROM dispatch_queue")?,
            },
            "message_outbox": {
                "pending": count("SELECT COUNT(*) FROM message_outbox WHERE status = 'pending'")?,
                "failed": count("SELECT COUNT(*) FROM message_outbox WHERE status = 'failed'")?,
            },
            "dispatch_outbox": {
                "pending": count("SELECT COUNT(*) FROM dispatch_outbox WHERE status = 'pending'")?,
                "processing": count("SELECT COUNT(*) FROM dispatch_outbox WHERE status = 'processing'")?,
                "failed": count("SELECT COUNT(*) FROM dispatch_outbox WHERE status = 'failed'")?,
            },
            "auto_queue": {
                "active_runs": if has_auto_runs {
                    count("SELECT COUNT(*) FROM auto_queue_runs WHERE status = 'active'")?
                } else {
                    0
                },
                "paused_runs": if has_auto_runs {
                    count("SELECT COUNT(*) FROM auto_queue_runs WHERE status = 'paused'")?
                } else {
                    0
                },
                "pending_entries": if has_auto_entries {
                    count("SELECT COUNT(*) FROM auto_queue_entries WHERE status = 'pending'")?
                } else {
                    0
                },
                "dispatched_entries": if has_auto_entries {
                    count("SELECT COUNT(*) FROM auto_queue_entries WHERE status = 'dispatched'")?
                } else {
                    0
                },
                "done_entries": if has_auto_entries {
                    count("SELECT COUNT(*) FROM auto_queue_entries WHERE status = 'done'")?
                } else {
                    0
                },
            }
        }))
    })();

    match result {
        Ok(value) => value.to_string(),
        Err(err) => json!({ "error": err.to_string() }).to_string(),
    }
}

async fn count_pg(pool: &PgPool, sql: &str) -> Result<i64, String> {
    sqlx::query_scalar::<_, i64>(sql)
        .fetch_one(pool)
        .await
        .map_err(|error| format!("count postgres rows for `{sql}`: {error}"))
}

async fn table_exists_pg(pool: &PgPool, table: &str) -> Result<bool, String> {
    let regclass_name = format!("public.{table}");
    sqlx::query_scalar::<_, bool>("SELECT to_regclass($1) IS NOT NULL")
        .bind(regclass_name)
        .fetch_one(pool)
        .await
        .map_err(|error| format!("check postgres table {table}: {error}"))
}
