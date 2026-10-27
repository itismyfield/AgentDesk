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
async fn existing_agent_ids_reach_lookup_and_new_duplicate_ids_stay_validated() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    for id in ["../bad", "a/b", "a\\b", ".", ".."] {
        let response = agents_crud::get_agent(State(state.clone()), Path(id.to_string()))
            .await
            .into_response();
        assert_eq!(response.status(), StatusCode::OK, "get {id}");
        let body = serde_json::from_value(serde_json::json!({"prompt_content": "test"})).unwrap();
        let response =
            agents_crud::update_agent(State(state.clone()), Path(id.to_string()), Json(body))
                .await
                .into_response();
        assert_eq!(
            response.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "update {id}"
        );
        let body = serde_json::from_value(
            serde_json::json!({"new_agent_id": "valid", "channel_id": "123"}),
        )
        .unwrap();
        let response =
            agents_crud::duplicate_agent(State(state.clone()), Path(id.to_string()), Json(body))
                .await
                .into_response();
        assert_eq!(
            response.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "duplicate {id}"
        );
        let response = agents_crud::delete_agent(State(state.clone()), Path(id.to_string()))
            .await
            .into_response();
        assert_eq!(
            response.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "delete {id}"
        );
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

#[cfg(unix)]
struct ManagedPromptFixture {
    _parent: tempfile::TempDir,
    _env: crate::config::TestEnvVarGuard,
    db: crate::dispatch::test_support::DispatchPostgresTestDb,
    state: AppState,
    prompt: std::path::PathBuf,
    vault: std::path::PathBuf,
}

#[cfg(unix)]
impl ManagedPromptFixture {
    async fn new() -> Self {
        let parent = tempfile::tempdir().unwrap();
        let runtime = parent.path().join("runtime");
        let vault = parent.path().join("vault/x.prompt.md");
        let prompt = runtime.join("config/agents/x.prompt.md");
        std::fs::create_dir_all(vault.parent().unwrap()).unwrap();
        std::fs::create_dir_all(prompt.parent().unwrap()).unwrap();
        std::fs::write(&vault, "vault prompt 한글").unwrap();
        std::os::unix::fs::symlink(&vault, &prompt).unwrap();
        let env = crate::config::test_env::set_agentdesk_root_for_test(&runtime);
        let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
            "managed_prompt",
            "managed prompt API fixture",
        )
        .await;
        let pool = db.connect_and_migrate().await;
        let mut state = state(&runtime);
        let config = Arc::make_mut(&mut state.config);
        config.agents.push(
            serde_json::from_value(serde_json::json!({
                "id": "x", "name": "Source", "provider": "claude",
                "channels": {"claude": {"id": "101", "prompt_file": prompt.display().to_string()}}
            }))
            .unwrap(),
        );
        crate::config::save_to_path(&crate::runtime_layout::config_file_path(&runtime), config)
            .unwrap();
        sqlx::query("INSERT INTO agents (id, name, provider) VALUES ('x', 'Source', 'claude')")
            .execute(&pool)
            .await
            .unwrap();
        state.pg_pool = Some(pool);
        Self {
            _parent: parent,
            _env: env,
            db,
            state,
            prompt,
            vault,
        }
    }

    async fn finish(self) {
        self.state.pg_pool_ref().unwrap().close().await;
        self.db.drop().await;
    }
}

#[cfg(unix)]
#[tokio::test]
async fn setup_dry_run_accepts_managed_vault_prompt_symlink_pg() {
    let fixture = ManagedPromptFixture::new().await;
    for path in [
        fixture.prompt.display().to_string(),
        "agents/x.prompt.md".into(),
        "config/agents/x.prompt.md".into(),
    ] {
        let body = agents_setup::AgentSetupBody {
            agent_id: "new-agent".into(),
            channel_id: "202".into(),
            provider: "claude".into(),
            prompt_template_path: path,
            skills: vec![],
            dry_run: true,
        };
        let (status, Json(result)) =
            agents_setup::setup_agent(State(fixture.state.clone()), Json(body)).await;
        assert_eq!(status, StatusCode::OK, "{result}");
        assert_eq!(result["ok"], true, "{result}");
        assert_eq!(result["dry_run"], true);
    }
    assert_eq!(
        std::fs::read(&fixture.prompt).unwrap(),
        std::fs::read(&fixture.vault).unwrap()
    );
    assert!(
        !fixture
            .prompt
            .parent()
            .unwrap()
            .join("new-agent/IDENTITY.md")
            .exists()
    );
    fixture.finish().await;
}

#[cfg(unix)]
#[tokio::test]
async fn duplicate_copies_managed_vault_prompt_symlink_pg() {
    let fixture = ManagedPromptFixture::new().await;
    let body = serde_json::from_value(serde_json::json!({
        "new_agent_id": "copy-agent", "channel_id": "303"
    }))
    .unwrap();
    let response =
        agents_crud::duplicate_agent(State(fixture.state.clone()), Path("x".into()), Json(body))
            .await;
    let (status, Json(result)) = response.unwrap();
    assert_eq!(status, StatusCode::CREATED, "{result}");
    assert_eq!(result["ok"], true);
    let copied = fixture
        .prompt
        .parent()
        .unwrap()
        .join("copy-agent/IDENTITY.md");
    assert_eq!(
        std::fs::read(copied).unwrap(),
        std::fs::read(&fixture.vault).unwrap()
    );
    fixture.finish().await;
}

#[tokio::test]
async fn legacy_agent_rows_support_get_update_delete_without_prompt_paths_pg() {
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::test_env::set_agentdesk_root_for_test(root.path());
    let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
        "legacy_agent",
        "legacy agent CRUD",
    )
    .await;
    let pool = db.connect_and_migrate().await;
    let mut state = state(root.path());
    state.pg_pool = Some(pool.clone());
    for id in ["legacy.agent", "요리사", "../legacy"] {
        sqlx::query("INSERT INTO agents (id, name, provider) VALUES ($1, 'Legacy', 'claude')")
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
        let (status, Json(agent)) = agents_crud::get_agent(State(state.clone()), Path(id.into()))
            .await
            .unwrap();
        assert_eq!(status, StatusCode::OK);
        assert_eq!(agent["agent"]["id"], id);
        assert!(agent["agent"]["prompt_path"].is_null());
        assert!(agent["agent"]["prompt_content"].is_null());
        let body = serde_json::from_value(serde_json::json!({"name": "Updated"})).unwrap();
        let (status, Json(agent)) =
            agents_crud::update_agent(State(state.clone()), Path(id.into()), Json(body))
                .await
                .unwrap();
        assert_eq!(status, StatusCode::OK);
        assert_eq!(agent["agent"]["name"], "Updated");
        let (status, _) = agents_crud::delete_agent(State(state.clone()), Path(id.into()))
            .await
            .unwrap();
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM agents WHERE id = $1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap(),
            0
        );
    }
    pool.close().await;
    db.drop().await;
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
