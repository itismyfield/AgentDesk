use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};
use std::time::Duration;

use reqwest::Method;
use serde::Serialize;
use serde_json::Value;
use sqlx::Row;

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
    request_body(Method::POST, "/api/dispatched-sessions/webhook", &body).await
}

pub(super) async fn delete_session(session_key: &str) -> Result<Value, String> {
    request_query(
        Method::DELETE,
        "/api/dispatched-sessions/webhook",
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

fn format_quality_rate(value: Option<f64>, unavailable: bool) -> String {
    if unavailable {
        return "측정 불가".to_string();
    }
    value
        .map(|rate| format!("{:.0}%", rate * 100.0))
        .unwrap_or_else(|| "n/a".to_string())
}

/// Snapshot of the metrics that drive the self-feedback prompt block.
/// Kept as a plain struct so the formatter can be exercised by unit tests
/// without a Postgres connection (#1103).
#[derive(Debug, Clone, Default)]
pub(crate) struct AgentQualitySnapshot {
    pub day: String,
    pub measurement_unavailable_7d: bool,
    pub turn_success_rate_7d: Option<f64>,
    pub review_pass_rate_7d: Option<f64>,
    pub turn_success_rate_30d: Option<f64>,
    pub review_pass_rate_30d: Option<f64>,
    pub avg_rework_count: Option<f64>,
    pub cost_per_done_card: Option<f64>,
    pub latency_p50_ms: Option<i64>,
    pub latency_p99_ms: Option<i64>,
    /// Top rework categories with counts, descending. Empty when no
    /// review_fail events exist or rework tagging produced nothing actionable.
    pub rework_categories: Vec<(String, i64)>,
}

fn format_latency_minutes(ms: Option<i64>) -> String {
    match ms {
        Some(value) if value > 0 => {
            let minutes = value as f64 / 60_000.0;
            format!("{minutes:.1}m")
        }
        _ => "n/a".to_string(),
    }
}

fn format_avg_rework(value: Option<f64>) -> String {
    value
        .map(|count| format!("{count:.1}"))
        .unwrap_or_else(|| "n/a".to_string())
}

fn format_cost_per_done(value: Option<f64>) -> String {
    value
        .map(|cost| format!("${cost:.2}"))
        .unwrap_or_else(|| "n/a".to_string())
}

fn format_rework_categories(categories: &[(String, i64)]) -> String {
    if categories.is_empty() {
        return "(no recent review_fail samples)".to_string();
    }
    categories
        .iter()
        .take(3)
        .map(|(label, count)| format!("{label} ×{count}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Pure formatter for the self-feedback prompt block. Output is stable
/// — given identical input, the returned string is byte-identical, which is
/// what guarantees Anthropic prefix-cache hits when the hourly bucket has not
/// rolled over yet (#1103).
pub(crate) fn format_agent_performance_section(snapshot: &AgentQualitySnapshot) -> String {
    let unavailable = snapshot.measurement_unavailable_7d;
    format!(
        "[Agent Performance — Last 7 Days]\n\
         Review pass rate: {} (30d baseline: {})\n\
         Avg rework: {}\n\
         Turn success: {} (30d baseline: {})\n\
         Cost/done: {}\n\
         Latency p50: {} / p99: {}\n\
         Top rework reasons: {}\n\
         Self-feedback: when a metric is down or 측정 불가, shrink turns, verify before final status, and leave explicit evidence.\n\
         Rollup day: {}",
        format_quality_rate(snapshot.review_pass_rate_7d, unavailable),
        format_quality_rate(snapshot.review_pass_rate_30d, false),
        format_avg_rework(snapshot.avg_rework_count),
        format_quality_rate(snapshot.turn_success_rate_7d, unavailable),
        format_quality_rate(snapshot.turn_success_rate_30d, false),
        format_cost_per_done(snapshot.cost_per_done_card),
        format_latency_minutes(snapshot.latency_p50_ms),
        format_latency_minutes(snapshot.latency_p99_ms),
        format_rework_categories(&snapshot.rework_categories),
        snapshot.day,
    )
}

/// Map a free-form review_fail payload (`notes` text + `items` strings) to a
/// fixed coarse category. Pure function — no DB, deterministic, easy to test.
/// Categories are intentionally small so the prompt stays cache-stable across
/// minor wording variations in reviewer notes.
pub(crate) fn classify_rework_text(text: &str) -> &'static str {
    let lower = text.to_ascii_lowercase();
    let contains_any = |needles: &[&str]| needles.iter().any(|needle| lower.contains(needle));

    if contains_any(&[
        "test",
        "테스트",
        "coverage",
        "커버리지",
        "assert",
        "spec",
        "fixture",
    ]) {
        "test"
    } else if contains_any(&[
        "null",
        "none",
        "nil",
        "edge case",
        "edge-case",
        "엣지",
        "예외 ",
        "예외처리",
        "boundary",
    ]) {
        "edge-case"
    } else if contains_any(&["style", "format", "포맷", "lint", "rustfmt", "clippy"]) {
        "style"
    } else if contains_any(&["naming", "rename", "이름", "변수명", "함수명"]) {
        "naming"
    } else if contains_any(&["doc", "docstring", "주석", "comment", "readme", "문서"]) {
        "docs"
    } else if contains_any(&["perf", "performance", "성능", "slow", "latency"]) {
        "perf"
    } else if contains_any(&["security", "보안", "vuln", "secret", "credential"]) {
        "security"
    } else if contains_any(&["refactor", "리팩"]) {
        "refactor"
    } else if contains_any(&["spec", "scope", "스펙", "범위", "요구사항", "missing"]) {
        "spec"
    } else {
        "logic"
    }
}

fn aggregate_rework_categories(payloads: &[Value]) -> Vec<(String, i64)> {
    let mut counts: HashMap<&'static str, i64> = HashMap::new();
    for payload in payloads {
        let mut texts: Vec<String> = Vec::new();
        if let Some(notes) = payload.get("notes").and_then(|value| value.as_str()) {
            texts.push(notes.to_string());
        }
        if let Some(items) = payload.get("items").and_then(|value| value.as_array()) {
            for item in items {
                if let Some(s) = item.as_str() {
                    texts.push(s.to_string());
                }
            }
        }
        if texts.is_empty() {
            // Even without text we record the bucket so the pass-rate context
            // is preserved; an empty review_fail counts as "logic" by default.
            *counts.entry("logic").or_insert(0) += 1;
            continue;
        }
        for text in &texts {
            let category = classify_rework_text(text);
            *counts.entry(category).or_insert(0) += 1;
        }
    }
    let mut sorted: Vec<(String, i64)> = counts
        .into_iter()
        .map(|(label, count)| (label.to_string(), count))
        .collect();
    sorted.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    sorted
}

pub(crate) fn get_agent_quality_prompt_section(agent_id: &str) -> Result<Option<String>, String> {
    let agent_id = agent_id.trim();
    if agent_id.is_empty() {
        return Ok(None);
    }
    let ctx = load_context()?;
    let Some(pool) = ctx.pg_pool.as_ref() else {
        return Ok(None);
    };
    let agent_id_owned = agent_id.to_string();
    crate::utils::async_bridge::block_on_pg_result(
        pool,
        move |bridge_pool| async move {
            let row = sqlx::query(
                "SELECT to_char(day, 'YYYY-MM-DD') AS day_text,
                        measurement_unavailable_7d,
                        turn_success_rate_7d,
                        review_pass_rate_7d,
                        turn_success_rate_30d,
                        review_pass_rate_30d,
                        avg_rework_count,
                        cost_per_done_card,
                        latency_p50_ms,
                        latency_p99_ms
                 FROM agent_quality_daily
                 WHERE agent_id = $1
                 ORDER BY day DESC
                 LIMIT 1",
            )
            .bind(&agent_id_owned)
            .fetch_optional(&bridge_pool)
            .await
            .map_err(|err| format!("load agent quality prompt section {agent_id_owned}: {err}"))?;

            let Some(row) = row else {
                return Ok(None);
            };

            let day: String = row
                .try_get("day_text")
                .map_err(|err| format!("decode quality prompt day: {err}"))?;
            let unavailable: bool = row
                .try_get("measurement_unavailable_7d")
                .map_err(|err| format!("decode quality prompt unavailable: {err}"))?;
            let turn_7d: Option<f64> = row
                .try_get("turn_success_rate_7d")
                .map_err(|err| format!("decode quality prompt turn 7d: {err}"))?;
            let review_7d: Option<f64> = row
                .try_get("review_pass_rate_7d")
                .map_err(|err| format!("decode quality prompt review 7d: {err}"))?;
            let turn_30d: Option<f64> = row
                .try_get("turn_success_rate_30d")
                .map_err(|err| format!("decode quality prompt turn 30d: {err}"))?;
            let review_30d: Option<f64> = row
                .try_get("review_pass_rate_30d")
                .map_err(|err| format!("decode quality prompt review 30d: {err}"))?;
            let avg_rework: Option<f64> = row
                .try_get("avg_rework_count")
                .map_err(|err| format!("decode quality prompt avg rework: {err}"))?;
            let cost_per_done: Option<f64> = row
                .try_get("cost_per_done_card")
                .map_err(|err| format!("decode quality prompt cost: {err}"))?;
            let latency_p50: Option<i64> = row
                .try_get("latency_p50_ms")
                .map_err(|err| format!("decode quality prompt latency p50: {err}"))?;
            let latency_p99: Option<i64> = row
                .try_get("latency_p99_ms")
                .map_err(|err| format!("decode quality prompt latency p99: {err}"))?;

            // Pull the last 7 days of review_fail event payloads to drive the
            // rework category auto-tagging. Capped to 200 rows to keep the
            // hourly cache miss path bounded — the categoriser only needs the
            // top-3 anyway.
            let payload_rows = sqlx::query(
                "SELECT payload
                 FROM agent_quality_event
                 WHERE agent_id = $1
                   AND event_type = 'review_fail'
                   AND created_at >= NOW() - INTERVAL '7 days'
                 ORDER BY created_at DESC
                 LIMIT 200",
            )
            .bind(&agent_id_owned)
            .fetch_all(&bridge_pool)
            .await
            .map_err(|err| format!("load review_fail payloads for {agent_id_owned}: {err}"))?;

            let payloads: Vec<Value> = payload_rows
                .iter()
                .filter_map(|row| row.try_get::<Value, _>("payload").ok())
                .collect();
            let rework_categories = aggregate_rework_categories(&payloads);

            let snapshot = AgentQualitySnapshot {
                day,
                measurement_unavailable_7d: unavailable,
                turn_success_rate_7d: turn_7d,
                review_pass_rate_7d: review_7d,
                turn_success_rate_30d: turn_30d,
                review_pass_rate_30d: review_30d,
                avg_rework_count: avg_rework,
                cost_per_done_card: cost_per_done,
                latency_p50_ms: latency_p50,
                latency_p99_ms: latency_p99,
                rework_categories,
            };

            Ok(Some(format_agent_performance_section(&snapshot)))
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

#[cfg(test)]
mod self_feedback_tests {
    //! #1103 — Tests for the self-feedback prompt block and rework category
    //! tagging. These tests cover the pure formatter and classifier so they
    //! run without a database connection. Cache-stability and channel toggle
    //! tests live in `prompt_builder.rs` where the cache and feature flag are
    //! actually enforced.
    use super::*;

    fn baseline_snapshot() -> AgentQualitySnapshot {
        AgentQualitySnapshot {
            day: "2026-04-25".to_string(),
            measurement_unavailable_7d: false,
            turn_success_rate_7d: Some(0.94),
            review_pass_rate_7d: Some(0.78),
            turn_success_rate_30d: Some(0.92),
            review_pass_rate_30d: Some(0.82),
            avg_rework_count: Some(1.8),
            cost_per_done_card: Some(0.42),
            latency_p50_ms: Some(138_000),
            latency_p99_ms: Some(486_000),
            rework_categories: vec![
                ("test".to_string(), 5),
                ("edge-case".to_string(), 3),
                ("logic".to_string(), 2),
                ("style".to_string(), 1),
            ],
        }
    }

    #[test]
    fn format_section_matches_dod_layout() {
        let snapshot = baseline_snapshot();
        let block = format_agent_performance_section(&snapshot);

        // Header + each DoD-required line in order.
        assert!(block.starts_with("[Agent Performance — Last 7 Days]"));
        assert!(block.contains("Review pass rate: 78% (30d baseline: 82%)"));
        assert!(block.contains("Avg rework: 1.8"));
        assert!(block.contains("Turn success: 94% (30d baseline: 92%)"));
        assert!(block.contains("Cost/done: $0.42"));
        assert!(block.contains("Latency p50: 2.3m / p99: 8.1m"));
        // Top-3 categories surface, sorted by count desc.
        assert!(block.contains("Top rework reasons: test ×5, edge-case ×3, logic ×2"));
        assert!(!block.contains("style ×1"), "fourth category must be cut");
        assert!(block.contains("Rollup day: 2026-04-25"));
    }

    #[test]
    fn format_section_is_byte_stable_for_identical_input() {
        // Cache stability invariant: same snapshot ⇒ identical bytes. The
        // hourly cache in `prompt_builder` relies on this.
        let snapshot = baseline_snapshot();
        let a = format_agent_performance_section(&snapshot);
        let b = format_agent_performance_section(&snapshot);
        assert_eq!(a, b);
    }

    #[test]
    fn format_section_handles_unavailable_measurement() {
        let mut snapshot = baseline_snapshot();
        snapshot.measurement_unavailable_7d = true;
        snapshot.turn_success_rate_7d = None;
        snapshot.review_pass_rate_7d = None;
        snapshot.avg_rework_count = None;
        snapshot.cost_per_done_card = None;
        snapshot.latency_p50_ms = None;
        snapshot.latency_p99_ms = None;
        snapshot.rework_categories.clear();

        let block = format_agent_performance_section(&snapshot);
        assert!(block.contains("Review pass rate: 측정 불가"));
        assert!(block.contains("Turn success: 측정 불가"));
        // 30d baseline must still render (we never mark 30d as unavailable here).
        assert!(block.contains("(30d baseline: 82%)"));
        assert!(block.contains("Avg rework: n/a"));
        assert!(block.contains("Cost/done: n/a"));
        assert!(block.contains("Latency p50: n/a / p99: n/a"));
        assert!(block.contains("Top rework reasons: (no recent review_fail samples)"));
    }

    #[test]
    fn classify_rework_text_buckets_common_phrases() {
        assert_eq!(classify_rework_text("missing test for null path"), "test");
        assert_eq!(classify_rework_text("테스트 누락"), "test");
        assert_eq!(classify_rework_text("handle null pointer"), "edge-case");
        assert_eq!(classify_rework_text("엣지 케이스 처리 안됨"), "edge-case");
        assert_eq!(classify_rework_text("rustfmt failing"), "style");
        assert_eq!(classify_rework_text("naming inconsistent"), "naming");
        assert_eq!(classify_rework_text("docstring missing"), "docs");
        assert_eq!(classify_rework_text("performance regression"), "perf");
        assert_eq!(classify_rework_text("security: secret leaked"), "security");
        assert_eq!(classify_rework_text("리팩토링 필요"), "refactor");
        // Default falls through to logic when no keyword matches.
        assert_eq!(classify_rework_text("wrong branch order"), "logic");
    }

    #[test]
    fn aggregate_rework_categories_sorts_by_count_desc() {
        let payloads = vec![
            serde_json::json!({"verdict": "improve", "notes": "missing test", "items": []}),
            serde_json::json!({"verdict": "rework", "notes": "엣지 케이스 빠짐", "items": []}),
            serde_json::json!({"verdict": "improve", "notes": "test coverage low", "items": ["add test"]}),
            serde_json::json!({"verdict": "rework", "notes": null, "items": ["fix logic branch"]}),
        ];
        let agg = aggregate_rework_categories(&payloads);
        assert!(!agg.is_empty());
        // First two payloads contain "test" (1) + "test coverage" (2 hits via notes+items)
        // edge-case from the second payload.
        let top = &agg[0];
        assert_eq!(top.0, "test");
        assert!(top.1 >= 2, "{:?}", agg);
    }

    #[test]
    fn aggregate_rework_categories_empty_input_returns_empty() {
        let agg = aggregate_rework_categories(&[]);
        assert!(agg.is_empty());
    }
}
