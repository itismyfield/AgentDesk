use std::sync::Arc;

use libsql_rusqlite::OptionalExtension;
use serde_json::{Value, json};

use crate::db::Db;
use crate::services::discord::health::HealthRegistry;
use crate::services::provider::ProviderKind;
use crate::services::service_error::{ErrorCode, ServiceError, ServiceResult};
use crate::services::turn_lifecycle::{TurnLifecycleTarget, stop_turn_preserving_queue};
use poise::serenity_prelude::ChannelId;

#[derive(Clone)]
pub struct QueueService {
    db: Db,
    health_registry: Option<Arc<HealthRegistry>>,
}

impl QueueService {
    pub fn new(db: Db, health_registry: Option<Arc<HealthRegistry>>) -> Self {
        Self {
            db,
            health_registry,
        }
    }

    pub fn cancel_dispatch(&self, dispatch_id: &str) -> ServiceResult<Value> {
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

    pub fn cancel_all_dispatches(
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

    pub async fn cancel_turn(
        &self,
        health_registry: Option<&Arc<HealthRegistry>>,
        channel_id: &str,
    ) -> ServiceResult<Value> {
        let session_info: Option<(String, Option<String>, Option<String>)> = {
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
            if let Ok(conn) = self.db.lock() {
                crate::dispatch::cancel_dispatch_and_reset_auto_queue_on_conn(
                    &conn,
                    dispatch_id,
                    None,
                )
                .ok();
            }
        }

        if let Ok(conn) = self.db.lock() {
            conn.execute(
                "UPDATE sessions
                 SET status = 'disconnected', active_dispatch_id = NULL, claude_session_id = NULL
                 WHERE session_key = ?1",
                [&session_key],
            )
            .ok();
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
