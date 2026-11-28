use rquickjs::{Ctx, Function, Object, Result as JsResult, function::Opt};
use sqlx::PgPool;
use std::time::{Duration, Instant};

use super::{db_execute_raw, db_guard_raw, db_query_json_raw, db_query_raw};
use crate::utils::bridge_deadline::{ScopedWorkDeadline, work_deadline};

pub(in crate::engine::ops) fn register_db_ops<'js>(
    ctx: &Ctx<'js>,
    pg_pool: Option<PgPool>,
) -> JsResult<()> {
    let ad: Object<'js> = ctx.globals().get("agentdesk")?;
    let db_obj = Object::new(ctx.clone())?;

    ad.set(
        "withWorkDeadline",
        Function::new(
            ctx.clone(),
            |reserve_ms: u32, work: Function<'js>| -> JsResult<bool> {
                let Some(deadline) = work_deadline(Duration::from_millis(reserve_ms.into())) else {
                    return work.call(());
                };
                if Instant::now() >= deadline {
                    return Ok(true);
                }
                let _work = ScopedWorkDeadline::install(deadline);
                Ok(work.call::<_, bool>(())? || Instant::now() >= deadline)
            },
        )?,
    )?;

    // Internal: __db_query_raw(sql, params_json) → json_string
    let pg_q = pg_pool.clone();
    let query_raw = Function::new(
        ctx.clone(),
        rquickjs::function::MutFn::from(
            move |sql: String, params_json: String, Opt(reserve): Opt<u32>| -> String {
                db_query_raw(
                    pg_q.clone(),
                    &sql,
                    &params_json,
                    Duration::from_millis(reserve.unwrap_or(0).into()),
                )
            },
        ),
    )?;
    db_obj.set("__query_raw", query_raw)?;

    // Internal: __db_query_json_raw(sql, params_json) → json_string
    let pg_q_json = pg_pool.clone();
    let query_json_raw = Function::new(
        ctx.clone(),
        rquickjs::function::MutFn::from(move |sql: String, params_json: String| -> String {
            db_query_json_raw(pg_q_json.clone(), &sql, &params_json)
        }),
    )?;
    db_obj.set("__query_json_raw", query_json_raw)?;

    // Internal: __db_execute_raw(sql, params_json) → json_string
    let pg_e = pg_pool.clone();
    let execute_raw = Function::new(
        ctx.clone(),
        rquickjs::function::MutFn::from(
            move |sql: String, params_json: String, Opt(reserve): Opt<u32>| -> String {
                db_execute_raw(
                    pg_e.clone(),
                    &sql,
                    &params_json,
                    Duration::from_millis(reserve.unwrap_or(0).into()),
                )
            },
        ),
    )?;
    db_obj.set("__execute_raw", execute_raw)?;

    let guard_raw = Function::new(ctx.clone(), move |sql: String| -> String {
        db_guard_raw(&sql, "agentdesk.db.execute")
    })?;
    db_obj.set("__guard_raw", guard_raw)?;

    ad.set("db", db_obj)?;

    // JS wrappers that do JSON marshaling
    let _: rquickjs::Value = ctx.eval(
        r#"
        (function() {
            function unwrap(raw) {
                var result = JSON.parse(raw);
                if (result.error) {
                    var error = new Error(result.error);
                    if (result.deadline_exceeded === true) error.deadline_exceeded = true;
                    throw error;
                }
                return result;
            }

            agentdesk.db.query = function(sql, params, reserveMs) {
                return unwrap(agentdesk.db.__query_raw(sql, JSON.stringify(params || []), reserveMs || 0));
            };
            agentdesk.db.queryJson = function(sql, params) {
                return unwrap(agentdesk.db.__query_json_raw(sql, JSON.stringify(params || [])));
            };
            agentdesk.db.execute = function(sql, params, reserveMs) {
                var guard = JSON.parse(agentdesk.db.__guard_raw(sql));
                if (guard.blocked) {
                    agentdesk.log.warn(guard.warning);
                    throw new Error(guard.error);
                }
                // Direct write — db.execute remains synchronous by design.
                // dispatch.create and kanban.setStatus use intent/transition model;
                // converting db.execute to intents requires typed intents for each
                // mutation pattern (card_review_state, kv_meta, agents, etc.).
                return unwrap(agentdesk.db.__execute_raw(sql, JSON.stringify(params || []), reserveMs || 0));
            };
        })();
        undefined;
    "#,
    )?;

    Ok(())
}
