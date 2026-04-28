use crate::db::Db;
use rquickjs::{Ctx, Function, Object, Result as JsResult};
use serde_json::json;
use sqlx::{PgPool, Postgres, QueryBuilder, Row as SqlxRow};

pub(crate) const ADVANCE_REVIEW_ROUND_HINT_KEY: &str = "advance_review_round_on_next_review";

pub(super) fn register_review_ops<'js>(
    ctx: &Ctx<'js>,
    _db: Option<Db>,
    pg_pool: Option<PgPool>,
) -> JsResult<()> {
    let ad: Object<'js> = ctx.globals().get("agentdesk")?;
    let review_obj = Object::new(ctx.clone())?;

    let pg_verdict = pg_pool.clone();
    review_obj.set(
        "__getVerdictRaw",
        Function::new(ctx.clone(), move |card_id: String| -> String {
            match pg_verdict.as_ref() {
                Some(pool) => review_get_verdict_raw_pg(pool, &card_id),
                None => json!({
                    "error": "postgres backend is required for review.getVerdict"
                })
                .to_string(),
            }
        })?,
    )?;

    let pg_entry = pg_pool.clone();
    review_obj.set(
        "__entryContextRaw",
        Function::new(ctx.clone(), move |card_id: String| -> String {
            match pg_entry.as_ref() {
                Some(pool) => review_entry_context_raw_pg(pool, &card_id),
                None => json!({
                    "error": "postgres backend is required for review.entryContext"
                })
                .to_string(),
            }
        })?,
    )?;

    let pg_record = pg_pool.clone();
    review_obj.set(
        "__recordEntryRaw",
        Function::new(
            ctx.clone(),
            move |card_id: String, opts_json: String| -> String {
                match pg_record.as_ref() {
                    Some(pool) => review_record_entry_raw_pg(pool, &card_id, &opts_json),
                    None => json!({
                        "error": "postgres backend is required for review.recordEntry"
                    })
                    .to_string(),
                }
            },
        )?,
    )?;

    let pg_active_work = pg_pool;
    review_obj.set(
        "__hasActiveWorkRaw",
        Function::new(ctx.clone(), move |card_id: String| -> String {
            match pg_active_work.as_ref() {
                Some(pool) => review_has_active_work_raw_pg(pool, &card_id),
                None => json!({
                    "error": "postgres backend is required for review.hasActiveWork"
                })
                .to_string(),
            }
        })?,
    )?;

    ad.set("review", review_obj)?;

    ctx.eval::<(), _>(
        r#"
        (function() {
            agentdesk.review.getVerdict = function(cardId) {
                var result = JSON.parse(agentdesk.review.__getVerdictRaw(cardId || ""));
                if (result.error) throw new Error(result.error);
                return result.review || null;
            };
            agentdesk.review.entryContext = function(cardId) {
                var result = JSON.parse(agentdesk.review.__entryContextRaw(cardId || ""));
                if (result.error) throw new Error(result.error);
                return result.entry || null;
            };
            agentdesk.review.recordEntry = function(cardId, opts) {
                var result = JSON.parse(
                    agentdesk.review.__recordEntryRaw(cardId || "", JSON.stringify(opts || {}))
                );
                if (result.error) throw new Error(result.error);
                return result;
            };
            agentdesk.review.hasActiveWork = function(cardId) {
                var result = JSON.parse(agentdesk.review.__hasActiveWorkRaw(cardId || ""));
                if (result.error) throw new Error(result.error);
                return !!result.has_active_work;
            };
        })();
        "#,
    )?;

    Ok(())
}

/// Ensure a Rust-side error string is valid JSON so the JS wrapper's
/// `JSON.parse` does not fail with "unexpected token". Future errors from
/// `block_on_pg_result` come through as raw `String` (not JSON), whereas
/// runtime errors are already JSON-wrapped by the map_runtime_error closure —
/// this normalizer covers both paths.
pub(crate) fn ensure_js_error_json(raw: String) -> String {
    if raw.trim_start().starts_with('{') {
        raw
    } else {
        json!({ "error": raw }).to_string()
    }
}

fn metadata_requests_review_round_advance(metadata_raw: Option<&str>) -> bool {
    metadata_raw
        .filter(|raw| !raw.trim().is_empty())
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
        .and_then(|value| value.as_object().cloned())
        .and_then(|metadata| metadata.get(ADVANCE_REVIEW_ROUND_HINT_KEY).cloned())
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
}

fn review_get_verdict_raw_pg(pool: &PgPool, card_id: &str) -> String {
    let card_id = card_id.to_string();
    let result = crate::utils::async_bridge::block_on_pg_result(
        pool,
        move |bridge_pool| async move {
            let review = sqlx::query(
                "SELECT kc.id,
                        rs.review_round,
                        rs.state,
                        rs.pending_dispatch_id,
                        rs.last_verdict,
                        rs.last_decision,
                        rs.decided_by,
                        rs.decided_at::text AS decided_at,
                        rs.review_entered_at::text AS review_entered_at,
                        rs.updated_at::text AS updated_at,
                        (
                            SELECT td.result ->> 'verdict'
                            FROM task_dispatches td
                            WHERE td.kanban_card_id = kc.id
                              AND td.dispatch_type = 'review'
                              AND td.status = 'completed'
                            ORDER BY COALESCE(td.completed_at, td.updated_at) DESC
                            LIMIT 1
                        ) AS latest_dispatch_verdict
                 FROM kanban_cards kc
                 LEFT JOIN card_review_state rs ON rs.card_id = kc.id
                 WHERE kc.id = $1",
            )
            .bind(&card_id)
            .fetch_optional(&bridge_pool)
            .await
            .map_err(|error| format!("load postgres review verdict for {card_id}: {error}"))?
            .map(|row| -> Result<serde_json::Value, String> {
                let state = row
                    .try_get::<Option<String>, _>("state")
                    .map_err(|error| format!("decode review state for {card_id}: {error}"))?
                    .unwrap_or_else(|| "idle".to_string());
                let last_verdict = row
                    .try_get::<Option<String>, _>("last_verdict")
                    .map_err(|error| format!("decode last_verdict for {card_id}: {error}"))?;
                let latest_dispatch_verdict = row
                    .try_get::<Option<String>, _>("latest_dispatch_verdict")
                    .map_err(|error| {
                        format!("decode latest dispatch verdict for {card_id}: {error}")
                    })?;
                let verdict = last_verdict.clone().or(latest_dispatch_verdict.clone());
                let source = if last_verdict.is_some() {
                    "review_state"
                } else if latest_dispatch_verdict.is_some() {
                    "dispatch_result"
                } else {
                    "none"
                };
                Ok(json!({
                    "card_id": row.try_get::<String, _>("id").map_err(|error| format!("decode card id for {card_id}: {error}"))?,
                    "review_round": row.try_get::<Option<i64>, _>("review_round").map_err(|error| format!("decode review_round for {card_id}: {error}"))?.unwrap_or(0),
                    "state": state,
                    "verdict": verdict,
                    "pending_dispatch_id": row.try_get::<Option<String>, _>("pending_dispatch_id").map_err(|error| format!("decode pending_dispatch_id for {card_id}: {error}"))?,
                    "decision": row.try_get::<Option<String>, _>("last_decision").map_err(|error| format!("decode last_decision for {card_id}: {error}"))?,
                    "decided_by": row.try_get::<Option<String>, _>("decided_by").map_err(|error| format!("decode decided_by for {card_id}: {error}"))?,
                    "decided_at": row.try_get::<Option<String>, _>("decided_at").map_err(|error| format!("decode decided_at for {card_id}: {error}"))?,
                    "review_entered_at": row.try_get::<Option<String>, _>("review_entered_at").map_err(|error| format!("decode review_entered_at for {card_id}: {error}"))?,
                    "updated_at": row.try_get::<Option<String>, _>("updated_at").map_err(|error| format!("decode updated_at for {card_id}: {error}"))?,
                    "source": source,
                }))
            })
            .transpose()?;

            Ok(json!({ "review": review }).to_string())
        },
        |error| json!({ "error": error }).to_string(),
    );

    match result {
        Ok(value) => value,
        Err(raw) => ensure_js_error_json(raw),
    }
}

fn review_entry_context_raw_pg(pool: &PgPool, card_id: &str) -> String {
    if card_id.trim().is_empty() {
        return json!({ "error": "review.entryContext requires card_id" }).to_string();
    }

    let card_id = card_id.to_string();
    let result = crate::utils::async_bridge::block_on_pg_result(
        pool,
        move |bridge_pool| async move {
            let entry = sqlx::query(
                "SELECT COALESCE(kc.review_round, 0) AS current_round,
                        (
                            SELECT COUNT(*)
                            FROM task_dispatches td
                            WHERE td.kanban_card_id = kc.id
                              AND td.dispatch_type IN ('implementation', 'rework')
                              AND td.status = 'completed'
                        ) AS completed_work_count,
                        (
                            SELECT MAX(COALESCE(td.completed_at, td.updated_at))::text
                            FROM task_dispatches td
                            WHERE td.kanban_card_id = kc.id
                              AND td.dispatch_type IN ('implementation', 'rework')
                              AND td.status = 'completed'
                        ) AS latest_work_completed_at,
                        (
                            SELECT MAX(COALESCE(td.completed_at, td.updated_at))::text
                            FROM task_dispatches td
                            WHERE td.kanban_card_id = kc.id
                              AND td.dispatch_type = 'review'
                              AND td.status = 'completed'
                        ) AS latest_review_completed_at,
                        kc.metadata::text AS metadata
                 FROM kanban_cards kc
                 WHERE kc.id = $1",
            )
            .bind(&card_id)
            .fetch_optional(&bridge_pool)
            .await
            .map_err(|error| format!("load postgres review entry context for {card_id}: {error}"))?
            .map(|row| -> Result<serde_json::Value, String> {
                let current_round = row
                    .try_get::<i64, _>("current_round")
                    .map_err(|error| format!("decode current_round for {card_id}: {error}"))?;
                let completed_work_count =
                    row.try_get::<i64, _>("completed_work_count")
                        .map_err(|error| {
                            format!("decode completed_work_count for {card_id}: {error}")
                        })?;
                let latest_work_completed_at = row
                    .try_get::<Option<String>, _>("latest_work_completed_at")
                    .map_err(|error| {
                        format!("decode latest_work_completed_at for {card_id}: {error}")
                    })?;
                let latest_review_completed_at = row
                    .try_get::<Option<String>, _>("latest_review_completed_at")
                    .map_err(|error| {
                        format!("decode latest_review_completed_at for {card_id}: {error}")
                    })?;
                let should_advance_round = current_round == 0
                    || completed_work_count > current_round
                    || matches!(
                        (
                            latest_work_completed_at.as_deref(),
                            latest_review_completed_at.as_deref()
                        ),
                        (Some(work), Some(review)) if work > review
                    )
                    || metadata_requests_review_round_advance(
                        row.try_get::<Option<String>, _>("metadata")
                            .map_err(|error| format!("decode metadata for {card_id}: {error}"))?
                            .as_deref(),
                    );
                let next_round = if should_advance_round {
                    current_round + 1
                } else {
                    current_round
                };
                Ok(json!({
                    "card_id": card_id,
                    "current_round": current_round,
                    "completed_work_count": completed_work_count,
                    "should_advance_round": should_advance_round,
                    "next_round": next_round,
                }))
            })
            .transpose()?;

            Ok(json!({ "entry": entry }).to_string())
        },
        |error| json!({ "error": error }).to_string(),
    );

    match result {
        Ok(value) => value,
        Err(raw) => ensure_js_error_json(raw),
    }
}

fn review_record_entry_raw_pg(pool: &PgPool, card_id: &str, opts_json: &str) -> String {
    if card_id.trim().is_empty() {
        return json!({ "error": "review.recordEntry requires card_id" }).to_string();
    }

    let opts: serde_json::Value = if opts_json.trim().is_empty() {
        serde_json::json!({})
    } else {
        match serde_json::from_str(opts_json) {
            Ok(opts) => opts,
            Err(error) => {
                return json!({ "error": format!("invalid review.recordEntry opts: {error}") })
                    .to_string();
            }
        }
    };

    let card_id = card_id.to_string();
    let result = crate::utils::async_bridge::block_on_pg_result(
        pool,
        move |bridge_pool| async move {
            let review_round = opts.get("review_round").and_then(|value| value.as_i64());
            let exclude_status = opts
                .get("exclude_status")
                .and_then(|value| value.as_str())
                .filter(|value| !value.trim().is_empty());

            let mut query =
                QueryBuilder::<Postgres>::new("UPDATE kanban_cards SET updated_at = NOW()");
            if let Some(review_round) = review_round {
                query.push(", review_round = ");
                query.push_bind(review_round);
            }
            query.push(" WHERE id = ");
            query.push_bind(card_id.clone());
            if let Some(exclude_status) = exclude_status {
                query.push(" AND status != ");
                query.push_bind(exclude_status.to_string());
            }

            let changed = query
                .build()
                .execute(&bridge_pool)
                .await
                .map_err(|error| format!("update postgres review entry for {card_id}: {error}"))?
                .rows_affected();

            clear_review_round_advance_hint_on_pg(&bridge_pool, &card_id).await?;

            Ok(json!({
                "ok": true,
                "rows_affected": changed,
                "changed": changed > 0,
            })
            .to_string())
        },
        |error| json!({ "error": error }).to_string(),
    );

    match result {
        Ok(value) => value,
        Err(raw) => ensure_js_error_json(raw),
    }
}

fn review_has_active_work_raw_pg(pool: &PgPool, card_id: &str) -> String {
    if card_id.trim().is_empty() {
        return json!({ "error": "review.hasActiveWork requires card_id" }).to_string();
    }

    let card_id = card_id.to_string();
    let result = crate::utils::async_bridge::block_on_pg_result(
        pool,
        move |bridge_pool| async move {
            let active_count = sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*)
                 FROM task_dispatches
                 WHERE kanban_card_id = $1
                   AND dispatch_type IN ('implementation', 'rework')
                   AND status IN ('pending', 'dispatched')",
            )
            .bind(&card_id)
            .fetch_one(&bridge_pool)
            .await
            .map_err(|error| format!("load postgres active work for {card_id}: {error}"))?;
            Ok(json!({ "has_active_work": active_count > 0 }).to_string())
        },
        |error| json!({ "error": error }).to_string(),
    );

    match result {
        Ok(value) => value,
        Err(raw) => ensure_js_error_json(raw),
    }
}

async fn clear_review_round_advance_hint_on_pg(pool: &PgPool, card_id: &str) -> Result<(), String> {
    let metadata_raw =
        sqlx::query_scalar::<_, Option<String>>("SELECT metadata FROM kanban_cards WHERE id = $1")
            .bind(card_id)
            .fetch_optional(pool)
            .await
            .map_err(|error| format!("load postgres review metadata for {card_id}: {error}"))?
            .flatten();
    let Some(raw) = metadata_raw.filter(|value| !value.trim().is_empty()) else {
        return Ok(());
    };
    let Ok(mut metadata) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return Ok(());
    };
    let Some(object) = metadata.as_object_mut() else {
        return Ok(());
    };
    if object.remove(ADVANCE_REVIEW_ROUND_HINT_KEY).is_none() {
        return Ok(());
    }

    let stored_metadata = if object.is_empty() {
        None
    } else {
        Some(metadata.to_string())
    };
    sqlx::query(
        "UPDATE kanban_cards
         SET metadata = $1,
             updated_at = NOW()
         WHERE id = $2",
    )
    .bind(stored_metadata)
    .bind(card_id)
    .execute(pool)
    .await
    .map_err(|error| format!("clear postgres review hint metadata for {card_id}: {error}"))?;
    Ok(())
}
