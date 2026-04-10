use serde_json::{Value, json};

use crate::db::Db;
use crate::services::service_error::{ServiceError, ServiceResult};

#[derive(Clone)]
pub struct QueueService {
    db: Db,
}

impl QueueService {
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    pub fn cancel_dispatch(&self, dispatch_id: &str) -> ServiceResult<Value> {
        let conn = self
            .db
            .lock()
            .map_err(|e| ServiceError::internal(format!("{e}")))?;

        let current_status: Option<String> = conn
            .query_row(
                "SELECT status FROM task_dispatches WHERE id = ?1",
                [dispatch_id],
                |row| row.get(0),
            )
            .ok();

        match current_status.as_deref() {
            None => Err(ServiceError::not_found("dispatch not found")),
            Some("completed") | Some("cancelled") | Some("failed") => {
                Err(ServiceError::conflict(format!(
                    "dispatch already in terminal state: {}",
                    current_status.unwrap_or_default()
                )))
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
        let conn = self
            .db
            .lock()
            .map_err(|e| ServiceError::internal(format!("{e}")))?;

        let mut conditions = vec!["status IN ('pending', 'dispatched')".to_string()];
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

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
        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            params.iter().map(|param| param.as_ref()).collect();
        let dispatch_ids: Vec<String> = conn
            .prepare(&sql)
            .ok()
            .and_then(|mut stmt| {
                stmt.query_map(param_refs.as_slice(), |row| row.get::<_, String>(0))
                    .ok()
                    .map(|rows| rows.filter_map(|row| row.ok()).collect())
            })
            .unwrap_or_default();

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

    pub fn cancel_turn(&self, channel_id: &str) -> ServiceResult<Value> {
        let session_info: Option<(String, Option<String>)> = self.db.lock().ok().and_then(|conn| {
            conn.query_row(
                "SELECT session_key, active_dispatch_id FROM sessions \
                 WHERE status = 'working' \
                 AND (session_key LIKE '%' || ?1 || '%' OR agent_id IN \
                      (SELECT id FROM agents WHERE
                          discord_channel_id = ?1 OR discord_channel_alt = ?1 OR
                          discord_channel_cc = ?1 OR discord_channel_cdx = ?1)) \
                 ORDER BY last_heartbeat DESC LIMIT 1",
                [channel_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
            )
            .ok()
        });

        let Some((session_key, dispatch_id)) = session_info else {
            return Err(ServiceError::not_found(
                "no active turn found for this channel",
            ));
        };

        let tmux_name = session_key.split(':').last().unwrap_or(&session_key);
        let killed = crate::services::platform::tmux::kill_session(tmux_name);

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
                "UPDATE sessions SET status = 'disconnected', active_dispatch_id = NULL WHERE session_key = ?1",
                [&session_key],
            )
            .ok();
        }

        tracing::info!(
            "[queue-api] Cancelled turn: session={}, tmux={}, killed={}, dispatch={:?}",
            session_key,
            tmux_name,
            killed,
            dispatch_id
        );

        Ok(json!({
            "ok": true,
            "session_key": session_key,
            "tmux_session": tmux_name,
            "tmux_killed": killed,
            "dispatch_cancelled": dispatch_id,
        }))
    }
}
