use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use serde::Deserialize;
use serde_json::json;

use super::AppState;
use crate::services::pipeline_override::{PipelineOverrideError, PipelineOverrideService};
pub use crate::services::pipeline_routes::PipelineStageInput as PutStageItem;
use crate::services::pipeline_routes::{PipelineRouteError, PipelineRouteService};
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub use crate::services::pipeline_routes::{
    STAGE_BACKOFF_VALUES, STAGE_ON_FAILURE_VALUES, validate_backoff, validate_on_failure,
};

fn pg_unavailable() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({"error": "postgres pool not configured"})),
    )
}

fn pipeline_override_error_response(
    error: PipelineOverrideError,
) -> (StatusCode, Json<serde_json::Value>) {
    match error {
        PipelineOverrideError::BadRequest(error) => {
            (StatusCode::BAD_REQUEST, Json(json!({"error": error})))
        }
        PipelineOverrideError::NotFound(error) => {
            (StatusCode::NOT_FOUND, Json(json!({"error": error})))
        }
        PipelineOverrideError::Database(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": error})),
        ),
    }
}

fn pipeline_route_error_response(
    error: PipelineRouteError,
) -> (StatusCode, Json<serde_json::Value>) {
    match error {
        PipelineRouteError::BadRequest { stage, error } => (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": format!("stage '{stage}': {error}"),
                "stage": stage,
            })),
        ),
        PipelineRouteError::NotFound(error) => {
            (StatusCode::NOT_FOUND, Json(json!({"error": error})))
        }
        PipelineRouteError::Readonly { table, source } => (
            StatusCode::METHOD_NOT_ALLOWED,
            Json(json!({
                "error": format!(
                    "table '{}' is file-canonical; edit policies/default-pipeline.yaml \
                     and restart the server to apply changes",
                    table
                ),
                "table": table,
                "source_of_truth": source,
            })),
        ),
        PipelineRouteError::Database(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": error})),
        ),
    }
}

// ── Query / Body types ─────────────────────────────────────────

// ── Dashboard v2 types (/pipeline/...) ────────────────────────

#[derive(Debug, Deserialize)]
pub struct GetStagesQuery {
    pub repo: Option<String>,
    pub agent_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct DeleteStagesQuery {
    pub repo: String,
}

#[derive(Debug, Deserialize)]
pub struct PutStagesBody {
    pub repo: String,
    pub stages: Vec<PutStageItem>,
}

#[derive(Debug, Deserialize)]
pub struct TranscriptQuery {
    pub limit: Option<usize>,
}

// ── Dashboard v2 handlers ─────────────────────────────────────

/// GET /api/pipeline/stages?repo=...&agent_id=...
pub async fn get_stages(
    State(state): State<AppState>,
    Query(params): Query<GetStagesQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return pg_unavailable();
    };
    let service = PipelineRouteService::new(pool);
    let stages = match service
        .list_stages(params.repo.as_deref(), params.agent_id.as_deref())
        .await
    {
        Ok(stages) => stages,
        Err(error) => return pipeline_route_error_response(error),
    };

    (StatusCode::OK, Json(json!({ "stages": stages })))
}

/// PUT /api/pipeline/stages — bulk replace stages for a repo
pub async fn put_stages(
    State(state): State<AppState>,
    Json(body): Json<PutStagesBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return pg_unavailable();
    };
    let service = PipelineRouteService::new(pool);
    let stages = match service.replace_stages(&body.repo, &body.stages).await {
        Ok(stages) => stages,
        Err(error) => return pipeline_route_error_response(error),
    };

    (StatusCode::OK, Json(json!({ "stages": stages })))
}

/// DELETE /api/pipeline/stages?repo=...
pub async fn delete_stages(
    State(state): State<AppState>,
    Query(params): Query<DeleteStagesQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return pg_unavailable();
    };
    let service = PipelineRouteService::new(pool);
    match service.delete_stages(&params.repo).await {
        Ok(count) => (
            StatusCode::OK,
            Json(json!({"deleted": true, "count": count})),
        ),
        Err(error) => pipeline_route_error_response(error),
    }
}

/// GET /api/pipeline/cards/{card_id} — card pipeline state with history
pub async fn get_card_pipeline(
    State(state): State<AppState>,
    Path(card_id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return pg_unavailable();
    };

    let service = PipelineRouteService::new(pool);
    let card_pipeline = match service.card_pipeline(&card_id).await {
        Ok(card_pipeline) => card_pipeline,
        Err(error) => return pipeline_route_error_response(error),
    };

    (
        StatusCode::OK,
        Json(json!({
            "repo_id": card_pipeline.repo_id,
            "stages": card_pipeline.stages,
            "history": card_pipeline.history,
            "current_stage": card_pipeline.current_stage,
        })),
    )
}

/// GET /api/pipeline/cards/{card_id}/history
pub async fn get_card_history(
    State(state): State<AppState>,
    Path(card_id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return pg_unavailable();
    };
    let service = PipelineRouteService::new(pool);
    let history = match service.card_history(&card_id).await {
        Ok(history) => history,
        Err(error) => return pipeline_route_error_response(error),
    };

    (StatusCode::OK, Json(json!({"history": history})))
}

/// GET /api/pipeline/cards/{card_id}/transcripts?limit=10
pub async fn get_card_transcripts(
    State(state): State<AppState>,
    Path(card_id): Path<String>,
    Query(params): Query<TranscriptQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return pg_unavailable();
    };

    let service = PipelineRouteService::new(pool);
    match service
        .card_transcripts(&card_id, params.limit.unwrap_or(10))
        .await
    {
        Ok(transcripts) => (
            StatusCode::OK,
            Json(json!({
                "card_id": card_id,
                "transcripts": transcripts,
            })),
        ),
        Err(error) => pipeline_route_error_response(error),
    }
}

// ── Pipeline Config Hierarchy (#135) ─────────────────────────

/// Query params for effective pipeline resolution.
#[derive(Debug, Deserialize)]
pub struct EffectivePipelineQuery {
    pub repo: Option<String>,
    pub agent_id: Option<String>,
}

/// GET /api/pipeline/config/default — the base pipeline YAML as JSON
pub async fn get_default_pipeline() -> (StatusCode, Json<serde_json::Value>) {
    match crate::pipeline::try_get() {
        Some(p) => (StatusCode::OK, Json(p.to_json())),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "default pipeline not loaded"})),
        ),
    }
}

/// GET /api/pipeline/config/effective?repo=...&agent_id=...
/// Returns the merged effective pipeline for a repo/agent combination.
pub async fn get_effective_pipeline(
    State(state): State<AppState>,
    Query(params): Query<EffectivePipelineQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return pg_unavailable();
    };
    let service = PipelineRouteService::new(pool);
    match service
        .effective_pipeline(params.repo.as_deref(), params.agent_id.as_deref())
        .await
    {
        Ok(effective) => (StatusCode::OK, Json(effective)),
        Err(error) => pipeline_route_error_response(error),
    }
}

/// Body for setting pipeline override
#[derive(Debug, Deserialize)]
pub struct SetPipelineOverrideBody {
    pub config: Option<serde_json::Value>,
}

/// GET /api/pipeline/config/repo/:owner/:repo
pub async fn get_repo_pipeline(
    State(state): State<AppState>,
    Path((owner, repo)): Path<(String, String)>,
) -> (StatusCode, Json<serde_json::Value>) {
    let id = format!("{owner}/{repo}");
    let Some(pool) = state.pg_pool_ref() else {
        return pg_unavailable();
    };
    let service = PipelineOverrideService::new(pool);
    let parsed = match service.get_repo_pipeline(&id).await {
        Ok(parsed) => parsed,
        Err(error) => return pipeline_override_error_response(error),
    };

    (
        StatusCode::OK,
        Json(json!({"repo": id, "pipeline_config": parsed})),
    )
}

/// PUT /api/pipeline/config/repo/:owner/:repo
pub async fn set_repo_pipeline(
    State(state): State<AppState>,
    Path((owner, repo)): Path<(String, String)>,
    Json(body): Json<SetPipelineOverrideBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let id = format!("{owner}/{repo}");
    let Some(pool) = state.pg_pool_ref() else {
        return pg_unavailable();
    };
    let service = PipelineOverrideService::new(pool);
    if let Err(error) = service.set_repo_pipeline(&id, body.config.as_ref()).await {
        return pipeline_override_error_response(error);
    }
    (StatusCode::OK, Json(json!({"ok": true, "repo": id})))
}

/// GET /api/pipeline/config/agent/:agent_id
pub async fn get_agent_pipeline(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return pg_unavailable();
    };
    let service = PipelineOverrideService::new(pool);
    let parsed = match service.get_agent_pipeline(&agent_id).await {
        Ok(parsed) => parsed,
        Err(error) => return pipeline_override_error_response(error),
    };

    (
        StatusCode::OK,
        Json(json!({"agent_id": agent_id, "pipeline_config": parsed})),
    )
}

/// PUT /api/pipeline/config/agent/:agent_id
pub async fn set_agent_pipeline(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Json(body): Json<SetPipelineOverrideBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return pg_unavailable();
    };
    let service = PipelineOverrideService::new(pool);
    if let Err(error) = service
        .set_agent_pipeline(&agent_id, body.config.as_ref())
        .await
    {
        return pipeline_override_error_response(error);
    }
    (
        StatusCode::OK,
        Json(json!({"ok": true, "agent_id": agent_id})),
    )
}

/// GET /api/pipeline/config/graph?repo=...&agent_id=...
/// Returns the effective pipeline as a visual graph (nodes + edges).
pub async fn get_pipeline_graph(
    State(state): State<AppState>,
    Query(params): Query<EffectivePipelineQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return pg_unavailable();
    };
    let service = PipelineRouteService::new(pool);
    match service
        .pipeline_graph(params.repo.as_deref(), params.agent_id.as_deref())
        .await
    {
        Ok(graph) => (StatusCode::OK, Json(graph)),
        Err(error) => pipeline_route_error_response(error),
    }
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::db::session_transcripts::{SessionTranscriptEvent, SessionTranscriptEventKind};
    use crate::engine::PolicyEngine;

    fn test_db() -> Db {
        let conn = sqlite_test::Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        crate::db::schema::migrate(&conn).unwrap();
        crate::db::wrap_conn(conn)
    }

    fn test_engine_with_pg(pg_pool: sqlx::PgPool) -> PolicyEngine {
        let mut config = crate::config::Config::default();
        config.policies.dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("policies");
        config.policies.hot_reload = false;
        PolicyEngine::new_with_pg(&config, Some(pg_pool)).unwrap()
    }

    /// Per-test Postgres database lifecycle for the #1238 migration of
    /// pipeline handler tests, which now require a PG pool.
    struct PipelinePgDatabase {
        _lifecycle: crate::db::postgres::PostgresTestLifecycleGuard,
        admin_url: String,
        database_name: String,
        database_url: String,
    }

    impl PipelinePgDatabase {
        async fn create() -> Self {
            let lifecycle = crate::db::postgres::lock_test_lifecycle();
            let admin_url = pg_test_admin_database_url();
            let database_name = format!("agentdesk_pipeline_{}", uuid::Uuid::new_v4().simple());
            let database_url = format!("{}/{}", pg_test_base_database_url(), database_name);
            crate::db::postgres::create_test_database(
                &admin_url,
                &database_name,
                "pipeline handler pg",
            )
            .await
            .expect("create pipeline postgres test db");

            Self {
                _lifecycle: lifecycle,
                admin_url,
                database_name,
                database_url,
            }
        }

        async fn migrate(&self) -> sqlx::PgPool {
            crate::db::postgres::connect_test_pool_and_migrate(
                &self.database_url,
                "pipeline handler pg",
            )
            .await
            .expect("connect + migrate pipeline postgres test db")
        }

        async fn drop(self) {
            crate::db::postgres::drop_test_database(
                &self.admin_url,
                &self.database_name,
                "pipeline handler pg",
            )
            .await
            .expect("drop pipeline postgres test db");
        }
    }

    fn pg_test_base_database_url() -> String {
        if let Ok(base) = std::env::var("POSTGRES_TEST_DATABASE_URL_BASE") {
            let trimmed = base.trim();
            if !trimmed.is_empty() {
                return trimmed.trim_end_matches('/').to_string();
            }
        }
        let user = std::env::var("PGUSER")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .or_else(|| std::env::var("USER").ok().filter(|v| !v.trim().is_empty()))
            .unwrap_or_else(|| "postgres".to_string());
        let password = std::env::var("PGPASSWORD")
            .ok()
            .filter(|v| !v.trim().is_empty());
        let host = std::env::var("PGHOST")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| "localhost".to_string());
        let port = std::env::var("PGPORT")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| "5432".to_string());
        match password {
            Some(password) => format!("postgresql://{user}:{password}@{host}:{port}"),
            None => format!("postgresql://{user}@{host}:{port}"),
        }
    }

    fn pg_test_admin_database_url() -> String {
        let admin_db = std::env::var("POSTGRES_TEST_ADMIN_DB")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| "postgres".to_string());
        format!("{}/{}", pg_test_base_database_url(), admin_db)
    }

    #[tokio::test]
    async fn get_card_transcripts_pg_returns_linked_turns() {
        let pg_db = PipelinePgDatabase::create().await;
        let pool = pg_db.migrate().await;
        let db = test_db();
        let state = AppState::test_state_with_pg(
            db.clone(),
            test_engine_with_pg(pool.clone()),
            pool.clone(),
        );

        sqlx::query(
            "INSERT INTO agents (id, name, provider, status, xp) VALUES ($1, $2, $3, $4, $5)",
        )
        .bind("agent-card-transcript")
        .bind("Card Transcript Agent")
        .bind("codex")
        .bind("idle")
        .bind(0_i32)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO kanban_cards (id, title, status, github_issue_number, created_at, updated_at)
             VALUES ($1, $2, $3, $4, NOW(), NOW())",
        )
        .bind("card-transcript-1")
        .bind("Transcript Card")
        .bind("in_progress")
        .bind(525_i32)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at
             ) VALUES ($1, $2, $3, $4, $5, $6, NOW(), NOW())",
        )
        .bind("dispatch-card-transcript-1")
        .bind("card-transcript-1")
        .bind("agent-card-transcript")
        .bind("implementation")
        .bind("completed")
        .bind("Transcript Dispatch")
        .execute(&pool)
        .await
        .unwrap();

        let events = vec![SessionTranscriptEvent {
            kind: SessionTranscriptEventKind::Result,
            tool_name: None,
            summary: Some("done".to_string()),
            content: "viewer wired".to_string(),
            status: Some("success".to_string()),
            is_error: false,
        }];
        let events_json = serde_json::to_string(&events).unwrap();
        sqlx::query(
            "INSERT INTO session_transcripts (
                turn_id, session_key, channel_id, agent_id, provider, dispatch_id,
                user_message, assistant_message, events_json, duration_ms
             ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, CAST($9 AS jsonb), $10)",
        )
        .bind("discord:card-transcript:1")
        .bind("host:card-transcript")
        .bind("chan-card")
        .bind("agent-card-transcript")
        .bind("codex")
        .bind("dispatch-card-transcript-1")
        .bind("wire transcript viewer")
        .bind("wired")
        .bind(events_json)
        .bind(6100_i32)
        .execute(&pool)
        .await
        .unwrap();

        let (status, Json(body)) = get_card_transcripts(
            State(state),
            Path("card-transcript-1".to_string()),
            Query(TranscriptQuery { limit: Some(5) }),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body["card_id"],
            serde_json::Value::String("card-transcript-1".to_string())
        );
        assert_eq!(
            body["transcripts"][0]["turn_id"],
            "discord:card-transcript:1"
        );
        assert_eq!(
            body["transcripts"][0]["dispatch_title"],
            "Transcript Dispatch"
        );
        assert_eq!(body["transcripts"][0]["card_title"], "Transcript Card");
        assert_eq!(body["transcripts"][0]["github_issue_number"], 525);
        assert_eq!(body["transcripts"][0]["events"][0]["kind"], "result");
        assert_eq!(body["transcripts"][0]["duration_ms"], 6100);

        pool.close().await;
        pg_db.drop().await;
    }

    /// #1097 (910-3): pipeline_stages is file-canonical, so PUT/DELETE
    /// must be rejected with HTTP 405.
    #[tokio::test]
    async fn put_stages_pg_rejected_when_db_source_of_truth_is_file() {
        let pg_db = PipelinePgDatabase::create().await;
        let pool = pg_db.migrate().await;
        let db = test_db();
        let state = AppState::test_state_with_pg(
            db.clone(),
            test_engine_with_pg(pool.clone()),
            pool.clone(),
        );

        // pipeline_stages is seeded as file-canonical by migration 0019.
        let body = PutStagesBody {
            repo: "owner/repo".to_string(),
            stages: vec![PutStageItem {
                stage_name: "build".to_string(),
                stage_order: Some(1),
                trigger_after: None,
                entry_skill: None,
                provider: None,
                agent_override_id: None,
                timeout_minutes: None,
                on_failure: None,
                on_failure_target: None,
                max_retries: None,
                backoff: None,
                skip_condition: None,
                parallel_with: None,
            }],
        };

        let (status, Json(json)) = put_stages(State(state.clone()), Json(body)).await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(json["table"], "pipeline_stages");
        assert_eq!(json["source_of_truth"], "file-canonical");
        assert!(
            json["error"]
                .as_str()
                .unwrap_or("")
                .contains("file-canonical")
        );

        let (status2, Json(json2)) = delete_stages(
            State(state),
            Query(DeleteStagesQuery {
                repo: "owner/repo".to_string(),
            }),
        )
        .await;
        assert_eq!(status2, StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(json2["table"], "pipeline_stages");

        pool.close().await;
        pg_db.drop().await;
    }

    // ── #1082 validator tests ──────────────────────────────────

    #[test]
    fn validate_on_failure_accepts_enum_and_rejects_others() {
        assert!(validate_on_failure(None).is_ok());
        assert!(validate_on_failure(Some("")).is_ok());
        for v in STAGE_ON_FAILURE_VALUES {
            assert!(validate_on_failure(Some(v)).is_ok(), "expected {} ok", v);
        }
        let err = validate_on_failure(Some("bogus")).unwrap_err();
        assert!(err.contains("bogus"), "{err}");
    }

    #[test]
    fn validate_backoff_accepts_enum_and_rejects_others() {
        assert!(validate_backoff(None).is_ok());
        for v in STAGE_BACKOFF_VALUES {
            assert!(validate_backoff(Some(v)).is_ok(), "expected {} ok", v);
        }
        assert!(validate_backoff(Some("cubic")).is_err());
    }
}
