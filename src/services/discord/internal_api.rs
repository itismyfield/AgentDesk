use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use serde_json::Value;

use super::health::HealthRegistry;
use crate::{
    db::Db,
    engine::PolicyEngine,
    server::{
        routes::{self, AppState},
        ws,
    },
};

#[derive(Clone)]
struct DirectApiContext {
    db: Db,
    pg_pool: Option<sqlx::PgPool>,
    engine: PolicyEngine,
    health_registry: Arc<HealthRegistry>,
}

static DIRECT_API_CONTEXT: OnceLock<RwLock<Option<DirectApiContext>>> = OnceLock::new();

fn context_slot() -> &'static RwLock<Option<DirectApiContext>> {
    DIRECT_API_CONTEXT.get_or_init(|| RwLock::new(None))
}

fn load_context() -> Result<DirectApiContext, String> {
    let guard = context_slot()
        .read()
        .map_err(|err| format!("direct runtime API context lock failed: {err}"))?;
    guard
        .as_ref()
        .cloned()
        .ok_or_else(|| "direct runtime API context is unavailable".to_string())
}

pub(super) fn init(
    db: Option<Db>,
    pg_pool: Option<sqlx::PgPool>,
    engine: Option<PolicyEngine>,
    health_registry: Arc<HealthRegistry>,
) {
    let (Some(db), Some(engine)) = (db, engine) else {
        return;
    };
    if let Ok(mut guard) = context_slot().write() {
        *guard = Some(DirectApiContext {
            db,
            pg_pool,
            engine,
            health_registry,
        });
    }
}

fn app_state() -> Result<AppState, String> {
    let ctx = load_context()?;
    Ok(AppState {
        db: ctx.db.clone(),
        pg_pool: ctx.pg_pool.clone(),
        engine: ctx.engine.clone(),
        config: Arc::new(crate::config::load_graceful()),
        broadcast_tx: ws::new_broadcast(),
        batch_buffer: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        health_registry: Some(ctx.health_registry.clone()),
    })
}

fn into_result(status: StatusCode, body: Value) -> Result<Value, String> {
    if status.is_success() {
        Ok(body)
    } else {
        Err(body
            .get("error")
            .and_then(|value| value.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| format!("{}: {}", status, body)))
    }
}

pub(super) async fn lookup_dispatch_info(dispatch_id: &str) -> Result<Value, String> {
    let mut params = HashMap::new();
    params.insert("dispatch_id".to_string(), dispatch_id.to_string());
    let (status, Json(body)) =
        routes::dispatches::get_card_thread(State(app_state()?), Query(params)).await;
    into_result(status, body)
}

pub(super) async fn link_dispatch_thread(
    body: routes::dispatches::LinkDispatchThreadBody,
) -> Result<Value, String> {
    let (status, Json(body)) =
        routes::dispatches::link_dispatch_thread(State(app_state()?), Json(body)).await;
    into_result(status, body)
}

pub(super) async fn lookup_pending_dispatch_for_thread(thread_id: u64) -> Result<Value, String> {
    let mut params = HashMap::new();
    params.insert("thread_id".to_string(), thread_id.to_string());
    let (status, Json(body)) =
        routes::dispatches::get_pending_dispatch_for_thread(State(app_state()?), Query(params))
            .await;
    into_result(status, body)
}

pub(super) async fn hook_session(
    body: routes::dispatched_sessions::HookSessionBody,
) -> Result<Value, String> {
    let (status, Json(body)) =
        routes::dispatched_sessions::hook_session(State(app_state()?), Json(body)).await;
    into_result(status, body)
}

pub(super) async fn delete_session(session_key: &str) -> Result<Value, String> {
    let (status, Json(body)) = routes::dispatched_sessions::delete_session(
        State(app_state()?),
        Query(routes::dispatched_sessions::DeleteSessionQuery {
            session_key: session_key.to_string(),
            provider: None,
        }),
    )
    .await;
    into_result(status, body)
}

pub(super) async fn clear_stale_session_id(session_id: &str) -> Result<Value, String> {
    let (status, Json(body)) = routes::dispatched_sessions::clear_stale_session_id(
        State(app_state()?),
        Json(serde_json::json!({ "session_id": session_id })),
    )
    .await;
    into_result(status, body)
}

pub(super) async fn clear_session_id(session_key: &str) -> Result<Value, String> {
    let (status, Json(body)) = routes::dispatched_sessions::clear_session_id_by_key(
        State(app_state()?),
        Json(serde_json::json!({ "session_key": session_key })),
    )
    .await;
    into_result(status, body)
}

pub(super) async fn get_provider_session_id(
    session_key: &str,
    provider: Option<&str>,
) -> Result<Value, String> {
    let (status, Json(body)) = routes::dispatched_sessions::get_claude_session_id(
        State(app_state()?),
        Query(routes::dispatched_sessions::DeleteSessionQuery {
            session_key: session_key.to_string(),
            provider: provider.map(str::to_string),
        }),
    )
    .await;
    into_result(status, body)
}

pub(super) async fn get_config_entries() -> Result<Value, String> {
    let (status, Json(body)) = routes::settings::get_config_entries(State(app_state()?)).await;
    into_result(status, body)
}

pub(super) async fn get_escalation_settings() -> Result<Value, String> {
    let (status, Json(body)) =
        routes::escalation::get_escalation_settings(State(app_state()?)).await;
    into_result(status, body)
}

pub(super) async fn put_escalation_settings(
    settings: routes::escalation::EscalationSettings,
) -> Result<Value, String> {
    let (status, Json(body)) =
        routes::escalation::put_escalation_settings(State(app_state()?), Json(settings)).await;
    into_result(status, body)
}

pub(super) async fn fetch_dispatch(dispatch_id: &str) -> Result<Value, String> {
    let (status, Json(body)) =
        routes::dispatches::get_dispatch(State(app_state()?), Path(dispatch_id.to_string())).await;
    into_result(status, body)
}

pub(super) async fn update_dispatch(
    dispatch_id: &str,
    body: routes::dispatches::UpdateDispatchBody,
) -> Result<Value, String> {
    let (status, Json(body)) = routes::dispatches::update_dispatch(
        State(app_state()?),
        Path(dispatch_id.to_string()),
        Json(body),
    )
    .await;
    into_result(status, body)
}

pub(super) async fn submit_review_decision(
    body: routes::review_verdict::ReviewDecisionBody,
) -> Result<Value, String> {
    let (status, Json(body)) =
        routes::review_verdict::submit_review_decision(State(app_state()?), Json(body)).await;
    into_result(status, body)
}

pub(super) async fn submit_review_verdict(
    body: routes::review_verdict::SubmitVerdictBody,
) -> Result<Value, String> {
    let (status, Json(body)) =
        routes::review_verdict::submit_verdict(State(app_state()?), Json(body)).await;
    into_result(status, body)
}

pub(super) async fn upsert_meeting(
    body: routes::meetings::UpsertMeetingBody,
) -> Result<Value, String> {
    let (status, Json(body)) =
        routes::meetings::upsert_meeting(State(app_state()?), Json(body)).await;
    into_result(status, body)
}

pub(super) fn set_kv_value(key: &str, value: &str) -> Result<(), String> {
    let ctx = load_context()?;
    let conn = ctx
        .db
        .lock()
        .map_err(|err| format!("db lock failed: {err}"))?;
    conn.execute(
        "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?1, ?2)",
        libsql_rusqlite::params![key, value],
    )
    .map_err(|err| err.to_string())?;
    Ok(())
}

pub(super) fn take_kv_value(key: &str) -> Result<Option<String>, String> {
    let ctx = load_context()?;
    let conn = ctx
        .db
        .lock()
        .map_err(|err| format!("db lock failed: {err}"))?;
    let value = match conn.query_row(
        "SELECT value FROM kv_meta WHERE key = ?1 AND (expires_at IS NULL OR expires_at > datetime('now'))",
        libsql_rusqlite::params![key],
        |row| row.get::<_, String>(0),
    ) {
        Ok(value) => Some(value),
        Err(libsql_rusqlite::Error::QueryReturnedNoRows) => None,
        Err(err) => return Err(err.to_string()),
    };
    if value.is_some() {
        conn.execute(
            "DELETE FROM kv_meta WHERE key = ?1",
            libsql_rusqlite::params![key],
        )
        .map_err(|err| err.to_string())?;
    }
    Ok(value)
}

pub(super) async fn gc_stale_thread_sessions() -> Result<usize, String> {
    let ctx = load_context()?;
    if let Some(pool) = ctx.pg_pool.as_ref() {
        return Ok(routes::dispatched_sessions::gc_stale_thread_sessions_pg(pool).await);
    }
    let conn = ctx
        .db
        .lock()
        .map_err(|err| format!("db lock failed: {err}"))?;
    Ok(routes::dispatched_sessions::gc_stale_thread_sessions_db(
        &conn,
    ))
}
