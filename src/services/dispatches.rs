use axum::http::StatusCode;
use serde_json::{Value, json};

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

        let params_ref: Vec<&dyn rusqlite::types::ToSql> = bind_values
            .iter()
            .map(|value| value as &dyn rusqlite::types::ToSql)
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
        let conn = self.db.lock().map_err(|e| {
            ServiceError::internal(format!("{e}"))
                .with_code(ErrorCode::Database)
                .with_operation("get_dispatch.lock")
                .with_context("dispatch_id", id)
        })?;

        conn.query_row(
            "SELECT id, kanban_card_id, from_agent_id, to_agent_id, dispatch_type, status, title, context, result, parent_dispatch_id, chain_depth, created_at, updated_at FROM task_dispatches WHERE id = ?1",
            [id],
            dispatch_row_to_json,
        )
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => ServiceError::not_found("dispatch not found")
                .with_code(ErrorCode::Dispatch)
                .with_context("dispatch_id", id),
            other => ServiceError::internal(format!("{other}"))
                .with_code(ErrorCode::Database)
                .with_operation("get_dispatch.query")
                .with_context("dispatch_id", id),
        })
    }

    pub fn create_dispatch(
        &self,
        input: CreateDispatchInput,
    ) -> ServiceResult<CreateDispatchResult> {
        let dispatch_type = input
            .dispatch_type
            .unwrap_or_else(|| "implementation".to_string());
        let context = input.context.unwrap_or_else(|| json!({}));
        let options = dispatch::DispatchCreateOptions {
            skip_outbox: input.skip_outbox.unwrap_or(false),
            ..Default::default()
        };

        match dispatch::create_dispatch_with_options(
            &self.db,
            &self.engine,
            &input.kanban_card_id,
            &input.to_agent_id,
            &dispatch_type,
            &input.title,
            &context,
            options,
        ) {
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
            crate::server::routes::dispatches::queue_dispatch_followup(&self.db, id);
            return Ok(dispatch);
        }

        if let Some(status) = input.status.as_deref() {
            if !VALID_DISPATCH_STATUSES.contains(&status) {
                return Err(ServiceError::bad_request(format!(
                    "invalid dispatch status '{}' — allowed values: {}",
                    status,
                    VALID_DISPATCH_STATUSES.join(", ")
                ))
                .with_code(ErrorCode::Validation)
                .with_context("dispatch_id", id)
                .with_context("status", status));
            }
        }

        let conn = self.db.lock().map_err(|e| {
            ServiceError::internal(format!("{e}"))
                .with_code(ErrorCode::Database)
                .with_operation("update_dispatch.lock")
                .with_context("dispatch_id", id)
        })?;

        if let Some(status) = input.status {
            let changed = dispatch::set_dispatch_status_on_conn(
                &conn,
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
            let mut values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
            let result_str = serde_json::to_string(&result).unwrap_or_default();
            let mut sets = vec!["result = ?1".to_string()];
            values.push(Box::new(result_str));
            sets.push("updated_at = datetime('now')".to_string());
            values.push(Box::new(id.to_string()));
            let params_ref: Vec<&dyn rusqlite::types::ToSql> =
                values.iter().map(|value| value.as_ref()).collect();
            match conn.execute(
                &format!(
                    "UPDATE task_dispatches SET {} WHERE id = ?2",
                    sets.join(", ")
                ),
                params_ref.as_slice(),
            ) {
                Ok(0) => {
                    return Err(ServiceError::not_found("dispatch not found")
                        .with_code(ErrorCode::Dispatch)
                        .with_context("dispatch_id", id));
                }
                Ok(_) => {}
                Err(error) => {
                    return Err(ServiceError::internal(format!("{error}"))
                        .with_code(ErrorCode::Database)
                        .with_operation("update_dispatch.update_result")
                        .with_context("dispatch_id", id));
                }
            }
        } else {
            return Err(ServiceError::bad_request("no fields to update")
                .with_code(ErrorCode::Validation)
                .with_context("dispatch_id", id));
        }

        conn.query_row(
            "SELECT id, kanban_card_id, from_agent_id, to_agent_id, dispatch_type, status, title, context, result, parent_dispatch_id, chain_depth, created_at, updated_at FROM task_dispatches WHERE id = ?1",
            [id],
            dispatch_row_to_json,
        )
        .map_err(|error| {
            ServiceError::internal(format!("{error}"))
                .with_code(ErrorCode::Database)
                .with_operation("update_dispatch.readback")
                .with_context("dispatch_id", id)
        })
    }
}

fn dispatch_row_to_json(row: &rusqlite::Row) -> rusqlite::Result<Value> {
    let status = row.get::<_, String>(5)?;
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
        "dispatch_type": row.get::<_, Option<String>>(4)?,
        "status": status,
        "title": row.get::<_, Option<String>>(6)?,
        "context": row.get::<_, Option<String>>(7)?
            .and_then(|text| serde_json::from_str::<Value>(&text).ok()),
        "result": row.get::<_, Option<String>>(8)?
            .and_then(|text| serde_json::from_str::<Value>(&text).ok()),
        "context_file": Value::Null,
        "result_file": Value::Null,
        "result_summary": Value::Null,
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
