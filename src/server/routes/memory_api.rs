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
//! NOTE (#1066 follow-up): memento `recall` and `forget` bridge invocations
//! are TBD — the existing `MementoBackend` only exposes `remember()`. For now
//! those two ops always take the local branch even when the backend reports
//! `memento`. The `source` field in the response surfaces which branch ran so
//! callers can distinguish.

use axum::{Json, extract::State, http::StatusCode};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::{PgPool, QueryBuilder, Row};

use super::AppState;
use crate::services::memory::MementoRememberRequest;
use crate::services::provider::ProviderKind;

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
/// 2. Memento MCP is configured for any active provider (Claude or Codex) → Memento.
/// 3. Otherwise → Local.
pub(crate) fn detect_memory_backend() -> MemoryBackend {
    if env_flag_true("ADK_FORCE_LOCAL_MEMORY") {
        return MemoryBackend::Local;
    }

    let memento_mcp_configured =
        crate::services::mcp_config::provider_has_memento_mcp(&ProviderKind::Claude)
            || crate::services::mcp_config::provider_has_memento_mcp(&ProviderKind::Codex);

    if memento_mcp_configured {
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
) -> (StatusCode, Json<Value>) {
    let backend = detect_memory_backend();

    // Memento recall bridge is TBD (see module docs). Fall back to local for
    // the moment; the response still reports the detected backend and a
    // deferred-bridge note so callers can tell.
    let (fragments, effective_source, note) = match local_recall_pg(&state, &body).await {
        Ok(fragments) => {
            let note = if backend == MemoryBackend::Memento {
                Some("memento recall bridge TBD; served from local_memory")
            } else {
                None
            };
            (fragments, MemoryBackend::Local, note)
        }
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("local recall failed: {error}")})),
            );
        }
    };

    let mut response = json!({
        "fragments": fragments,
        "source": effective_source.as_str(),
        "detected_backend": backend.as_str(),
    });
    if let Some(note) = note {
        if let Some(obj) = response.as_object_mut() {
            obj.insert("note".to_string(), json!(note));
        }
    }
    (StatusCode::OK, Json(response))
}

/// POST /api/memory/remember
pub async fn memory_remember(
    State(state): State<AppState>,
    Json(body): Json<RememberBody>,
) -> (StatusCode, Json<Value>) {
    if body.content.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "content is required"})),
        );
    }
    if body.topic.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "topic is required"})),
        );
    }
    if body.kind.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "type is required"})),
        );
    }

    let backend = detect_memory_backend();

    if backend == MemoryBackend::Memento {
        match memento_remember(&body).await {
            Ok(()) => {
                // Memento does not surface a public fragment ID via its
                // current `remember` wrapper, so we return an opaque
                // backend-qualified token to satisfy the API contract.
                let id = format!("memento:{}", body.topic.trim());
                return (
                    StatusCode::OK,
                    Json(json!({
                        "id": id,
                        "source": MemoryBackend::Memento.as_str(),
                    })),
                );
            }
            Err(error) => {
                tracing::warn!(
                    target: "memory_api",
                    "memento remember failed, falling back to local: {error}"
                );
            }
        }
    }

    match local_remember_pg(&state, &body).await {
        Ok(id) => (
            StatusCode::OK,
            Json(json!({
                "id": id,
                "source": MemoryBackend::Local.as_str(),
                "detected_backend": backend.as_str(),
            })),
        ),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("local remember failed: {error}")})),
        ),
    }
}

/// POST /api/memory/forget
pub async fn memory_forget(
    State(state): State<AppState>,
    Json(body): Json<ForgetBody>,
) -> (StatusCode, Json<Value>) {
    let id = body.id.trim().to_string();
    if id.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "id is required"})),
        );
    }

    let backend = detect_memory_backend();

    // Memento forget bridge is TBD (see module docs). Still attempt local
    // deletion so id prefixes the caller may have received ("memento:...")
    // do not silently succeed against a local row. Return 404 if nothing
    // matched locally.
    match local_forget_pg(&state, &id).await {
        Ok(true) => (
            StatusCode::OK,
            Json(json!({
                "ok": true,
                "source": MemoryBackend::Local.as_str(),
                "detected_backend": backend.as_str(),
            })),
        ),
        Ok(false) => {
            let note = if backend == MemoryBackend::Memento {
                "memento forget bridge TBD; id not found in local_memory"
            } else {
                "id not found"
            };
            (
                StatusCode::NOT_FOUND,
                Json(json!({
                    "ok": false,
                    "error": note,
                    "detected_backend": backend.as_str(),
                })),
            )
        }
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("local forget failed: {error}")})),
        ),
    }
}

// ── Local backend (PostgreSQL) ───────────────────────────────────

async fn ensure_local_memory_table(pool: &PgPool) -> Result<(), String> {
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
    .execute(pool)
    .await
    .map(|_| ())
    .map_err(|error| format!("ensure local_memory: {error}"))
}

async fn local_recall_pg(state: &AppState, body: &RecallBody) -> Result<Vec<Value>, String> {
    let Some(pool) = state.pg_pool_ref() else {
        return Err("postgres pool unavailable".to_string());
    };
    ensure_local_memory_table(pool).await?;
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
    ensure_local_memory_table(pool).await?;
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
    ensure_local_memory_table(pool).await?;
    sqlx::query("DELETE FROM local_memory WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await
        .map(|result| result.rows_affected() > 0)
        .map_err(|error| format!("delete local_memory: {error}"))
}

fn uuid_like() -> String {
    // Unique enough for this endpoint without pulling uuid into this module:
    // nanos + thread id fingerprint. Collisions within the same millisecond
    // would require the same thread firing two inserts back-to-back, which
    // the PRIMARY KEY constraint will surface loudly if it ever happens.
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tid = format!("{:?}", std::thread::current().id());
    format!("{nanos:x}-{}", simple_hash(&tid))
}

fn simple_hash(value: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    format!("{:x}", hasher.finish())
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
        ..MementoRememberRequest::default()
    };

    memento.remember(request).await.map(|_| ())
}

// ── Tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_db;
    use crate::engine::PolicyEngine;
    use crate::server::routes::AppState;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    /// All tests in this module mutate the process-wide `ADK_FORCE_LOCAL_MEMORY`
    /// env variable, so they must be serialized behind a single mutex. Parallel
    /// test execution otherwise leaves races between `set_var` and
    /// `remove_var` that flake `detect_memory_backend()`.
    fn env_lock() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    struct ForceLocalGuard<'a> {
        _inner: MutexGuard<'a, ()>,
    }

    impl<'a> ForceLocalGuard<'a> {
        fn new() -> Self {
            let guard = env_lock();
            unsafe { std::env::set_var("ADK_FORCE_LOCAL_MEMORY", "1") };
            Self { _inner: guard }
        }
    }

    impl<'a> Drop for ForceLocalGuard<'a> {
        fn drop(&mut self) {
            unsafe { std::env::remove_var("ADK_FORCE_LOCAL_MEMORY") };
        }
    }

    fn test_engine(db: &crate::db::Db) -> PolicyEngine {
        let mut config = crate::config::Config::default();
        config.policies.hot_reload = false;
        PolicyEngine::new_with_legacy_db(&config, db.clone()).unwrap()
    }

    fn test_state() -> AppState {
        let db = test_db();
        let engine = test_engine(&db);
        AppState::test_state(db.clone(), engine)
    }

    #[tokio::test]
    async fn detect_memory_backend_respects_force_local_env() {
        let _guard = ForceLocalGuard::new();
        assert_eq!(detect_memory_backend(), MemoryBackend::Local);
    }

    #[tokio::test]
    async fn local_memory_roundtrip_remember_recall_forget() {
        let _guard = ForceLocalGuard::new();
        let state = test_state();

        // remember
        let remember = memory_remember(
            axum::extract::State(state.clone()),
            axum::Json(RememberBody {
                content: "PostgreSQL cutover completed on 2026-04-24".to_string(),
                topic: "pg-cutover".to_string(),
                kind: "decision".to_string(),
                importance: Some(0.8),
                workspace: Some("ops".to_string()),
                keywords: Some(vec!["postgres".to_string(), "cutover".to_string()]),
            }),
        )
        .await;
        assert_eq!(remember.0, StatusCode::OK);
        let id = remember.1.0["id"].as_str().unwrap().to_string();
        assert!(id.starts_with("mem-"));
        assert_eq!(remember.1.0["source"], "local");

        // recall (by keyword)
        let recall = memory_recall(
            axum::extract::State(state.clone()),
            axum::Json(RecallBody {
                keywords: Some(vec!["postgres".to_string()]),
                text: None,
                workspace: Some("ops".to_string()),
                limit: Some(5),
            }),
        )
        .await;
        assert_eq!(recall.0, StatusCode::OK);
        let fragments = recall.1.0["fragments"].as_array().unwrap();
        assert_eq!(fragments.len(), 1);
        assert_eq!(fragments[0]["id"], id);
        assert_eq!(fragments[0]["topic"], "pg-cutover");
        assert_eq!(recall.1.0["source"], "local");

        // recall by text (fallback to text matcher, no workspace filter)
        let recall_text = memory_recall(
            axum::extract::State(state.clone()),
            axum::Json(RecallBody {
                keywords: None,
                text: Some("cutover".to_string()),
                workspace: None,
                limit: None,
            }),
        )
        .await;
        assert_eq!(recall_text.0, StatusCode::OK);
        let frags = recall_text.1.0["fragments"].as_array().unwrap();
        assert!(frags.iter().any(|f| f["id"] == id));

        // forget
        let forget = memory_forget(
            axum::extract::State(state.clone()),
            axum::Json(ForgetBody { id: id.clone() }),
        )
        .await;
        assert_eq!(forget.0, StatusCode::OK);
        assert_eq!(forget.1.0["ok"], true);

        // recall after forget — should be empty
        let recall_after = memory_recall(
            axum::extract::State(state.clone()),
            axum::Json(RecallBody {
                keywords: Some(vec!["postgres".to_string()]),
                text: None,
                workspace: None,
                limit: None,
            }),
        )
        .await;
        assert_eq!(recall_after.0, StatusCode::OK);
        assert_eq!(recall_after.1.0["fragments"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn forget_returns_404_for_unknown_id() {
        let _guard = ForceLocalGuard::new();
        let state = test_state();
        let forget = memory_forget(
            axum::extract::State(state),
            axum::Json(ForgetBody {
                id: "does-not-exist".to_string(),
            }),
        )
        .await;
        assert_eq!(forget.0, StatusCode::NOT_FOUND);
        assert_eq!(forget.1.0["ok"], false);
    }

    #[tokio::test]
    async fn remember_validates_required_fields() {
        let _guard = ForceLocalGuard::new();
        let state = test_state();
        let resp = memory_remember(
            axum::extract::State(state),
            axum::Json(RememberBody {
                content: "".to_string(),
                topic: "t".to_string(),
                kind: "fact".to_string(),
                ..RememberBody::default()
            }),
        )
        .await;
        assert_eq!(resp.0, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn force_local_env_propagates_to_recall_response() {
        let _guard = ForceLocalGuard::new();
        let state = test_state();
        let recall = memory_recall(
            axum::extract::State(state),
            axum::Json(RecallBody::default()),
        )
        .await;
        assert_eq!(recall.0, StatusCode::OK);
        assert_eq!(recall.1.0["source"], "local");
        assert_eq!(recall.1.0["detected_backend"], "local");
    }
}
