use std::sync::Arc;

use libsql_rusqlite::OptionalExtension;
use serde_json::{Value, json};
use sqlx::{PgPool, Postgres, QueryBuilder};

use crate::db::Db;
use crate::services::discord::health::HealthRegistry;
use crate::services::provider::ProviderKind;
use crate::services::service_error::{ErrorCode, ServiceError, ServiceResult};
use crate::services::turn_lifecycle::{TurnLifecycleTarget, stop_turn_preserving_queue};
use poise::serenity_prelude::ChannelId;

#[derive(Clone)]
pub struct QueueService {
    db: Db,
    pg_pool: Option<PgPool>,
}

impl QueueService {
    pub fn new(db: Db, pg_pool: Option<PgPool>) -> Self {
        Self { db, pg_pool }
    }

    pub async fn cancel_dispatch(&self, dispatch_id: &str) -> ServiceResult<Value> {
        if let Some(pool) = self.pg_pool.as_ref() {
            return self.cancel_dispatch_pg(pool, dispatch_id).await;
        }

        self.cancel_dispatch_sqlite(dispatch_id)
    }

    fn cancel_dispatch_sqlite(&self, dispatch_id: &str) -> ServiceResult<Value> {
        let conn = self.db.lock().map_err(|e| {
            ServiceError::internal(format!("{e}"))
                .with_code(ErrorCode::Database)
                .with_operation("cancel_dispatch.lock")
                .with_context("dispatch_id", dispatch_id)
        })?;

        let current_status: Option<String> = conn
            .query_row(
                "SELECT status FROM task_dispatches WHERE id = ?1",
                [dispatch_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| {
                ServiceError::internal(format!("load dispatch status: {error}"))
                    .with_code(ErrorCode::Database)
                    .with_operation("cancel_dispatch.query_status")
                    .with_context("dispatch_id", dispatch_id)
            })?;

        match current_status.as_deref() {
            None => Err(ServiceError::not_found("dispatch not found")
                .with_code(ErrorCode::Dispatch)
                .with_context("dispatch_id", dispatch_id)),
            Some("completed") | Some("cancelled") | Some("failed") => {
                Err(ServiceError::conflict(format!(
                    "dispatch already in terminal state: {}",
                    current_status.unwrap_or_default()
                ))
                .with_code(ErrorCode::Dispatch)
                .with_context("dispatch_id", dispatch_id))
            }
            Some(_) => {
                crate::dispatch::cancel_dispatch_and_reset_auto_queue_on_conn(
                    &conn,
                    dispatch_id,
                    None,
                )
                .ok();
                conn.execute(
                    "DELETE FROM kv_meta WHERE key = ?1",
                    [&format!("dispatch_notified:{dispatch_id}")],
                )
                .ok();

                tracing::info!("[queue-api] Cancelled dispatch {dispatch_id}");
                Ok(json!({"ok": true, "dispatch_id": dispatch_id}))
            }
        }
    }

    async fn cancel_dispatch_pg(&self, pool: &PgPool, dispatch_id: &str) -> ServiceResult<Value> {
        let current_status =
            sqlx::query_scalar::<_, String>("SELECT status FROM task_dispatches WHERE id = $1")
                .bind(dispatch_id)
                .fetch_optional(pool)
                .await
                .map_err(|error| {
                    ServiceError::internal(format!("load postgres dispatch status: {error}"))
                        .with_code(ErrorCode::Database)
                        .with_operation("cancel_dispatch.query_status_pg")
                        .with_context("dispatch_id", dispatch_id)
                })?;

        match current_status.as_deref() {
            None => Err(ServiceError::not_found("dispatch not found")
                .with_code(ErrorCode::Dispatch)
                .with_context("dispatch_id", dispatch_id)),
            Some("completed") | Some("cancelled") | Some("failed") => {
                Err(ServiceError::conflict(format!(
                    "dispatch already in terminal state: {}",
                    current_status.unwrap_or_default()
                ))
                .with_code(ErrorCode::Dispatch)
                .with_context("dispatch_id", dispatch_id))
            }
            Some(_) => {
                crate::dispatch::cancel_dispatch_and_reset_auto_queue_on_pg(
                    pool,
                    dispatch_id,
                    None,
                )
                .await
                .map_err(|error| {
                    ServiceError::internal(format!("cancel postgres dispatch: {error}"))
                        .with_code(ErrorCode::Database)
                        .with_operation("cancel_dispatch.cancel_pg")
                        .with_context("dispatch_id", dispatch_id)
                })?;

                sqlx::query("DELETE FROM kv_meta WHERE key = $1")
                    .bind(format!("dispatch_notified:{dispatch_id}"))
                    .execute(pool)
                    .await
                    .map_err(|error| {
                        ServiceError::internal(format!(
                            "clear postgres dispatch notify guard: {error}"
                        ))
                        .with_code(ErrorCode::Database)
                        .with_operation("cancel_dispatch.clear_guard_pg")
                        .with_context("dispatch_id", dispatch_id)
                    })?;

                tracing::info!("[queue-api] Cancelled dispatch {dispatch_id}");
                Ok(json!({"ok": true, "dispatch_id": dispatch_id}))
            }
        }
    }

    pub async fn cancel_all_dispatches(
        &self,
        kanban_card_id: Option<&str>,
        agent_id: Option<&str>,
    ) -> ServiceResult<Value> {
        if let Some(pool) = self.pg_pool.as_ref() {
            return self
                .cancel_all_dispatches_pg(pool, kanban_card_id, agent_id)
                .await;
        }

        self.cancel_all_dispatches_sqlite(kanban_card_id, agent_id)
    }

    fn cancel_all_dispatches_sqlite(
        &self,
        kanban_card_id: Option<&str>,
        agent_id: Option<&str>,
    ) -> ServiceResult<Value> {
        let conn = self.db.lock().map_err(|e| {
            ServiceError::internal(format!("{e}"))
                .with_code(ErrorCode::Database)
                .with_operation("cancel_all_dispatches.lock")
        })?;

        let mut conditions = vec!["status IN ('pending', 'dispatched')".to_string()];
        let mut params: Vec<Box<dyn libsql_rusqlite::types::ToSql>> = Vec::new();

        if let Some(card_id) = kanban_card_id {
            params.push(Box::new(card_id.to_string()));
            conditions.push(format!("kanban_card_id = ?{}", params.len()));
        }
        if let Some(agent_id) = agent_id {
            params.push(Box::new(agent_id.to_string()));
            conditions.push(format!("to_agent_id = ?{}", params.len()));
        }

        let sql = format!(
            "SELECT id FROM task_dispatches WHERE {}",
            conditions.join(" AND ")
        );
        let param_refs: Vec<&dyn libsql_rusqlite::types::ToSql> =
            params.iter().map(|param| param.as_ref()).collect();
        let dispatch_ids: Vec<String> = {
            let mut stmt = conn.prepare(&sql).map_err(|error| {
                ServiceError::internal(format!("prepare cancel-all query: {error}"))
                    .with_code(ErrorCode::Database)
                    .with_operation("cancel_all_dispatches.prepare")
            })?;
            let rows = stmt
                .query_map(param_refs.as_slice(), |row| row.get::<_, String>(0))
                .map_err(|error| {
                    ServiceError::internal(format!("query cancel-all dispatches: {error}"))
                        .with_code(ErrorCode::Database)
                        .with_operation("cancel_all_dispatches.query")
                })?;
            rows.collect::<Result<Vec<_>, _>>().map_err(|error| {
                ServiceError::internal(format!("read cancel-all dispatch rows: {error}"))
                    .with_code(ErrorCode::Database)
                    .with_operation("cancel_all_dispatches.collect")
            })
        }?;

        let mut count = 0;
        for dispatch_id in &dispatch_ids {
            count += crate::dispatch::cancel_dispatch_and_reset_auto_queue_on_conn(
                &conn,
                dispatch_id,
                None,
            )
            .unwrap_or(0);
        }

        tracing::info!(
            "[queue-api] Cancelled {count} dispatches (card={:?}, agent={:?})",
            kanban_card_id,
            agent_id
        );
        Ok(json!({"ok": true, "cancelled": count}))
    }

    async fn cancel_all_dispatches_pg(
        &self,
        pool: &PgPool,
        kanban_card_id: Option<&str>,
        agent_id: Option<&str>,
    ) -> ServiceResult<Value> {
        let mut query = QueryBuilder::<Postgres>::new(
            "SELECT id FROM task_dispatches WHERE status IN ('pending', 'dispatched')",
        );

        if let Some(card_id) = kanban_card_id {
            query.push(" AND kanban_card_id = ");
            query.push_bind(card_id);
        }
        if let Some(agent_id) = agent_id {
            query.push(" AND to_agent_id = ");
            query.push_bind(agent_id);
        }

        let dispatch_ids = query
            .build_query_scalar::<String>()
            .fetch_all(pool)
            .await
            .map_err(|error| {
                ServiceError::internal(format!("query postgres cancel-all dispatches: {error}"))
                    .with_code(ErrorCode::Database)
                    .with_operation("cancel_all_dispatches.query_pg")
            })?;

        let mut count = 0usize;
        for dispatch_id in &dispatch_ids {
            count += crate::dispatch::cancel_dispatch_and_reset_auto_queue_on_pg(
                pool,
                dispatch_id,
                None,
            )
            .await
            .map_err(|error| {
                ServiceError::internal(format!("cancel postgres dispatch {dispatch_id}: {error}"))
                    .with_code(ErrorCode::Database)
                    .with_operation("cancel_all_dispatches.cancel_pg")
                    .with_context("dispatch_id", dispatch_id)
            })?;
        }

        tracing::info!(
            "[queue-api] Cancelled {count} dispatches (card={:?}, agent={:?})",
            kanban_card_id,
            agent_id
        );
        Ok(json!({"ok": true, "cancelled": count}))
    }

    pub async fn cancel_turn(
        &self,
        health_registry: Option<&Arc<HealthRegistry>>,
        channel_id: &str,
    ) -> ServiceResult<Value> {
        let session_info = if let Some(pool) = self.pg_pool.as_ref() {
            sqlx::query_as::<_, (String, Option<String>, Option<String>)>(
                "SELECT session_key, active_dispatch_id, provider
                 FROM sessions
                 WHERE status = 'working'
                   AND (
                     session_key LIKE '%' || $1 || '%'
                     OR agent_id IN (
                       SELECT id FROM agents
                       WHERE discord_channel_id = $1
                          OR discord_channel_alt = $1
                          OR discord_channel_cc = $1
                          OR discord_channel_cdx = $1
                     )
                   )
                 ORDER BY last_heartbeat DESC
                 LIMIT 1",
            )
            .bind(channel_id)
            .fetch_optional(pool)
            .await
            .map_err(|error| {
                ServiceError::internal(format!("load postgres active turn: {error}"))
                    .with_code(ErrorCode::Database)
                    .with_operation("cancel_turn.query_active_session_pg")
                    .with_context("channel_id", channel_id)
            })?
        } else {
            let conn = self.db.lock().map_err(|error| {
                ServiceError::internal(format!("{error}"))
                    .with_code(ErrorCode::Database)
                    .with_operation("cancel_turn.lock")
                    .with_context("channel_id", channel_id)
            })?;
            conn.query_row(
                "SELECT session_key, active_dispatch_id, provider FROM sessions \
                 WHERE status = 'working' \
                 AND (session_key LIKE '%' || ?1 || '%' OR agent_id IN \
                      (SELECT id FROM agents WHERE
                          discord_channel_id = ?1 OR discord_channel_alt = ?1 OR
                          discord_channel_cc = ?1 OR discord_channel_cdx = ?1)) \
                 ORDER BY last_heartbeat DESC LIMIT 1",
                [channel_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(|error| {
                ServiceError::internal(format!("load active turn: {error}"))
                    .with_code(ErrorCode::Database)
                    .with_operation("cancel_turn.query_active_session")
                    .with_context("channel_id", channel_id)
            })?
        };

        let Some((session_key, dispatch_id, provider_name)) = session_info else {
            return Err(
                ServiceError::not_found("no active turn found for this channel")
                    .with_code(ErrorCode::Queue)
                    .with_context("channel_id", channel_id),
            );
        };

        let tmux_name = session_key.split(':').last().unwrap_or(&session_key);
        let lifecycle = stop_turn_preserving_queue(
            health_registry.map(Arc::as_ref),
            &TurnLifecycleTarget {
                provider: provider_name.as_deref().and_then(ProviderKind::from_str),
                channel_id: channel_id.parse::<u64>().ok().map(ChannelId::new),
                tmux_name: tmux_name.to_string(),
            },
            "queue-api cancel_turn",
        )
        .await;

        if let Some(dispatch_id) = dispatch_id.as_ref() {
            if let Some(pool) = self.pg_pool.as_ref() {
                if let Err(error) = crate::dispatch::cancel_dispatch_and_reset_auto_queue_on_pg(
                    pool,
                    dispatch_id,
                    None,
                )
                .await
                {
                    tracing::warn!(
                        dispatch_id,
                        "failed to cancel postgres dispatch while cancelling turn: {error}"
                    );
                }
            } else if let Ok(conn) = self.db.lock() {
                if let Err(error) = crate::dispatch::cancel_dispatch_and_reset_auto_queue_on_conn(
                    &conn,
                    dispatch_id,
                    None,
                ) {
                    tracing::warn!(
                        dispatch_id,
                        "failed to cancel sqlite dispatch while cancelling turn: {error}"
                    );
                }
            }
        }

        if let Some(pool) = self.pg_pool.as_ref() {
            if let Err(error) = sqlx::query(
                "UPDATE sessions
                 SET status = 'disconnected',
                     active_dispatch_id = NULL,
                     claude_session_id = NULL
                 WHERE session_key = $1",
            )
            .bind(&session_key)
            .execute(pool)
            .await
            {
                tracing::warn!(
                    session_key,
                    "failed to mark postgres session disconnected during cancel_turn: {error}"
                );
            }
        } else if let Ok(conn) = self.db.lock() {
            if let Err(error) = conn.execute(
                "UPDATE sessions
                 SET status = 'disconnected', active_dispatch_id = NULL, claude_session_id = NULL
                 WHERE session_key = ?1",
                [&session_key],
            ) {
                tracing::warn!(
                    session_key,
                    "failed to mark sqlite session disconnected during cancel_turn: {error}"
                );
            }
        }

        tracing::info!(
            "[queue-api] Cancelled turn: session={}, tmux={}, killed={}, dispatch={:?}, lifecycle={}",
            session_key,
            tmux_name,
            lifecycle.tmux_killed,
            dispatch_id,
            lifecycle.lifecycle_path,
        );

        Ok(json!({
            "ok": true,
            "session_key": session_key,
            "tmux_session": tmux_name,
            "tmux_killed": lifecycle.tmux_killed,
            "lifecycle_path": lifecycle.lifecycle_path,
            "queued_remaining": lifecycle.queue_depth,
            "queue_preserved": lifecycle.queue_preserved,
            "inflight_cleared": lifecycle.inflight_cleared,
            "dispatch_cancelled": dispatch_id,
        }))
    }
}
