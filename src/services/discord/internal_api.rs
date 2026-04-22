use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};
use std::time::Duration;

use reqwest::Method;
use serde::Serialize;
use serde_json::Value;

use crate::server::routes;

#[derive(Clone)]
struct DirectApiContext {
    api_port: u16,
    pg_pool: Option<sqlx::PgPool>,
}

static DIRECT_API_CONTEXT: OnceLock<RwLock<Option<DirectApiContext>>> = OnceLock::new();
static DIRECT_API_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

fn context_slot() -> &'static RwLock<Option<DirectApiContext>> {
    DIRECT_API_CONTEXT.get_or_init(|| RwLock::new(None))
}

fn client() -> &'static reqwest::Client {
    DIRECT_API_CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("direct runtime API client")
    })
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

pub(super) fn init(api_port: u16, pg_pool: Option<sqlx::PgPool>) {
    if let Ok(mut guard) = context_slot().write() {
        *guard = Some(DirectApiContext { api_port, pg_pool });
    }
}

fn api_url(ctx: &DirectApiContext, path: &str) -> String {
    crate::config::local_api_url(ctx.api_port, path)
}

fn api_origin(ctx: &DirectApiContext) -> String {
    format!("http://{}:{}", crate::config::loopback(), ctx.api_port)
}

fn into_result(status: reqwest::StatusCode, body: Value) -> Result<Value, String> {
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

async fn read_response(response: reqwest::Response) -> Result<Value, String> {
    let status = response.status();
    let body = response.json::<Value>().await.unwrap_or_else(
        |error| serde_json::json!({ "error": format!("invalid direct API response: {error}") }),
    );
    into_result(status, body)
}

async fn request_json(method: Method, path: &str) -> Result<Value, String> {
    let ctx = load_context()?;
    let response = client()
        .request(method, api_url(&ctx, path))
        .header(reqwest::header::ORIGIN, api_origin(&ctx))
        .header(reqwest::header::REFERER, api_origin(&ctx))
        .send()
        .await
        .map_err(|error| format!("direct runtime API {path}: {error}"))?;
    read_response(response).await
}

async fn request_query<Q>(method: Method, path: &str, query: &Q) -> Result<Value, String>
where
    Q: Serialize + ?Sized,
{
    let ctx = load_context()?;
    let response = client()
        .request(method, api_url(&ctx, path))
        .query(query)
        .header(reqwest::header::ORIGIN, api_origin(&ctx))
        .header(reqwest::header::REFERER, api_origin(&ctx))
        .send()
        .await
        .map_err(|error| format!("direct runtime API {path}: {error}"))?;
    read_response(response).await
}

async fn request_body<B>(method: Method, path: &str, body: &B) -> Result<Value, String>
where
    B: Serialize + ?Sized,
{
    let ctx = load_context()?;
    let response = client()
        .request(method, api_url(&ctx, path))
        .json(body)
        .header(reqwest::header::ORIGIN, api_origin(&ctx))
        .header(reqwest::header::REFERER, api_origin(&ctx))
        .send()
        .await
        .map_err(|error| format!("direct runtime API {path}: {error}"))?;
    read_response(response).await
}

pub(super) async fn lookup_dispatch_info(dispatch_id: &str) -> Result<Value, String> {
    let mut params = HashMap::new();
    params.insert("dispatch_id".to_string(), dispatch_id.to_string());
    request_query(Method::GET, "/api/internal/card-thread", &params).await
}

pub(super) async fn lookup_dispatch_type(dispatch_id: &str) -> Result<Option<String>, String> {
    let body = lookup_dispatch_info(dispatch_id).await?;
    Ok(body
        .get("dispatch_type")
        .and_then(|value| value.as_str())
        .map(str::to_string))
}

pub(super) async fn link_dispatch_thread(
    body: routes::dispatches::LinkDispatchThreadBody,
) -> Result<Value, String> {
    request_body(Method::POST, "/api/internal/link-dispatch-thread", &body).await
}

pub(super) async fn lookup_pending_dispatch_for_thread(thread_id: u64) -> Result<Value, String> {
    let mut params = HashMap::new();
    params.insert("thread_id".to_string(), thread_id.to_string());
    request_query(
        Method::GET,
        "/api/internal/pending-dispatch-for-thread",
        &params,
    )
    .await
}

pub(super) async fn hook_session(
    body: routes::dispatched_sessions::HookSessionBody,
) -> Result<Value, String> {
    request_body(Method::POST, "/api/hook/session", &body).await
}

pub(super) async fn delete_session(session_key: &str) -> Result<Value, String> {
    request_query(
        Method::DELETE,
        "/api/hook/session",
        &routes::dispatched_sessions::DeleteSessionQuery {
            session_key: session_key.to_string(),
            provider: None,
        },
    )
    .await
}

pub(super) async fn clear_stale_session_id(session_id: &str) -> Result<Value, String> {
    request_body(
        Method::POST,
        "/api/dispatched-sessions/clear-stale-session-id",
        &serde_json::json!({ "session_id": session_id }),
    )
    .await
}

pub(super) async fn clear_session_id(session_key: &str) -> Result<Value, String> {
    request_body(
        Method::POST,
        "/api/dispatched-sessions/clear-session-id",
        &serde_json::json!({ "session_key": session_key }),
    )
    .await
}

pub(super) async fn get_provider_session_id(
    session_key: &str,
    provider: Option<&str>,
) -> Result<Value, String> {
    request_query(
        Method::GET,
        "/api/dispatched-sessions/claude-session-id",
        &routes::dispatched_sessions::DeleteSessionQuery {
            session_key: session_key.to_string(),
            provider: provider.map(str::to_string),
        },
    )
    .await
}

pub(super) async fn get_config_entries() -> Result<Value, String> {
    request_json(Method::GET, "/api/settings/config").await
}

pub(super) async fn get_escalation_settings() -> Result<Value, String> {
    request_json(Method::GET, "/api/settings/escalation").await
}

pub(super) async fn put_escalation_settings(
    settings: routes::escalation::EscalationSettings,
) -> Result<Value, String> {
    request_body(Method::PUT, "/api/settings/escalation", &settings).await
}

pub(super) async fn fetch_dispatch(dispatch_id: &str) -> Result<Value, String> {
    request_json(Method::GET, &format!("/api/dispatches/{dispatch_id}")).await
}

pub(super) async fn update_dispatch(
    dispatch_id: &str,
    body: routes::dispatches::UpdateDispatchBody,
) -> Result<Value, String> {
    request_body(
        Method::PATCH,
        &format!("/api/dispatches/{dispatch_id}"),
        &body,
    )
    .await
}

pub(super) async fn submit_review_decision(
    body: routes::review_verdict::ReviewDecisionBody,
) -> Result<Value, String> {
    request_body(Method::POST, "/api/review-decision", &body).await
}

pub(super) async fn submit_review_verdict(
    body: routes::review_verdict::SubmitVerdictBody,
) -> Result<Value, String> {
    request_body(Method::POST, "/api/review-verdict", &body).await
}

pub(super) async fn upsert_meeting(
    body: routes::meetings::UpsertMeetingBody,
) -> Result<Value, String> {
    request_body(Method::POST, "/api/round-table-meetings", &body).await
}

pub(crate) fn set_kv_value(key: &str, value: &str) -> Result<(), String> {
    let ctx = load_context()?;
    let Some(pool) = ctx.pg_pool.as_ref() else {
        return Err("direct runtime pg context is unavailable".to_string());
    };
    let key = key.to_string();
    let value = value.to_string();
    crate::utils::async_bridge::block_on_pg_result(
        pool,
        move |bridge_pool| async move {
            sqlx::query(
                "INSERT INTO kv_meta (key, value, expires_at)
                 VALUES ($1, $2, NULL)
                 ON CONFLICT (key) DO UPDATE
                 SET value = EXCLUDED.value,
                     expires_at = EXCLUDED.expires_at",
            )
            .bind(&key)
            .bind(&value)
            .execute(&bridge_pool)
            .await
            .map_err(|err| format!("upsert pg kv_meta {key}: {err}"))?;
            Ok(())
        },
        |error| error,
    )
}

pub(crate) fn get_kv_value(key: &str) -> Result<Option<String>, String> {
    let ctx = load_context()?;
    let Some(pool) = ctx.pg_pool.as_ref() else {
        return Err("direct runtime pg context is unavailable".to_string());
    };
    let key = key.to_string();
    crate::utils::async_bridge::block_on_pg_result(
        pool,
        move |bridge_pool| async move {
            sqlx::query_scalar::<_, String>(
                "SELECT value
                 FROM kv_meta
                 WHERE key = $1
                   AND (expires_at IS NULL OR expires_at > NOW())
                 LIMIT 1",
            )
            .bind(&key)
            .fetch_optional(&bridge_pool)
            .await
            .map_err(|err| format!("load pg kv_meta {key}: {err}"))
        },
        |error| error,
    )
}

pub(super) fn take_kv_value(key: &str) -> Result<Option<String>, String> {
    let ctx = load_context()?;
    let Some(pool) = ctx.pg_pool.as_ref() else {
        return Err("direct runtime pg context is unavailable".to_string());
    };
    let key = key.to_string();
    crate::utils::async_bridge::block_on_pg_result(
        pool,
        move |bridge_pool| async move {
            sqlx::query_scalar::<_, String>(
                "DELETE FROM kv_meta
                 WHERE key = $1
                   AND (expires_at IS NULL OR expires_at > NOW())
                 RETURNING value",
            )
            .bind(&key)
            .fetch_optional(&bridge_pool)
            .await
            .map_err(|err| format!("take pg kv_meta {key}: {err}"))
        },
        |error| error,
    )
}

pub(crate) fn delete_kv_value(key: &str) -> Result<(), String> {
    let ctx = load_context()?;
    let Some(pool) = ctx.pg_pool.as_ref() else {
        return Err("direct runtime pg context is unavailable".to_string());
    };
    let key = key.to_string();
    crate::utils::async_bridge::block_on_pg_result(
        pool,
        move |bridge_pool| async move {
            sqlx::query("DELETE FROM kv_meta WHERE key = $1")
                .bind(&key)
                .execute(&bridge_pool)
                .await
                .map_err(|err| format!("delete pg kv_meta {key}: {err}"))?;
            Ok(())
        },
        |error| error,
    )
}

pub(super) fn clear_kv_prefix(prefix: &str) -> Result<(), String> {
    let ctx = load_context()?;
    let Some(pool) = ctx.pg_pool.as_ref() else {
        return Err("direct runtime pg context is unavailable".to_string());
    };
    let prefix_text = prefix.to_string();
    let pattern = format!("{prefix}%");
    crate::utils::async_bridge::block_on_pg_result(
        pool,
        move |bridge_pool| async move {
            sqlx::query("DELETE FROM kv_meta WHERE key LIKE $1")
                .bind(&pattern)
                .execute(&bridge_pool)
                .await
                .map_err(|err| format!("delete pg kv_meta prefix {prefix_text}: {err}"))?;
            Ok(())
        },
        |error| error,
    )
}

pub(super) async fn gc_stale_thread_sessions() -> Result<usize, String> {
    let body = request_json(Method::DELETE, "/api/dispatched-sessions/gc-threads").await?;
    Ok(body
        .get("gc_threads")
        .and_then(|value| value.as_u64())
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(0))
}
