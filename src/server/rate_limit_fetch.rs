use std::sync::OnceLock;

use anyhow::Result;
use serde::Serialize;

use super::{Duration, PgPool, rate_limit_sync, worker_registry};

pub(super) const CLAUDE_RATE_LIMIT_FORCED_REFRESH_TIMEOUT: Duration = Duration::from_secs(8);

pub(super) fn claude_rate_limit_refresh_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ClaudeRateLimitRefreshOutcome {
    pub triggered: bool,
    pub dispatch_gate_refreshed: bool,
    pub refreshed_at: Option<i64>,
    pub reason: Option<&'static str>,
    pub error: Option<String>,
}

impl ClaudeRateLimitRefreshOutcome {
    fn scheduled() -> Self {
        Self {
            triggered: true,
            dispatch_gate_refreshed: false,
            refreshed_at: None,
            reason: Some("refresh_scheduled"),
            error: None,
        }
    }

    fn skipped(reason: &'static str) -> Self {
        Self {
            triggered: false,
            dispatch_gate_refreshed: false,
            refreshed_at: None,
            reason: Some(reason),
            error: None,
        }
    }

    fn failed(error: anyhow::Error) -> Self {
        Self {
            triggered: false,
            dispatch_gate_refreshed: false,
            refreshed_at: None,
            reason: Some("sync_failed"),
            error: Some(error.to_string()),
        }
    }
}

pub(crate) fn spawn_claude_rate_limit_refresh_if_leader(
    pg_pool: PgPool,
) -> super::ClaudeRateLimitRefreshOutcome {
    if !worker_registry::rate_limit_sync_active() {
        return ClaudeRateLimitRefreshOutcome::skipped("rate_limit_sync_not_active_on_this_node");
    }

    tokio::spawn(async move {
        match tokio::time::timeout(
            CLAUDE_RATE_LIMIT_FORCED_REFRESH_TIMEOUT,
            super::trigger_claude_rate_limit_refresh_if_leader(&pg_pool),
        )
        .await
        {
            Ok(outcome) if outcome.error.is_none() => {
                tracing::info!(
                    triggered = outcome.triggered,
                    dispatch_gate_refreshed = outcome.dispatch_gate_refreshed,
                    refreshed_at = ?outcome.refreshed_at,
                    reason = ?outcome.reason,
                    "[rate-limit-sync] dashboard-triggered Claude refresh completed"
                );
            }
            Ok(outcome) => {
                tracing::warn!(
                    reason = ?outcome.reason,
                    error = ?outcome.error,
                    "[rate-limit-sync] dashboard-triggered Claude refresh failed"
                );
            }
            Err(_) => {
                tracing::warn!(
                    timeout_secs = CLAUDE_RATE_LIMIT_FORCED_REFRESH_TIMEOUT.as_secs(),
                    "[rate-limit-sync] dashboard-triggered Claude refresh timed out"
                );
            }
        }
    });

    ClaudeRateLimitRefreshOutcome::scheduled()
}

pub(crate) async fn trigger_claude_rate_limit_refresh_if_leader(
    pg_pool: &PgPool,
) -> ClaudeRateLimitRefreshOutcome {
    if !worker_registry::rate_limit_sync_active() {
        return ClaudeRateLimitRefreshOutcome::skipped("rate_limit_sync_not_active_on_this_node");
    }

    match rate_limit_sync::sync_claude_rate_limit_cache_once_and_refresh_dispatch_gate_serialized(
        pg_pool,
    )
    .await
    {
        Ok(_) => ClaudeRateLimitRefreshOutcome {
            triggered: true,
            dispatch_gate_refreshed: true,
            refreshed_at: Some(chrono::Utc::now().timestamp()),
            reason: None,
            error: None,
        },
        Err(error) => ClaudeRateLimitRefreshOutcome::failed(error),
    }
}

/// Rebuild the `dispatch_gate` snapshots off the hot dispatch path, once per rate-limit tick.
/// This only warms the leader; followers refresh lazily via `refresh_snapshots_if_stale`.
pub(super) async fn refresh_dispatch_gate_snapshots(pg_pool: &PgPool) {
    let now = chrono::Utc::now().timestamp();
    crate::services::dispatch_gate::refresh_snapshots_from_db(pg_pool, now).await;
    // Keeps the activation-path throttle from redundantly re-reading the cache.
    crate::services::dispatch_gate::mark_snapshots_refreshed(now);
}

/// Fetch rate limits from the OpenAI API via the models endpoint (free, read-only).
/// Parses `x-ratelimit-*` response headers into bucket format.
pub(super) async fn fetch_openai_rate_limits(
    api_key: &str,
) -> Result<Vec<serde_json::Value>, anyhow::Error> {
    let client = reqwest::Client::new();
    let resp = client
        .get("https://api.openai.com/v1/models")
        .header("authorization", format!("Bearer {api_key}"))
        .send()
        .await?;

    let headers = resp.headers().clone();
    let mut buckets = Vec::new();

    if let Some(limit) = parse_header_i64(&headers, "x-ratelimit-limit-requests") {
        let remaining =
            parse_header_i64(&headers, "x-ratelimit-remaining-requests").unwrap_or(limit);
        let reset = parse_header_reset(&headers, "x-ratelimit-reset-requests");
        buckets.push(serde_json::json!({
            "name": "requests",
            "limit": limit,
            "used": limit - remaining,
            "remaining": remaining,
            "reset": reset,
        }));
    }

    if let Some(limit) = parse_header_i64(&headers, "x-ratelimit-limit-tokens") {
        let remaining = parse_header_i64(&headers, "x-ratelimit-remaining-tokens").unwrap_or(limit);
        let reset = parse_header_reset(&headers, "x-ratelimit-reset-tokens");
        buckets.push(serde_json::json!({
            "name": "tokens",
            "limit": limit,
            "used": limit - remaining,
            "remaining": remaining,
            "reset": reset,
        }));
    }

    Ok(buckets)
}

pub(super) fn parse_header_i64(headers: &reqwest::header::HeaderMap, name: &str) -> Option<i64> {
    headers.get(name)?.to_str().ok()?.parse().ok()
}

/// Parse a rate-limit reset header into unix epoch seconds. Accepts RFC3339 (Anthropic) and
/// relative durations such as `6m0s` (OpenAI).
pub(super) fn parse_header_reset(headers: &reqwest::header::HeaderMap, name: &str) -> i64 {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| parse_rate_limit_reset_value(s, chrono::Utc::now().timestamp()))
        .unwrap_or(0)
}

pub(super) fn parse_rate_limit_reset_value(value: &str, now: i64) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|dt| dt.timestamp())
        .or_else(|| parse_rate_limit_duration_seconds(value).map(|seconds| now + seconds))
}

pub(super) fn parse_rate_limit_duration_seconds(value: &str) -> Option<i64> {
    let input = value.trim();
    if input.is_empty() {
        return None;
    }
    if let Ok(seconds) = input.parse::<i64>() {
        return Some(seconds.max(0));
    }

    let mut total = 0.0_f64;
    let mut number = String::new();
    let mut consumed = false;
    let chars = input.chars().collect::<Vec<_>>();
    let mut index = 0;
    while index < chars.len() {
        let ch = chars[index];
        if ch.is_ascii_digit() || ch == '.' {
            number.push(ch);
            index += 1;
            continue;
        }
        if number.is_empty() {
            return None;
        }
        let amount = number.parse::<f64>().ok()?;
        number.clear();
        let multiplier = match ch {
            'd' => 86_400.0,
            'h' => 3_600.0,
            'm' => {
                if chars.get(index + 1) == Some(&'s') {
                    index += 1;
                    0.001
                } else {
                    60.0
                }
            }
            's' => 1.0,
            _ => return None,
        };
        total += amount * multiplier;
        consumed = true;
        index += 1;
    }

    if !number.is_empty() || !consumed {
        return None;
    }
    Some(total.ceil().max(0.0) as i64)
}

fn push_claude_oauth_usage_bucket(
    buckets: &mut Vec<serde_json::Value>,
    bucket: &serde_json::Value,
    label: &str,
    utilization_field: &str,
) {
    let Some(utilization) = bucket
        .get(utilization_field)
        .and_then(serde_json::Value::as_f64)
    else {
        return;
    };
    let resets_at = bucket
        .get("resets_at")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    // Also report used/limit, but keep the precise `utilization` so the dispatch gate
    // cannot round 99.5% into a false 100% saturation.
    let limit = 100i64;
    let used = utilization.floor().clamp(0.0, 100.0) as i64;
    let reset_ts = chrono::DateTime::parse_from_rfc3339(resets_at)
        .map(|dt| dt.timestamp())
        .unwrap_or(0);

    buckets.push(serde_json::json!({
        "name": label,
        "limit": limit,
        "used": used,
        "remaining": limit - used,
        "utilization": utilization,
        "reset": reset_ts,
    }));
}

pub(super) fn parse_claude_oauth_usage_buckets(data: &serde_json::Value) -> Vec<serde_json::Value> {
    let mut buckets = Vec::new();
    for (key, label) in [("five_hour", "5h"), ("seven_day", "7d")] {
        if let Some(bucket) = data.get(key) {
            push_claude_oauth_usage_bucket(&mut buckets, bucket, label, "utilization");
        }
    }
    if let Some(bucket) = data
        .get("limits")
        .and_then(serde_json::Value::as_array)
        .and_then(|limits| {
            limits.iter().find(|bucket| {
                bucket.get("kind").and_then(serde_json::Value::as_str) == Some("weekly_scoped")
                    && bucket
                        .pointer("/scope/model/display_name")
                        .and_then(serde_json::Value::as_str)
                        == Some("Fable")
                    && bucket
                        .get("percent")
                        .and_then(serde_json::Value::as_f64)
                        .is_some()
            })
        })
    {
        push_claude_oauth_usage_bucket(&mut buckets, bucket, "7d Fable", "percent");
    }
    buckets
}

pub(super) async fn fetch_codex_oauth_usage(
    token: &str,
) -> Result<Vec<serde_json::Value>, anyhow::Error> {
    let client = reqwest::Client::new();
    let resp = client
        .get("https://chatgpt.com/backend-api/codex/usage")
        .header("authorization", format!("Bearer {token}"))
        .header("user-agent", "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36")
        .header("accept", "application/json")
        .send()
        .await?;

    if !resp.status().is_success() {
        return Err(anyhow::anyhow!(
            "Codex usage API returned {}",
            resp.status()
        ));
    }

    let data: serde_json::Value = resp.json().await?;
    let mut buckets = Vec::new();

    if let Some(rl) = data.get("rate_limit") {
        for window_key in &["primary_window", "secondary_window"] {
            if let Some(window) = rl.get(window_key) {
                let used_percent = window
                    .get("used_percent")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);
                let window_seconds = window
                    .get("limit_window_seconds")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0);
                let reset_at = window.get("reset_at").and_then(|v| v.as_i64()).unwrap_or(0);

                let label = if window_seconds <= 18000 {
                    "5h"
                } else if window_seconds <= 86400 {
                    "1d"
                } else {
                    "7d"
                };

                let limit = 100i64;
                let used = used_percent.round() as i64;

                buckets.push(serde_json::json!({
                    "name": label,
                    "limit": limit,
                    "used": used,
                    "remaining": limit - used,
                    "reset": reset_at,
                }));
            }
        }
    }

    Ok(buckets)
}
