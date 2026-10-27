use super::*;
use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
};

fn state(root: &std::path::Path) -> AppState {
    let mut config = crate::config::Config::default();
    config.policies.dir = root.join("policies");
    config.policies.hot_reload = false;
    config.data.dir = root.join("data");
    std::fs::create_dir_all(&config.policies.dir).unwrap();
    AppState {
        engine: PolicyEngine::new_with_pg(&config, None).unwrap(),
        config: Arc::new(config),
        pg_pool: None,
        broadcast_tx: crate::eventbus::new_broadcast(),
        batch_buffer: Default::default(),
        health_registry: None,
        cluster_instance_id: None,
    }
}

#[tokio::test]
async fn create_rejects_invalid_ids_before_database_access() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    for id in [
        "", ".", "..", "../bad", "a/b", "a\\b", "a.b", "a b", "한글", "a\0b",
    ] {
        let body = serde_json::from_value(serde_json::json!({"id": id, "name": "test"})).unwrap();
        let response = agents_crud::create_agent(State(state.clone()), Json(body))
            .await
            .into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{id:?}");
    }
}

#[tokio::test]
async fn path_agent_handlers_reject_unsafe_ids_before_lookup() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    for id in ["../bad", "a/b", "a\\b", ".", ".."] {
        let response = agents_crud::get_agent(State(state.clone()), Path(id.to_string()))
            .await
            .into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "get {id}");
        let body = serde_json::from_value(serde_json::json!({"prompt_content": "test"})).unwrap();
        let response =
            agents_crud::update_agent(State(state.clone()), Path(id.to_string()), Json(body))
                .await
                .into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "update {id}");
        let body = serde_json::from_value(
            serde_json::json!({"new_agent_id": "valid", "channel_id": "123"}),
        )
        .unwrap();
        let response =
            agents_crud::duplicate_agent(State(state.clone()), Path(id.to_string()), Json(body))
                .await
                .into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "duplicate {id}");
        let response = agents_crud::delete_agent(State(state.clone()), Path(id.to_string()))
            .await
            .into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "delete {id}");
        let body = serde_json::from_value(serde_json::json!({
            "new_agent_id": id, "channel_id": "123"
        }))
        .unwrap();
        let response = agents_crud::duplicate_agent(
            State(state.clone()),
            Path("valid-source".into()),
            Json(body),
        )
        .await
        .into_response();
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "duplicate target {id}"
        );
    }
}

#[tokio::test]
async fn create_preserves_legacy_id_formats_through_validation() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    for id in [
        "agent",
        "project-agentdesk",
        "agent_2",
        "GPT-6_1",
        "123",
        "-",
        "_",
    ] {
        let body = serde_json::from_value(serde_json::json!({"id": id, "name": "test"})).unwrap();
        let response = agents_crud::create_agent(State(state.clone()), Json(body))
            .await
            .into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE, "{id}");
    }
}
