use axum::{Json, extract::State, http::StatusCode};
use serde::Deserialize;
use serde_json::json;

use super::AppState;
use crate::services::discord_dm_reply_store::register_pending_dm_reply_db;

/// TODO(#1238 / 843g): see agents::agent_quality_legacy_db.
fn dm_reply_legacy_db(state: &AppState) -> &crate::db::Db {
    use std::sync::OnceLock;
    static PLACEHOLDER: OnceLock<crate::db::Db> = OnceLock::new();
    state
        .engine
        .legacy_db()
        .or_else(|| state.legacy_db())
        .unwrap_or_else(|| PLACEHOLDER.get_or_init(super::pending_migration_shim_for_callers))
}

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

    // TODO(#1238 / 843g): register_pending_dm_reply_db short-circuits to PG
    // when a pool is configured (production runtime). The SQLite handle is
    // kept only for the test fallback; once #1238 ports the function to
    // PG-only this placeholder goes away.
    match register_pending_dm_reply_db(
        dm_reply_legacy_db(&state),
        state.pg_pool_ref(),
        &source_agent,
        &user_id,
        channel_id.as_deref(),
        &context_str,
        ttl_seconds,
    )
    .await
    {
        Ok(id) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] [HTTP] dmReply.register -> user={user_id} agent={source_agent} (id={id})"
            );
            (StatusCode::OK, Json(json!({"ok": true, "id": id})))
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e}))),
    }
}
