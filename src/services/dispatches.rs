use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::Row;

use crate::db::Db;
use crate::dispatch;
use crate::engine::PolicyEngine;
use crate::services::service_error::{ErrorCode, ServiceError, ServiceResult};

const VALID_DISPATCH_STATUSES: &[&str] =
    &["pending", "dispatched", "completed", "cancelled", "failed"];

#[derive(Clone)]
pub struct DispatchService {
    db: Db,
    engine: PolicyEngine,
}

pub struct CreateDispatchInput {
    pub kanban_card_id: String,
    pub to_agent_id: String,
    pub dispatch_type: Option<String>,
    pub title: String,
    pub context: Option<Value>,
    pub skip_outbox: Option<bool>,
}

pub struct CreateDispatchResult {
    pub dispatch: Value,
    pub status: StatusCode,
}

pub struct UpdateDispatchInput {
    pub status: Option<String>,
    pub result: Option<Value>,
}

impl DispatchService {
    pub fn new(db: Db, engine: PolicyEngine) -> Self {
        Self { db, engine }
    }

    pub fn list_dispatches(
        &self,
        status: Option<&str>,
        kanban_card_id: Option<&str>,
    ) -> ServiceResult<Vec<Value>> {
        if let Some(pool) = self.engine.pg_pool() {
            return list_dispatches_pg(pool, status, kanban_card_id).map_err(|error| {
                ServiceError::internal(error)
                    .with_code(ErrorCode::Database)
                    .with_operation("list_dispatches.pg")
            });
        }

        let conn = self.db.lock().map_err(|e| {
            ServiceError::internal(format!("{e}"))
                .with_code(ErrorCode::Database)
                .with_operation("list_dispatches.lock")
        })?;

        let mut sql = String::from(
            "SELECT id, kanban_card_id, from_agent_id, to_agent_id, dispatch_type, status, title, context, result, parent_dispatch_id, chain_depth, created_at, updated_at FROM task_dispatches WHERE 1=1",
        );
        let mut bind_values: Vec<String> = Vec::new();

        if let Some(status) = status {
            bind_values.push(status.to_string());
            sql.push_str(&format!(" AND status = ?{}", bind_values.len()));
        }
        if let Some(card_id) = kanban_card_id {
            bind_values.push(card_id.to_string());
            sql.push_str(&format!(" AND kanban_card_id = ?{}", bind_values.len()));
        }

        sql.push_str(" ORDER BY created_at DESC");

        let mut stmt = conn.prepare(&sql).map_err(|e| {
            ServiceError::internal(format!("prepare: {e}"))
                .with_code(ErrorCode::Database)
                .with_operation("list_dispatches.prepare")
        })?;

        let params_ref: Vec<&dyn libsql_rusqlite::types::ToSql> = bind_values
            .iter()
            .map(|value| value as &dyn libsql_rusqlite::types::ToSql)
            .collect();

        let rows = stmt
            .query_map(params_ref.as_slice(), dispatch_row_to_json)
            .map_err(|e| {
                ServiceError::internal(format!("{e}"))
                    .with_code(ErrorCode::Database)
                    .with_operation("list_dispatches.query")
            })?;

        Ok(rows.filter_map(|row| row.ok()).collect())
    }

    pub fn get_dispatch(&self, id: &str) -> ServiceResult<Value> {
        dispatch::load_dispatch_row_pg_first(&self.db, self.engine.pg_pool(), id)
            .map_err(|error| {
                ServiceError::internal(format!("{error}"))
                    .with_code(ErrorCode::Database)
                    .with_operation("get_dispatch.query")
                    .with_context("dispatch_id", id)
            })?
            .ok_or_else(|| {
                ServiceError::not_found("dispatch not found")
                    .with_code(ErrorCode::Dispatch)
                    .with_context("dispatch_id", id)
            })
    }

    pub async fn create_dispatch(
        &self,
        input: CreateDispatchInput,
    ) -> ServiceResult<CreateDispatchResult> {
        let dispatch_type = input
            .dispatch_type
            .unwrap_or_else(|| "implementation".to_string());
        let to_agent_id = if let Some(pg_pool) = self.engine.pg_pool() {
            resolve_dispatch_target_agent_id_with_pg(pg_pool, &input.to_agent_id)
                .await
                .unwrap_or_else(|| input.to_agent_id.clone())
        } else {
            self.db
                .lock()
                .ok()
                .and_then(|conn| {
                    resolve_dispatch_target_agent_id_on_conn(&conn, &input.to_agent_id)
                })
                .unwrap_or_else(|| input.to_agent_id.clone())
        };
        let context = input.context.unwrap_or_else(|| json!({}));
        let options = dispatch::DispatchCreateOptions {
            skip_outbox: input.skip_outbox.unwrap_or(false),
            sidecar_dispatch: context
                .get("sidecar_dispatch")
                .and_then(|value| value.as_bool())
                .unwrap_or(false)
                || context
                    .get("phase_gate")
                    .and_then(|value| value.as_object())
                    .is_some(),
        };

        let result = dispatch::create_dispatch_with_options(
            &self.db,
            self.engine.pg_pool(),
            &self.engine,
            &input.kanban_card_id,
            &to_agent_id,
            &dispatch_type,
            &input.title,
            &context,
            options,
        );

        match result {
            Ok(dispatch) => {
                let was_reused = dispatch
                    .get("__reused")
                    .and_then(|value| value.as_bool())
                    .unwrap_or(false);
                Ok(CreateDispatchResult {
                    dispatch,
                    status: if was_reused {
                        StatusCode::OK
                    } else {
                        StatusCode::CREATED
                    },
                })
            }
            Err(error) => {
                let message = format!("{error}");
                if message.contains("not found") {
                    Err(ServiceError::not_found(message).with_code(ErrorCode::Dispatch))
                } else if message.starts_with("Cannot create ")
                    || message.contains("already exists")
                {
                    Err(ServiceError::conflict(message).with_code(ErrorCode::Dispatch))
                } else {
                    Err(ServiceError::internal(message).with_code(ErrorCode::Dispatch))
                }
            }
        }
    }

    pub fn update_dispatch(&self, id: &str, input: UpdateDispatchInput) -> ServiceResult<Value> {
        if input.status.as_deref() == Some("completed") {
            let context = input.result.as_ref();

            // Guard: review dispatches MUST carry an explicit verdict.
            // /api/review-verdict validates this; this path (PATCH
            // /api/dispatches/:id) previously accepted `{items, notes}` without
            // a verdict, leaving review_state in `reviewing` until timeouts [C]
            // escalated the card to dilemma_pending (see #925 incident).
            if let Ok(Some(row)) =
                crate::dispatch::load_dispatch_row_pg_first(&self.db, self.engine.pg_pool(), id)
            {
                let dtype = row
                    .get("dispatch_type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if dtype == "review" {
                    let has_verdict = context
                        .and_then(|c| c.get("verdict").or_else(|| c.get("decision")))
                        .and_then(|v| v.as_str())
                        .is_some_and(|s| !s.is_empty());
                    if !has_verdict {
                        return Err(ServiceError::bad_request(
                            "review dispatch completion requires explicit verdict — use POST /api/review-verdict",
                        )
                        .with_code(ErrorCode::Validation)
                        .with_context("dispatch_id", id));
                    }
                }
            }

            let dispatch = dispatch::finalize_dispatch(&self.db, &self.engine, id, "api", context)
                .map_err(|error| {
                    let message = format!("{error}");
                    if message.contains("not found") {
                        ServiceError::not_found(message)
                            .with_code(ErrorCode::Dispatch)
                            .with_context("dispatch_id", id)
                    } else if message.contains("no agent execution evidence") {
                        ServiceError::bad_request(message)
                            .with_code(ErrorCode::Dispatch)
                            .with_context("dispatch_id", id)
                    } else {
                        ServiceError::internal(message)
                            .with_code(ErrorCode::Dispatch)
                            .with_context("dispatch_id", id)
                    }
                })?;
            crate::services::dispatches_followup::queue_dispatch_followup_sync(
                &self.db,
                self.engine.pg_pool(),
                id,
            );
            return Ok(dispatch);
        }

        if let Some(status) = input.status.as_deref()
            && !VALID_DISPATCH_STATUSES.contains(&status)
        {
            return Err(ServiceError::bad_request(format!(
                "invalid dispatch status '{}' — allowed values: {}",
                status,
                VALID_DISPATCH_STATUSES.join(", ")
            ))
            .with_code(ErrorCode::Validation)
            .with_context("dispatch_id", id)
            .with_context("status", status));
        }

        if let Some(status) = input.status {
            let changed = dispatch::set_dispatch_status_pg_first(
                &self.db,
                self.engine.pg_pool(),
                id,
                &status,
                input.result.as_ref(),
                "api_update_dispatch",
                None,
                false,
            )
            .map_err(|error| {
                ServiceError::internal(format!("{error}"))
                    .with_code(ErrorCode::Dispatch)
                    .with_operation("update_dispatch.set_status")
                    .with_context("dispatch_id", id)
            })?;
            if changed == 0 {
                return Err(ServiceError::not_found("dispatch not found")
                    .with_code(ErrorCode::Dispatch)
                    .with_context("dispatch_id", id));
            }
        } else if let Some(result) = input.result {
            let changed = if let Some(pool) = self.engine.pg_pool() {
                update_dispatch_result_pg(pool, id, &result).map_err(|error| {
                    ServiceError::internal(error)
                        .with_code(ErrorCode::Database)
                        .with_operation("update_dispatch.update_result_pg")
                        .with_context("dispatch_id", id)
                })?
            } else {
                let conn = self.db.lock().map_err(|e| {
                    ServiceError::internal(format!("{e}"))
                        .with_code(ErrorCode::Database)
                        .with_operation("update_dispatch.lock")
                        .with_context("dispatch_id", id)
                })?;
                let mut values: Vec<Box<dyn libsql_rusqlite::types::ToSql>> = Vec::new();
                let result_str = serde_json::to_string(&result).unwrap_or_default();
                let mut sets = vec!["result = ?1".to_string()];
                values.push(Box::new(result_str));
                sets.push("updated_at = datetime('now')".to_string());
                values.push(Box::new(id.to_string()));
                let params_ref: Vec<&dyn libsql_rusqlite::types::ToSql> =
                    values.iter().map(|value| value.as_ref()).collect();
                match conn.execute(
                    &format!(
                        "UPDATE task_dispatches SET {} WHERE id = ?2",
                        sets.join(", ")
                    ),
                    params_ref.as_slice(),
                ) {
                    Ok(changed) => changed,
                    Err(error) => {
                        return Err(ServiceError::internal(format!("{error}"))
                            .with_code(ErrorCode::Database)
                            .with_operation("update_dispatch.update_result")
                            .with_context("dispatch_id", id));
                    }
                }
            };

            if changed == 0 {
                return Err(ServiceError::not_found("dispatch not found")
                    .with_code(ErrorCode::Dispatch)
                    .with_context("dispatch_id", id));
            }
        } else {
            return Err(ServiceError::bad_request("no fields to update")
                .with_code(ErrorCode::Validation)
                .with_context("dispatch_id", id));
        }

        dispatch::load_dispatch_row_pg_first(&self.db, self.engine.pg_pool(), id)
            .map_err(|error| {
                ServiceError::internal(format!("{error}"))
                    .with_code(ErrorCode::Database)
                    .with_operation("update_dispatch.readback")
                    .with_context("dispatch_id", id)
            })?
            .ok_or_else(|| {
                ServiceError::not_found("dispatch not found")
                    .with_code(ErrorCode::Dispatch)
                    .with_context("dispatch_id", id)
            })
    }
}

fn resolve_dispatch_target_agent_id_on_conn(
    conn: &libsql_rusqlite::Connection,
    raw_target: &str,
) -> Option<String> {
    conn.query_row(
        "SELECT id FROM agents WHERE id = ?1 LIMIT 1",
        [raw_target],
        |row| row.get(0),
    )
    .ok()
    .or_else(|| {
        conn.query_row(
            "SELECT id FROM agents
             WHERE discord_channel_id = ?1
                OR discord_channel_alt = ?1
                OR discord_channel_cc = ?1
                OR discord_channel_cdx = ?1
             LIMIT 1",
            [raw_target],
            |row| row.get(0),
        )
        .ok()
    })
}

fn list_dispatches_pg(
    pool: &sqlx::PgPool,
    status: Option<&str>,
    kanban_card_id: Option<&str>,
) -> Result<Vec<Value>, String> {
    let status = status.map(|value| value.to_string());
    let kanban_card_id = kanban_card_id.map(|value| value.to_string());
    crate::utils::async_bridge::block_on_pg_result(
        pool,
        move |pool| async move {
            let rows = sqlx::query(
                "SELECT
                    id,
                    kanban_card_id,
                    from_agent_id,
                    to_agent_id,
                    dispatch_type,
                    status,
                    title,
                    context,
                    result,
                    parent_dispatch_id,
                    COALESCE(chain_depth, 0)::BIGINT AS chain_depth,
                    created_at::text AS created_at,
                    updated_at::text AS updated_at,
                    completed_at::text AS completed_at
                 FROM task_dispatches
                 WHERE ($1::text IS NULL OR status = $1)
                   AND ($2::text IS NULL OR kanban_card_id = $2)
                 ORDER BY created_at DESC",
            )
            .bind(status.as_deref())
            .bind(kanban_card_id.as_deref())
            .fetch_all(&pool)
            .await
            .map_err(|error| format!("list postgres dispatches: {error}"))?;

            rows.into_iter()
                .map(|row| dispatch_row_to_json_pg(&row))
                .collect()
        },
        |error| error,
    )
}

fn update_dispatch_result_pg(
    pool: &sqlx::PgPool,
    dispatch_id: &str,
    result: &Value,
) -> Result<usize, String> {
    let dispatch_id = dispatch_id.to_string();
    let result_json = serde_json::to_string(result)
        .map_err(|error| format!("serialize dispatch result {dispatch_id}: {error}"))?;
    crate::utils::async_bridge::block_on_pg_result(
        pool,
        move |pool| async move {
            let updated = sqlx::query(
                "UPDATE task_dispatches
                 SET result = $2, updated_at = NOW()
                 WHERE id = $1",
            )
            .bind(&dispatch_id)
            .bind(&result_json)
            .execute(&pool)
            .await
            .map_err(|error| format!("update postgres dispatch result {dispatch_id}: {error}"))?;
            Ok(updated.rows_affected() as usize)
        },
        |error| error,
    )
}

async fn resolve_dispatch_target_agent_id_with_pg(
    pool: &sqlx::PgPool,
    raw_target: &str,
) -> Option<String> {
    let exact_match = sqlx::query("SELECT id FROM agents WHERE id = $1 LIMIT 1")
        .bind(raw_target)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .and_then(|row| row.try_get::<String, _>("id").ok());
    if exact_match.is_some() {
        return exact_match;
    }

    sqlx::query(
        "SELECT id FROM agents
         WHERE discord_channel_id = $1
            OR discord_channel_alt = $1
            OR discord_channel_cc = $1
            OR discord_channel_cdx = $1
         LIMIT 1",
    )
    .bind(raw_target)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
    .and_then(|row| row.try_get::<String, _>("id").ok())
}

fn dispatch_row_to_json(row: &libsql_rusqlite::Row) -> libsql_rusqlite::Result<Value> {
    let status = row.get::<_, String>(5)?;
    let dispatch_type = row.get::<_, Option<String>>(4)?;
    let context_raw = row.get::<_, Option<String>>(7)?;
    let result_raw = row.get::<_, Option<String>>(8)?;
    let context = context_raw
        .as_deref()
        .and_then(|text| serde_json::from_str::<Value>(text).ok());
    let result = result_raw
        .as_deref()
        .and_then(|text| serde_json::from_str::<Value>(text).ok());
    let result_summary = dispatch::summarize_dispatch_result(
        dispatch_type.as_deref(),
        Some(status.as_str()),
        result.as_ref(),
        context.as_ref(),
    );
    let created_at = row.get::<_, Option<String>>(11).ok().flatten().or_else(|| {
        row.get::<_, Option<i64>>(11)
            .ok()
            .flatten()
            .map(|value| value.to_string())
    });
    let updated_at = row.get::<_, Option<String>>(12).ok().flatten().or_else(|| {
        row.get::<_, Option<i64>>(12)
            .ok()
            .flatten()
            .map(|value| value.to_string())
    });
    let completed_at = if status == "completed" {
        updated_at.clone()
    } else {
        None
    };

    Ok(json!({
        "id": row.get::<_, String>(0)?,
        "kanban_card_id": row.get::<_, Option<String>>(1)?,
        "from_agent_id": row.get::<_, Option<String>>(2)?,
        "to_agent_id": row.get::<_, Option<String>>(3)?,
        "dispatch_type": dispatch_type,
        "status": status,
        "title": row.get::<_, Option<String>>(6)?,
        "context": context,
        "result": result,
        "context_file": Value::Null,
        "result_file": Value::Null,
        "result_summary": result_summary,
        "parent_dispatch_id": row.get::<_, Option<String>>(9)?,
        "chain_depth": row.get::<_, i64>(10).unwrap_or(0),
        "created_at": created_at,
        "dispatched_at": row.get::<_, Option<String>>(11)
            .ok()
            .flatten()
            .or_else(|| {
                row.get::<_, Option<i64>>(11)
                    .ok()
                    .flatten()
                    .map(|value| value.to_string())
            }),
        "updated_at": updated_at,
        "completed_at": completed_at,
    }))
}

fn dispatch_row_to_json_pg(row: &sqlx::postgres::PgRow) -> Result<Value, String> {
    let status = row
        .try_get::<String, _>("status")
        .map_err(|error| format!("decode postgres dispatch status: {error}"))?;
    let dispatch_type = row
        .try_get::<Option<String>, _>("dispatch_type")
        .map_err(|error| format!("decode postgres dispatch type: {error}"))?;
    let context_raw = row
        .try_get::<Option<String>, _>("context")
        .map_err(|error| format!("decode postgres dispatch context: {error}"))?;
    let result_raw = row
        .try_get::<Option<String>, _>("result")
        .map_err(|error| format!("decode postgres dispatch result: {error}"))?;
    let context = context_raw
        .as_deref()
        .and_then(|text| serde_json::from_str::<Value>(text).ok());
    let result = result_raw
        .as_deref()
        .and_then(|text| serde_json::from_str::<Value>(text).ok());
    let result_summary = dispatch::summarize_dispatch_result(
        dispatch_type.as_deref(),
        Some(status.as_str()),
        result.as_ref(),
        context.as_ref(),
    );
    let created_at = row
        .try_get::<String, _>("created_at")
        .map_err(|error| format!("decode postgres dispatch created_at: {error}"))?;
    let updated_at = row
        .try_get::<String, _>("updated_at")
        .map_err(|error| format!("decode postgres dispatch updated_at: {error}"))?;
    let completed_at = row
        .try_get::<Option<String>, _>("completed_at")
        .map_err(|error| format!("decode postgres dispatch completed_at: {error}"))?
        .or_else(|| (status == "completed").then(|| updated_at.clone()));

    Ok(json!({
        "id": row.try_get::<String, _>("id").map_err(|error| format!("decode postgres dispatch id: {error}"))?,
        "kanban_card_id": row.try_get::<Option<String>, _>("kanban_card_id").map_err(|error| format!("decode postgres dispatch kanban_card_id: {error}"))?,
        "from_agent_id": row.try_get::<Option<String>, _>("from_agent_id").map_err(|error| format!("decode postgres dispatch from_agent_id: {error}"))?,
        "to_agent_id": row.try_get::<Option<String>, _>("to_agent_id").map_err(|error| format!("decode postgres dispatch to_agent_id: {error}"))?,
        "dispatch_type": dispatch_type,
        "status": status,
        "title": row.try_get::<Option<String>, _>("title").map_err(|error| format!("decode postgres dispatch title: {error}"))?,
        "context": context,
        "result": result,
        "context_file": Value::Null,
        "result_file": Value::Null,
        "result_summary": result_summary,
        "parent_dispatch_id": row.try_get::<Option<String>, _>("parent_dispatch_id").map_err(|error| format!("decode postgres dispatch parent_dispatch_id: {error}"))?,
        "chain_depth": row.try_get::<i64, _>("chain_depth").map_err(|error| format!("decode postgres dispatch chain_depth: {error}"))?,
        "created_at": created_at.clone(),
        "dispatched_at": Some(created_at),
        "updated_at": updated_at,
        "completed_at": completed_at,
    }))
}
