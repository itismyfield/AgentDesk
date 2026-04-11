use axum::{Json, extract::State, http::StatusCode};
use serde::Deserialize;
use serde_json::json;

use super::AppState;

#[derive(Deserialize)]
pub struct RegisterRequest {
    pub source_agent: String,
    pub user_id: String,
    pub channel_id: Option<String>,
    pub context: Option<serde_json::Value>,
    pub ttl_seconds: Option<i64>,
}

/// POST /api/dm-reply/register
///
/// Register a pending DM reply entry so that the next DM from the given user
/// is captured and routed back to the source agent. This is the HTTP equivalent
/// of the JS bridge `agentdesk.dmReply.register()`.
pub async fn register_handler(
    State(state): State<AppState>,
    Json(body): Json<RegisterRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let source_agent = body.source_agent.trim().to_string();
    let user_id = body.user_id.trim().to_string();

    if source_agent.is_empty() || user_id.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "source_agent and user_id are required"})),
        );
    }

    let channel_id = body.channel_id.as_deref().map(|s| s.trim().to_string());
    let context_str = serde_json::to_string(&body.context.unwrap_or(json!({})))
        .unwrap_or_else(|_| "{}".to_string());
    let ttl_seconds = body.ttl_seconds.unwrap_or(3600);

    let db = state.db.clone();
    let result = tokio::task::spawn_blocking(move || {
        let conn = match db.separate_conn() {
            Ok(c) => c,
            Err(e) => return Err(format!("db connection: {e}")),
        };

        let expires_at = if ttl_seconds > 0 {
            format!("datetime('now', '+{ttl_seconds} seconds')")
        } else {
            "NULL".to_string()
        };

        let sql = format!(
            "INSERT INTO pending_dm_replies (source_agent, user_id, channel_id, context, expires_at) \
             VALUES (?1, ?2, ?3, ?4, {expires_at})"
        );
        match conn.execute(
            &sql,
            rusqlite::params![source_agent, user_id, channel_id, context_str],
        ) {
            Ok(_) => {
                let id = conn.last_insert_rowid();
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] [HTTP] dmReply.register -> user={user_id} agent={source_agent} (id={id})"
                );
                Ok(id)
            }
            Err(e) => Err(format!("insert failed: {e}")),
        }
    })
    .await;

    match result {
        Ok(Ok(id)) => (StatusCode::OK, Json(json!({"ok": true, "id": id}))),
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e}))),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("task join: {e}")})),
        ),
    }
}
