use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::{delete, post},
};
use poise::serenity_prelude::ChannelId;
use serde::Deserialize;
use serde_json::json;

use super::{ApiRouter, AppState, protected_api_domain, state};

#[derive(Debug, Deserialize)]
pub(crate) struct UpsertMonitoringBody {
    key: String,
    description: String,
}

pub(crate) fn router(state: AppState) -> ApiRouter {
    protected_api_domain(
        Router::new()
            .route(
                "/channels/{channel_id}/monitoring",
                post(upsert_monitoring).get(list_monitoring),
            )
            .route(
                "/channels/{channel_id}/monitoring/{key}",
                delete(remove_monitoring),
            ),
        state,
    )
}

pub async fn upsert_monitoring(
    State(state): State<AppState>,
    Path(channel_id): Path<u64>,
    Json(body): Json<UpsertMonitoringBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let key = body.key.trim();
    let description = body.description.trim();
    if key.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"status": "error", "error": "key is required"})),
        );
    }
    if description.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"status": "error", "error": "description is required"})),
        );
    }

    let active_count = {
        let monitoring = state::global_monitoring_store();
        let mut store = monitoring.lock().await;
        store.upsert(channel_id, key.to_string(), description.to_string())
    };
    schedule_render(&state, channel_id);

    (
        StatusCode::OK,
        Json(json!({"status": "ok", "active_count": active_count})),
    )
}

pub async fn remove_monitoring(
    State(state): State<AppState>,
    Path((channel_id, key)): Path<(u64, String)>,
) -> (StatusCode, Json<serde_json::Value>) {
    let active_count = {
        let monitoring = state::global_monitoring_store();
        let mut store = monitoring.lock().await;
        store.remove(channel_id, &key)
    };
    schedule_render(&state, channel_id);

    (
        StatusCode::OK,
        Json(json!({"status": "ok", "active_count": active_count})),
    )
}

pub async fn list_monitoring(
    State(_state): State<AppState>,
    Path(channel_id): Path<u64>,
) -> (StatusCode, Json<serde_json::Value>) {
    let entries = {
        let monitoring = state::global_monitoring_store();
        let store = monitoring.lock().await;
        store.list(channel_id)
    };

    (
        StatusCode::OK,
        Json(json!({
            "status": "ok",
            "active_count": entries.len(),
            "entries": entries,
        })),
    )
}

fn schedule_render(state: &AppState, channel_id: u64) {
    crate::services::discord::monitoring_status::schedule_render_channel(
        state::global_monitoring_store(),
        state.health_registry.clone(),
        ChannelId::new(channel_id),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_body_deserializes_payload_shape() -> Result<(), serde_json::Error> {
        let body: UpsertMonitoringBody =
            serde_json::from_str(r#"{"key":"m1","description":"waiting"}"#)?;

        assert_eq!(body.key, "m1");
        assert_eq!(body.description, "waiting");
        Ok(())
    }
}
