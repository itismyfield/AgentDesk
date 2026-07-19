//! #1066 — `/api/memory/{recall,remember,forget}` dual-mode API.
//!
//! Two backends:
//! * `Memento` — reuses the existing `MementoBackend` to call the local
//!   memento MCP server. Requires a runtime-configured memento endpoint +
//!   access key env var, and a memento MCP entry in the agent MCP config.
//! * `Local` — PostgreSQL-backed `local_memory` table with LIKE-based recall.
//!   Used as the default fallback when memento is not available or when
//!   `ADK_FORCE_LOCAL_MEMORY=1` is set.
//!
//! Auto-selection is performed per request by [`detect_memory_backend`].
//!
//! NOTE (#1066/#3743): memento `recall` and `forget` bridge invocations are
//! not implemented on this HTTP route. When memento is the detected backend,
//! those operations return an explicit capability error instead of falling back
//! to local `local_memory`, so callers cannot mistake fallback rows for MCP
//! backed memory.

use axum::{Json, extract::State, http::StatusCode};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::{QueryBuilder, Row};

use super::AppState;
use crate::error::{AppError, AppResult, ErrorCode};
use crate::services::memory::MementoRememberRequest;

// ── Backend detection ────────────────────────────────────────────

/// Resolved backend selection for a single `/api/memory/*` request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MemoryBackend {
    Memento,
    Local,
}

impl MemoryBackend {
    fn as_str(self) -> &'static str {
        match self {
            MemoryBackend::Memento => "memento",
            MemoryBackend::Local => "local",
        }
    }
}

/// Decide which backend to use for a `/api/memory/*` request.
///
/// Priority:
/// 1. `ADK_FORCE_LOCAL_MEMORY=1` → always Local (testing / escape hatch).
/// 2. Memento runtime config is active → Memento.
/// 3. Otherwise → Local.
pub(crate) fn detect_memory_backend() -> MemoryBackend {
    if env_flag_true("ADK_FORCE_LOCAL_MEMORY") {
        return MemoryBackend::Local;
    }

    if crate::services::memory::backend_is_active(
        crate::services::discord::settings::MemoryBackendKind::Memento,
    ) {
        MemoryBackend::Memento
    } else {
        MemoryBackend::Local
    }
}

fn env_flag_true(name: &str) -> bool {
    std::env::var(name)
        .ok()
        .map(|value| matches!(value.trim(), "1" | "true" | "TRUE" | "yes" | "on"))
        .unwrap_or(false)
}

// ── Request / response bodies ────────────────────────────────────

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct RecallBody {
    pub keywords: Option<Vec<String>>,
    pub text: Option<String>,
    pub workspace: Option<String>,
    pub limit: Option<u32>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct RememberBody {
    pub content: String,
    pub topic: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub importance: Option<f64>,
    pub workspace: Option<String>,
    pub global: Option<bool>,
    pub channel_id: Option<u64>,
    pub channel_name: Option<String>,
    pub keywords: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct ForgetBody {
    pub id: String,
}

// ── Handlers ─────────────────────────────────────────────────────

/// POST /api/memory/recall
pub async fn memory_recall(
    State(state): State<AppState>,
    Json(body): Json<RecallBody>,
) -> AppResult<(StatusCode, Json<Value>)> {
    let backend = detect_memory_backend();

    if backend == MemoryBackend::Memento {
        return Ok(memento_memory_operation_unsupported("recall"));
    }

    let fragments = local_recall_pg(&state, &body).await.map_err(|error| {
        AppError::internal(format!("local recall failed: {error}")).with_code(ErrorCode::Database)
    })?;

    let response = json!({
        "fragments": fragments,
        "source": MemoryBackend::Local.as_str(),
        "detected_backend": backend.as_str(),
    });
    Ok((StatusCode::OK, Json(response)))
}

/// POST /api/memory/remember
pub async fn memory_remember(
    State(state): State<AppState>,
    Json(body): Json<RememberBody>,
) -> AppResult<(StatusCode, Json<Value>)> {
    if body.content.trim().is_empty() {
        return Err(AppError::bad_request("content is required"));
    }
    if body.topic.trim().is_empty() {
        return Err(AppError::bad_request("topic is required"));
    }
    if body.kind.trim().is_empty() {
        return Err(AppError::bad_request("type is required"));
    }

    let backend = detect_memory_backend();

    if backend == MemoryBackend::Memento {
        if let Some(error) = validate_memento_remember_scope(&body) {
            return Err(AppError::bad_request(error));
        }
        match memento_remember(&body).await {
            Ok(()) => {
                // Memento does not surface a public fragment ID via its
                // current `remember` wrapper, so we return an opaque
                // backend-qualified token to satisfy the API contract.
                let id = format!("memento:{}", body.topic.trim());
                return Ok((
                    StatusCode::OK,
                    Json(json!({
                        "id": id,
                        "source": MemoryBackend::Memento.as_str(),
                    })),
                ));
            }
            Err(error) => {
                tracing::warn!(
                    target: "memory_api",
                    "memento remember failed, falling back to local: {error}"
                );
            }
        }
    }

    let id = local_remember_pg(&state, &body).await.map_err(|error| {
        AppError::internal(format!("local remember failed: {error}")).with_code(ErrorCode::Database)
    })?;
    Ok((
        StatusCode::OK,
        Json(json!({
            "id": id,
            "source": MemoryBackend::Local.as_str(),
            "detected_backend": backend.as_str(),
        })),
    ))
}

fn validate_memento_remember_scope(body: &RememberBody) -> Option<String> {
    let global = body.global.unwrap_or(false);
    let workspace = body
        .workspace
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if global && workspace.is_some() {
        return Some("global=true cannot be combined with workspace".to_string());
    }
    if matches!(body.channel_id, Some(0)) {
        return Some("channel_id must be a non-zero Discord snowflake".to_string());
    }
    let has_workspace_override = std::env::var("MEMENTO_WORKSPACE")
        .ok()
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false);
    if !global && workspace.is_none() && body.channel_id.is_none() && !has_workspace_override {
        return Some(
            "Memento remember requires workspace, channel_id, global=true, or MEMENTO_WORKSPACE"
                .to_string(),
        );
    }
    None
}

/// POST /api/memory/forget
pub async fn memory_forget(
    State(state): State<AppState>,
    Json(body): Json<ForgetBody>,
) -> AppResult<(StatusCode, Json<Value>)> {
    let id = body.id.trim().to_string();
    if id.is_empty() {
        return Err(AppError::bad_request("id is required"));
    }

    let backend = detect_memory_backend();

    if backend == MemoryBackend::Memento {
        return Ok(memento_memory_operation_unsupported("forget"));
    }

    match local_forget_pg(&state, &id).await {
        Ok(true) => Ok((
            StatusCode::OK,
            Json(json!({
                "ok": true,
                "source": MemoryBackend::Local.as_str(),
                "detected_backend": backend.as_str(),
            })),
        )),
        Ok(false) => Ok((
            StatusCode::NOT_FOUND,
            Json(json!({
                "ok": false,
                "error": "id not found",
                "detected_backend": backend.as_str(),
            })),
        )),
        Err(error) => Err(AppError::internal(format!("local forget failed: {error}"))
            .with_code(ErrorCode::Database)),
    }
}

fn memento_memory_operation_unsupported(operation: &'static str) -> (StatusCode, Json<Value>) {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(json!({
            "ok": false,
            "error": format!("memento {operation} bridge is not implemented on /api/memory/{operation}"),
            "code": format!("memento_{operation}_unsupported"),
            "operation": operation,
            "source": MemoryBackend::Memento.as_str(),
            "detected_backend": MemoryBackend::Memento.as_str(),
            "local_fallback_available": true,
            "local_fallback_hint": "set ADK_FORCE_LOCAL_MEMORY=1 to query/delete only PostgreSQL local_memory fallback rows",
        })),
    )
}

// ── Local backend (PostgreSQL) ───────────────────────────────────

async fn local_recall_pg(state: &AppState, body: &RecallBody) -> Result<Vec<Value>, String> {
    let Some(pool) = state.pg_pool_ref() else {
        return Err("postgres pool unavailable".to_string());
    };
    let limit = body.limit.map(|value| value.clamp(1, 200)).unwrap_or(20) as i64;
    let mut query = QueryBuilder::new(
        "SELECT id, content, topic, kind, importance, workspace, keywords::TEXT AS keywords, created_at::TEXT AS created_at
           FROM local_memory WHERE 1=1",
    );

    if let Some(ws) = body
        .workspace
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        query.push(" AND workspace = ").push_bind(ws);
    }

    let mut keyword_terms: Vec<String> = Vec::new();
    if let Some(keywords) = body.keywords.as_ref() {
        for kw in keywords {
            let trimmed = kw.trim();
            if !trimmed.is_empty() {
                keyword_terms.push(trimmed.to_string());
            }
        }
    }
    if let Some(text) = body
        .text
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        keyword_terms.push(text.to_string());
    }

    for term in &keyword_terms {
        let like = format!("%{term}%");
        query
            .push(" AND (content ILIKE ")
            .push_bind(like.clone())
            .push(" OR topic ILIKE ")
            .push_bind(like.clone())
            .push(" OR COALESCE(keywords::TEXT, '') ILIKE ")
            .push_bind(like)
            .push(")");
    }

    query
        .push(" ORDER BY created_at DESC LIMIT ")
        .push_bind(limit);

    let rows = query
        .build()
        .fetch_all(pool)
        .await
        .map_err(|error| format!("query recall: {error}"))?;

    Ok(rows
        .into_iter()
        .map(|row| {
            let keywords = row.try_get::<Option<String>, _>("keywords").ok().flatten();
            let keywords_value = keywords
                .as_deref()
                .filter(|s| !s.is_empty())
                .and_then(|s| serde_json::from_str::<Value>(s).ok())
                .unwrap_or_else(|| json!([]));
            json!({
                "id": row.try_get::<String, _>("id").unwrap_or_default(),
                "content": row.try_get::<String, _>("content").unwrap_or_default(),
                "topic": row.try_get::<String, _>("topic").unwrap_or_default(),
                "type": row.try_get::<String, _>("kind").unwrap_or_default(),
                "importance": row.try_get::<Option<f64>, _>("importance").ok().flatten(),
                "workspace": row.try_get::<Option<String>, _>("workspace").ok().flatten(),
                "keywords": keywords_value,
                "created_at": row.try_get::<Option<String>, _>("created_at").ok().flatten(),
            })
        })
        .collect())
}

async fn local_remember_pg(state: &AppState, body: &RememberBody) -> Result<String, String> {
    let Some(pool) = state.pg_pool_ref() else {
        return Err("postgres pool unavailable".to_string());
    };
    let id = format!("mem-{}", uuid_like());

    let keywords_json = body
        .keywords
        .as_ref()
        .map(|kws| {
            let cleaned: Vec<String> = kws
                .iter()
                .map(|kw| kw.trim().to_string())
                .filter(|kw| !kw.is_empty())
                .collect();
            serde_json::to_string(&cleaned).unwrap_or_else(|_| "[]".to_string())
        })
        .filter(|s| s != "[]");

    sqlx::query(
        "INSERT INTO local_memory (id, content, topic, kind, importance, workspace, keywords)
         VALUES ($1, $2, $3, $4, $5, $6, $7::JSONB)",
    )
    .bind(&id)
    .bind(body.content.trim())
    .bind(body.topic.trim())
    .bind(body.kind.trim())
    .bind(body.importance)
    .bind(body.workspace.as_ref().map(|v| v.trim().to_string()))
    .bind(keywords_json)
    .execute(pool)
    .await
    .map_err(|error| format!("insert local_memory: {error}"))?;

    Ok(id)
}

async fn local_forget_pg(state: &AppState, id: &str) -> Result<bool, String> {
    let Some(pool) = state.pg_pool_ref() else {
        return Err("postgres pool unavailable".to_string());
    };
    sqlx::query("DELETE FROM local_memory WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await
        .map(|result| result.rows_affected() > 0)
        .map_err(|error| format!("delete local_memory: {error}"))
}

fn uuid_like() -> String {
    uuid::Uuid::new_v4().to_string()
}

// ── Memento bridge (remember only; recall/forget TBD) ────────────

async fn memento_remember(body: &RememberBody) -> Result<(), String> {
    let base = crate::services::discord::settings::memory_settings_for_binding(None);
    // Force memento regardless of the resolved binding default, since the
    // caller explicitly selected memento via backend detection.
    let settings = crate::services::discord::settings::ResolvedMemorySettings {
        backend: crate::services::discord::settings::MemoryBackendKind::Memento,
        ..base
    };
    // The trait object does not expose `remember` directly — only the
    // concrete `MementoBackend` does. Build it explicitly for this call.
    let memento = crate::services::memory::MementoBackend::new(settings);

    let request = MementoRememberRequest {
        content: body.content.trim().to_string(),
        topic: body.topic.trim().to_string(),
        kind: body.kind.trim().to_string(),
        importance: body.importance,
        keywords: body.keywords.clone().unwrap_or_default(),
        workspace: body
            .workspace
            .as_ref()
            .map(|w| w.trim().to_string())
            .filter(|w| !w.is_empty()),
        global: body.global.unwrap_or(false),
        channel_id: body.channel_id,
        channel_name: body
            .channel_name
            .as_ref()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty()),
        ..MementoRememberRequest::default()
    };

    memento.remember(request).await.map(|_| ())
}

// ── Tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod request_body_tests {
    use super::*;

    const PG_TEST_LABEL: &str = "memory API local fallback migration test";
    const TEST_MEMENTO_ACCESS_KEY_ENV: &str = "ADK_TEST_MEMENTO_ACCESS_KEY_3743";

    struct TestEnvGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }

    impl TestEnvGuard {
        fn new(names: &[&'static str]) -> Self {
            let lock = crate::config::shared_test_env_lock()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let saved = names
                .iter()
                .map(|name| (*name, std::env::var_os(name)))
                .collect();
            Self { _lock: lock, saved }
        }

        fn set<V>(&self, name: &'static str, value: V)
        where
            V: AsRef<std::ffi::OsStr>,
        {
            unsafe { std::env::set_var(name, value) };
        }

        fn remove(&self, name: &'static str) {
            unsafe { std::env::remove_var(name) };
        }
    }

    impl Drop for TestEnvGuard {
        fn drop(&mut self) {
            for (name, value) in &self.saved {
                match value {
                    Some(value) => unsafe { std::env::set_var(name, value) },
                    None => unsafe { std::env::remove_var(name) },
                }
            }
        }
    }

    fn memory_env_guard() -> TestEnvGuard {
        TestEnvGuard::new(&[
            "AGENTDESK_ROOT_DIR",
            "ADK_FORCE_LOCAL_MEMORY",
            TEST_MEMENTO_ACCESS_KEY_ENV,
        ])
    }

    fn write_memento_runtime_config(root: &std::path::Path) {
        let config_dir = crate::runtime_layout::config_dir(root);
        std::fs::create_dir_all(&config_dir).expect("create runtime config dir");
        let config = json!({
            "version": 2,
            "backend": "memento",
            "mcp": {
                "endpoint": "http://127.0.0.1:57332/mcp",
                "access_key_env": TEST_MEMENTO_ACCESS_KEY_ENV,
            },
        });
        std::fs::write(
            crate::runtime_layout::memory_backend_path(root),
            serde_json::to_vec_pretty(&config).expect("serialize memory backend config"),
        )
        .expect("write memory backend config");
    }

    struct MemoryApiPostgresDb {
        _lock: crate::db::postgres::PostgresTestLifecycleGuard,
        admin_url: String,
        database_name: String,
        database_url: String,
    }

    impl MemoryApiPostgresDb {
        async fn try_create() -> Option<Self> {
            let lock = crate::db::postgres::lock_test_lifecycle();
            let admin_url = crate::dispatch::test_support::postgres_admin_database_url();
            let database_name = format!("agentdesk_memory_api_{}", uuid::Uuid::new_v4().simple());
            let database_url = format!(
                "{}/{}",
                crate::dispatch::test_support::postgres_base_database_url(),
                database_name
            );
            if let Err(error) =
                crate::db::postgres::create_test_database(&admin_url, &database_name, PG_TEST_LABEL)
                    .await
            {
                eprintln!("skipping {PG_TEST_LABEL}: {error}");
                drop(lock);
                return None;
            }

            Some(Self {
                _lock: lock,
                admin_url,
                database_name,
                database_url,
            })
        }

        async fn connect(&self) -> sqlx::PgPool {
            crate::db::postgres::connect_test_pool(&self.database_url, PG_TEST_LABEL)
                .await
                .expect("connect memory API postgres test db")
        }

        async fn connect_and_migrate(&self) -> sqlx::PgPool {
            crate::db::postgres::connect_test_pool_and_migrate(&self.database_url, PG_TEST_LABEL)
                .await
                .expect("connect + migrate memory API postgres test db")
        }

        async fn drop(self) {
            crate::db::postgres::drop_test_database(
                &self.admin_url,
                &self.database_name,
                PG_TEST_LABEL,
            )
            .await
            .expect("drop memory API postgres test db");
        }
    }

    fn test_engine_with_pg(pg_pool: sqlx::PgPool) -> crate::engine::PolicyEngine {
        let mut config = crate::config::Config::default();
        config.policies.dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("policies");
        config.policies.hot_reload = false;
        crate::engine::PolicyEngine::new_with_pg(&config, Some(pg_pool)).unwrap()
    }

    fn test_state_with_pg(pg_pool: sqlx::PgPool) -> AppState {
        let tx = crate::server::ws::new_broadcast();
        let buf = crate::server::ws::spawn_batch_flusher(tx.clone());
        AppState {
            pg_pool: Some(pg_pool.clone()),
            engine: test_engine_with_pg(pg_pool),
            config: std::sync::Arc::new(crate::config::Config::default()),
            broadcast_tx: tx,
            batch_buffer: buf,
            health_registry: None,
            cluster_instance_id: None,
        }
    }

    #[test]
    fn remember_body_deserializes_channel_scope_fields() {
        let body: RememberBody = serde_json::from_str(
            r#"{
                "content": "fact",
                "topic": "scope",
                "type": "fact",
                "global": true,
                "channel_id": 1479671301387059200,
                "channel_name": "adk-cdx"
            }"#,
        )
        .unwrap();

        assert_eq!(body.global, Some(true));
        assert_eq!(body.channel_id, Some(1_479_671_301_387_059_200));
        assert_eq!(body.channel_name.as_deref(), Some("adk-cdx"));
    }

    #[test]
    fn memento_remember_scope_validation_rejects_conflicts() {
        let conflict = RememberBody {
            content: "fact".to_string(),
            topic: "scope".to_string(),
            kind: "fact".to_string(),
            workspace: Some("ops".to_string()),
            global: Some(true),
            ..RememberBody::default()
        };
        assert!(validate_memento_remember_scope(&conflict).is_some());

        let zero_channel = RememberBody {
            content: "fact".to_string(),
            topic: "scope".to_string(),
            kind: "fact".to_string(),
            channel_id: Some(0),
            ..RememberBody::default()
        };
        assert!(validate_memento_remember_scope(&zero_channel).is_some());
    }

    #[test]
    fn detect_memory_backend_uses_local_when_memento_runtime_unavailable() {
        let env = memory_env_guard();
        let root = tempfile::tempdir().expect("runtime root");
        write_memento_runtime_config(root.path());

        env.set("AGENTDESK_ROOT_DIR", root.path().as_os_str());
        env.remove("ADK_FORCE_LOCAL_MEMORY");
        env.remove(TEST_MEMENTO_ACCESS_KEY_ENV);

        assert_eq!(detect_memory_backend(), MemoryBackend::Local);
    }

    #[test]
    fn detect_memory_backend_uses_memento_when_runtime_config_is_active() {
        let env = memory_env_guard();
        let root = tempfile::tempdir().expect("runtime root");
        write_memento_runtime_config(root.path());

        env.set("AGENTDESK_ROOT_DIR", root.path().as_os_str());
        env.remove("ADK_FORCE_LOCAL_MEMORY");
        env.set(TEST_MEMENTO_ACCESS_KEY_ENV, "test-token");

        assert_eq!(detect_memory_backend(), MemoryBackend::Memento);
    }

    #[test]
    fn detect_memory_backend_force_local_overrides_active_memento() {
        let env = memory_env_guard();
        let root = tempfile::tempdir().expect("runtime root");
        write_memento_runtime_config(root.path());

        env.set("AGENTDESK_ROOT_DIR", root.path().as_os_str());
        env.set("ADK_FORCE_LOCAL_MEMORY", "1");
        env.set(TEST_MEMENTO_ACCESS_KEY_ENV, "test-token");

        assert_eq!(detect_memory_backend(), MemoryBackend::Local);
    }

    #[test]
    fn memento_recall_forget_unsupported_response_is_explicit() {
        let (recall_status, Json(recall)) = memento_memory_operation_unsupported("recall");
        assert_eq!(recall_status, StatusCode::NOT_IMPLEMENTED);
        assert_eq!(recall["ok"], false);
        assert_eq!(recall["source"], "memento");
        assert_eq!(recall["detected_backend"], "memento");
        assert_eq!(recall["code"], "memento_recall_unsupported");
        assert!(
            recall["error"]
                .as_str()
                .unwrap_or_default()
                .contains("memento recall bridge is not implemented")
        );
        assert!(
            recall["local_fallback_hint"]
                .as_str()
                .unwrap_or_default()
                .contains("ADK_FORCE_LOCAL_MEMORY=1")
        );

        let (forget_status, Json(forget)) = memento_memory_operation_unsupported("forget");
        assert_eq!(forget_status, StatusCode::NOT_IMPLEMENTED);
        assert_eq!(forget["ok"], false);
        assert_eq!(forget["source"], "memento");
        assert_eq!(forget["detected_backend"], "memento");
        assert_eq!(forget["code"], "memento_forget_unsupported");
        assert!(
            forget["error"]
                .as_str()
                .unwrap_or_default()
                .contains("memento forget bridge is not implemented")
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn memory_recall_forget_handlers_use_local_when_memento_runtime_unavailable() {
        let env = memory_env_guard();
        let root = tempfile::tempdir().expect("runtime root");
        write_memento_runtime_config(root.path());

        env.set("AGENTDESK_ROOT_DIR", root.path().as_os_str());
        env.remove("ADK_FORCE_LOCAL_MEMORY");
        env.remove(TEST_MEMENTO_ACCESS_KEY_ENV);

        let Some(pg_db) = MemoryApiPostgresDb::try_create().await else {
            return;
        };
        let pool = pg_db.connect_and_migrate().await;
        let state = test_state_with_pg(pool.clone());

        let remember = RememberBody {
            content: "Memento is configured but unavailable, so recall uses local fallback"
                .to_string(),
            topic: "memory-contract".to_string(),
            kind: "fact".to_string(),
            importance: Some(0.8),
            workspace: Some("issue-3743".to_string()),
            keywords: Some(vec!["fallback".to_string(), "inactive-memento".to_string()]),
            ..RememberBody::default()
        };
        let id = local_remember_pg(&state, &remember)
            .await
            .expect("seed local memory row");

        let (recall_status, Json(recall)) = memory_recall(
            State(state.clone()),
            Json(RecallBody {
                keywords: Some(vec!["inactive-memento".to_string()]),
                workspace: Some("issue-3743".to_string()),
                limit: Some(5),
                ..RecallBody::default()
            }),
        )
        .await
        .expect("recall local memory through route");
        assert_eq!(recall_status, StatusCode::OK);
        assert_eq!(recall["source"], "local");
        assert_eq!(recall["detected_backend"], "local");
        assert_eq!(recall["fragments"][0]["id"], id);

        let (forget_status, Json(forget)) = memory_forget(State(state), Json(ForgetBody { id }))
            .await
            .expect("forget local memory through route");
        assert_eq!(forget_status, StatusCode::OK);
        assert_eq!(forget["ok"], true);
        assert_eq!(forget["source"], "local");
        assert_eq!(forget["detected_backend"], "local");

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn local_memory_api_uses_migrated_table() {
        let Some(pg_db) = MemoryApiPostgresDb::try_create().await else {
            return;
        };
        let pool = pg_db.connect_and_migrate().await;
        let state = test_state_with_pg(pool.clone());

        let table: Option<String> =
            sqlx::query_scalar("SELECT to_regclass('public.local_memory')::TEXT")
                .fetch_one(&pool)
                .await
                .expect("read migrated local_memory table");
        assert_eq!(table.as_deref(), Some("local_memory"));

        let remember = RememberBody {
            content: "The relay fallback writes through the local memory API".to_string(),
            topic: "memory-migration".to_string(),
            kind: "fact".to_string(),
            importance: Some(0.7),
            workspace: Some("issue-3723".to_string()),
            keywords: Some(vec!["relay".to_string(), "fallback".to_string()]),
            ..RememberBody::default()
        };
        let id = local_remember_pg(&state, &remember)
            .await
            .expect("remember local memory row");

        let recall = RecallBody {
            keywords: Some(vec!["fallback".to_string()]),
            workspace: Some("issue-3723".to_string()),
            limit: Some(5),
            ..RecallBody::default()
        };
        let fragments = local_recall_pg(&state, &recall)
            .await
            .expect("recall local memory row");
        assert_eq!(fragments.len(), 1);
        assert_eq!(fragments[0]["id"], id);
        assert_eq!(fragments[0]["topic"], "memory-migration");
        assert_eq!(fragments[0]["keywords"], json!(["relay", "fallback"]));

        assert!(
            local_forget_pg(&state, &id)
                .await
                .expect("forget local memory row")
        );
        assert!(
            !local_forget_pg(&state, &id)
                .await
                .expect("forget missing local memory row")
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn local_memory_migration_preserves_preexisting_route_table_rows() {
        let Some(pg_db) = MemoryApiPostgresDb::try_create().await else {
            return;
        };
        let pool = pg_db.connect().await;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS local_memory (
                id TEXT PRIMARY KEY,
                content TEXT NOT NULL,
                topic TEXT NOT NULL,
                kind TEXT NOT NULL,
                importance DOUBLE PRECISION,
                workspace TEXT,
                keywords JSONB,
                created_at TIMESTAMPTZ DEFAULT NOW()
            )",
        )
        .execute(&pool)
        .await
        .expect("create pre-migration local_memory table");
        sqlx::query(
            "INSERT INTO local_memory
                (id, content, topic, kind, importance, workspace, keywords)
             VALUES
                ('legacy-row', 'kept across migration', 'legacy', 'fact', 0.5, 'issue-3723', '[\"legacy\"]'::JSONB)",
        )
        .execute(&pool)
        .await
        .expect("insert pre-migration local_memory row");

        crate::db::postgres::migrate(&pool)
            .await
            .expect("migrate database with preexisting local_memory table");

        let content: String =
            sqlx::query_scalar("SELECT content FROM local_memory WHERE id = 'legacy-row'")
                .fetch_one(&pool)
                .await
                .expect("read preserved local_memory row");
        assert_eq!(content, "kept across migration");

        let keywords_index: Option<String> =
            sqlx::query_scalar("SELECT to_regclass('public.idx_local_memory_keywords_gin')::TEXT")
                .fetch_one(&pool)
                .await
                .expect("read migrated local_memory keywords index");
        assert_eq!(
            keywords_index.as_deref(),
            Some("idx_local_memory_keywords_gin")
        );

        pool.close().await;
        pg_db.drop().await;
    }
}
