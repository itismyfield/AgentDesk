use super::dto::RateLimitsResponse;
use serde_json::{Value, json};
use sqlx::{PgPool, Row};
use std::collections::HashSet;

const UNSUPPORTED_RATE_LIMIT_PROVIDERS: &[(&str, &str)] = &[
    (
        "opencode",
        "No OpenCode rate-limit telemetry source is implemented yet.",
    ),
    (
        "qwen",
        "No Qwen rate-limit telemetry source is implemented yet.",
    ),
];
const UNSUPPORTED_RATE_LIMIT_USAGE_LOOKBACK_SECONDS: i64 = 30 * 24 * 60 * 60;

pub async fn rate_limits_pg(pool: &PgPool, now: i64) -> RateLimitsResponse {
    RateLimitsResponse {
        providers: build_rate_limit_provider_payloads_pg(pool, now).await,
    }
}

pub async fn build_rate_limit_provider_payloads_pg(pool: &PgPool, now: i64) -> Vec<Value> {
    let stale_sec =
        sqlx::query("SELECT value FROM kv_meta WHERE key = 'rateLimitStaleSec' LIMIT 1")
            .fetch_optional(pool)
            .await
            .ok()
            .flatten()
            .and_then(|row| row.try_get::<String, _>("value").ok())
            .and_then(|value| value.parse::<i64>().ok())
            .unwrap_or(600);

    let rows = match sqlx::query(
        "SELECT provider, data, fetched_at
         FROM rate_limit_cache
         ORDER BY provider",
    )
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows,
        Err(_) => return build_unsupported_rate_limit_entries_pg(pool, now).await,
    };

    let mut seen = HashSet::new();
    let mut providers = Vec::new();

    for row in rows {
        let provider = match row.try_get::<String, _>("provider") {
            Ok(provider) => provider,
            Err(_) => continue,
        };
        let data = match row.try_get::<String, _>("data") {
            Ok(data) => data,
            Err(_) => continue,
        };
        let fetched_at = match row.try_get::<i64, _>("fetched_at") {
            Ok(fetched_at) => fetched_at,
            Err(_) => continue,
        };

        let parsed: Value = match serde_json::from_str(&data) {
            Ok(parsed) => parsed,
            Err(_) => continue,
        };
        let buckets = parsed
            .get("buckets")
            .and_then(|value| value.as_array())
            .cloned()
            .unwrap_or_default();
        let unsupported = parsed
            .get("unsupported")
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        let reason = parsed
            .get("reason")
            .and_then(|value| value.as_str())
            .map(str::to_string);
        let stale = (now - fetched_at) > stale_sec;
        seen.insert(provider.to_lowercase());
        providers.push(json!({
            "provider": provider,
            "buckets": buckets,
            "fetched_at": fetched_at,
            "stale": stale,
            "unsupported": unsupported,
            "reason": reason,
        }));
    }

    for (provider, reason) in UNSUPPORTED_RATE_LIMIT_PROVIDERS {
        if seen.contains(*provider) {
            continue;
        }
        if !provider_has_recent_session_usage_pg(pool, provider, now).await {
            continue;
        }
        providers.push(unsupported_provider_entry(provider, reason, now));
    }

    sort_provider_payloads(&mut providers);
    providers
}

async fn provider_has_recent_session_usage_pg(pool: &PgPool, provider: &str, now: i64) -> bool {
    let threshold = now.saturating_sub(UNSUPPORTED_RATE_LIMIT_USAGE_LOOKBACK_SECONDS);
    sqlx::query(
        "SELECT 1
         FROM sessions
         WHERE lower(provider) = lower($1)
           AND COALESCE(
                 EXTRACT(EPOCH FROM last_heartbeat)::BIGINT,
                 EXTRACT(EPOCH FROM created_at)::BIGINT,
                 0
               ) >= $2
         LIMIT 1",
    )
    .bind(provider)
    .bind(threshold)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
    .is_some()
}

async fn build_unsupported_rate_limit_entries_pg(pool: &PgPool, now: i64) -> Vec<Value> {
    let mut providers = Vec::new();
    for (provider, reason) in UNSUPPORTED_RATE_LIMIT_PROVIDERS {
        if provider_has_recent_session_usage_pg(pool, provider, now).await {
            providers.push(unsupported_provider_entry(provider, reason, now));
        }
    }
    providers
}

fn unsupported_provider_entry(provider: &str, reason: &str, now: i64) -> Value {
    json!({
        "provider": provider,
        "buckets": [],
        "fetched_at": now,
        "stale": false,
        "unsupported": true,
        "reason": reason,
    })
}

fn sort_provider_payloads(providers: &mut [Value]) {
    providers.sort_by_key(|entry| {
        provider_sort_key(
            entry
                .get("provider")
                .and_then(|value| value.as_str())
                .unwrap_or_default(),
        )
    });
}

fn provider_sort_key(provider: &str) -> u8 {
    match provider.to_lowercase().as_str() {
        "claude" => 0,
        "codex" => 1,
        "gemini" => 2,
        "opencode" => 3,
        "qwen" => 4,
        _ => 9,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sort_provider_payloads_uses_stable_dashboard_order() {
        let mut providers = vec![
            json!({"provider": "qwen"}),
            json!({"provider": "other"}),
            json!({"provider": "codex"}),
            json!({"provider": "claude"}),
            json!({"provider": "gemini"}),
            json!({"provider": "opencode"}),
        ];

        sort_provider_payloads(&mut providers);

        let order = providers
            .iter()
            .map(|entry| entry["provider"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            order,
            vec!["claude", "codex", "gemini", "opencode", "qwen", "other"]
        );
    }
}
