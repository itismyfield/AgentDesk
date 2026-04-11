use anyhow::Result;

use crate::db::Db;
use crate::engine::PolicyEngine;

/// Background task that periodically fetches rate-limit data from external providers
/// and caches it in the `rate_limit_cache` table for the dashboard API.
pub(super) async fn rate_limit_sync_loop(db: Db) {
    use std::time::Duration;

    let interval = Duration::from_secs(120);
    let mut first = true;

    loop {
        if !first {
            tokio::time::sleep(interval).await;
        }
        first = false;

        let claude_result = if let Some(token) = get_claude_oauth_token() {
            fetch_claude_oauth_usage(&token).await
        } else if let Ok(api_key) = std::env::var("ANTHROPIC_API_KEY") {
            fetch_anthropic_rate_limits(&api_key).await
        } else {
            Err(anyhow::anyhow!("no Claude credentials found"))
        };
        match claude_result {
            Ok(buckets) => {
                let data = serde_json::json!({ "buckets": buckets }).to_string();
                let now = chrono::Utc::now().timestamp();
                if let Ok(conn) = db.lock() {
                    conn.execute(
                        "INSERT OR REPLACE INTO rate_limit_cache (provider, data, fetched_at) VALUES (?1, ?2, ?3)",
                        rusqlite::params!["claude", data, now],
                    )
                    .ok();
                }
                tracing::info!("[rate-limit-sync] Claude: {} buckets cached", buckets.len());
            }
            Err(error) => {
                tracing::warn!("[rate-limit-sync] Claude rate_limit fetch failed: {error}");
            }
        }

        let codex_result = if let Some(token) = load_codex_access_token() {
            fetch_codex_oauth_usage(&token).await
        } else if let Ok(api_key) = std::env::var("OPENAI_API_KEY") {
            fetch_openai_rate_limits(&api_key).await
        } else {
            Err(anyhow::anyhow!("no Codex credentials found"))
        };
        match codex_result {
            Ok(buckets) => {
                let data = serde_json::json!({ "buckets": buckets }).to_string();
                let now = chrono::Utc::now().timestamp();
                if let Ok(conn) = db.lock() {
                    conn.execute(
                        "INSERT OR REPLACE INTO rate_limit_cache (provider, data, fetched_at) VALUES (?1, ?2, ?3)",
                        rusqlite::params!["codex", data, now],
                    )
                    .ok();
                }
                tracing::info!("[rate-limit-sync] Codex: {} buckets cached", buckets.len());
            }
            Err(error) => {
                tracing::warn!("[rate-limit-sync] Codex rate_limit fetch failed: {error}");
            }
        }
    }
}

/// Fetch rate limits from the Anthropic API via the count_tokens endpoint (free, no token cost).
/// Parses `anthropic-ratelimit-*` response headers into bucket format.
async fn fetch_anthropic_rate_limits(
    api_key: &str,
) -> Result<Vec<serde_json::Value>, anyhow::Error> {
    let client = reqwest::Client::new();
    let resp = client
        .post("https://api.anthropic.com/v1/messages/count_tokens")
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .json(&serde_json::json!({
            "model": "claude-haiku-4-5-20251001",
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await?;

    let headers = resp.headers().clone();
    let mut buckets = Vec::new();

    if let Some(limit) = parse_header_i64(&headers, "anthropic-ratelimit-requests-limit") {
        let remaining =
            parse_header_i64(&headers, "anthropic-ratelimit-requests-remaining").unwrap_or(limit);
        let reset = parse_header_reset(&headers, "anthropic-ratelimit-requests-reset");
        buckets.push(serde_json::json!({
            "name": "requests",
            "limit": limit,
            "used": limit - remaining,
            "remaining": remaining,
            "reset": reset,
        }));
    }

    if let Some(limit) = parse_header_i64(&headers, "anthropic-ratelimit-tokens-limit") {
        let remaining =
            parse_header_i64(&headers, "anthropic-ratelimit-tokens-remaining").unwrap_or(limit);
        let reset = parse_header_reset(&headers, "anthropic-ratelimit-tokens-reset");
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

/// Fetch rate limits from the OpenAI API via the models endpoint (free, read-only).
/// Parses `x-ratelimit-*` response headers into bucket format.
async fn fetch_openai_rate_limits(api_key: &str) -> Result<Vec<serde_json::Value>, anyhow::Error> {
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

fn parse_header_i64(headers: &reqwest::header::HeaderMap, name: &str) -> Option<i64> {
    headers.get(name)?.to_str().ok()?.parse().ok()
}

fn parse_header_reset(headers: &reqwest::header::HeaderMap, name: &str) -> i64 {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| {
            chrono::DateTime::parse_from_rfc3339(value)
                .ok()
                .map(|date| date.timestamp())
        })
        .unwrap_or(0)
}

fn get_claude_oauth_token() -> Option<String> {
    if let Ok(output) = std::process::Command::new("security")
        .args([
            "find-generic-password",
            "-s",
            "Claude Code-credentials",
            "-w",
        ])
        .output()
    {
        if output.status.success() {
            if let Ok(raw) = String::from_utf8(output.stdout) {
                let raw = raw.trim();
                if let Ok(creds) = serde_json::from_str::<serde_json::Value>(raw) {
                    if let Some(token) = creds
                        .get("claudeAiOauth")
                        .and_then(|oauth| oauth.get("accessToken"))
                        .and_then(|value| value.as_str())
                    {
                        return Some(token.to_string());
                    }
                }
            }
        }
    }

    let home = dirs::home_dir()?;
    let cred_path = home.join(".claude").join(".credentials.json");
    let raw = std::fs::read_to_string(cred_path).ok()?;
    let creds: serde_json::Value = serde_json::from_str(&raw).ok()?;
    creds
        .get("claudeAiOauth")
        .and_then(|oauth| oauth.get("accessToken"))
        .and_then(|value| value.as_str())
        .map(|value| value.to_string())
}

/// Fetch Claude usage via OAuth API (subscription-based, no API key needed).
/// Returns utilization-based buckets (5h, 7d).
async fn fetch_claude_oauth_usage(token: &str) -> Result<Vec<serde_json::Value>, anyhow::Error> {
    let client = reqwest::Client::new();
    let resp = client
        .get("https://api.anthropic.com/api/oauth/usage")
        .header("accept", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("user-agent", "agentdesk/1.0.0")
        .send()
        .await?;

    if resp.status() == 429 {
        return Err(anyhow::anyhow!("Claude OAuth usage API rate limited (429)"));
    }
    if !resp.status().is_success() {
        return Err(anyhow::anyhow!(
            "Claude OAuth usage API returned {}",
            resp.status()
        ));
    }

    let data: serde_json::Value = resp.json().await?;
    let mut buckets = Vec::new();

    for key in &["five_hour", "seven_day", "seven_day_sonnet"] {
        if let Some(bucket) = data.get(key) {
            let utilization = bucket
                .get("utilization")
                .and_then(|value| value.as_f64())
                .unwrap_or(0.0);
            let resets_at = bucket
                .get("resets_at")
                .and_then(|value| value.as_str())
                .unwrap_or("");
            let label = match *key {
                "five_hour" => "5h",
                "seven_day" => "7d",
                "seven_day_sonnet" => "7d Sonnet",
                _ => key,
            };
            let limit = 100i64;
            let used = utilization.round() as i64;
            let reset_ts = chrono::DateTime::parse_from_rfc3339(resets_at)
                .map(|date| date.timestamp())
                .unwrap_or(0);

            buckets.push(serde_json::json!({
                "name": label,
                "limit": limit,
                "used": used,
                "remaining": limit - used,
                "reset": reset_ts,
            }));
        }
    }

    Ok(buckets)
}

fn load_codex_access_token() -> Option<String> {
    let home = dirs::home_dir()?;
    let auth_path = home.join(".codex").join("auth.json");
    let raw = std::fs::read_to_string(auth_path).ok()?;
    let auth: serde_json::Value = serde_json::from_str(&raw).ok()?;
    auth.get("tokens")
        .and_then(|tokens| tokens.get("access_token"))
        .and_then(|value| value.as_str())
        .map(|value| value.to_string())
}

/// Fetch Codex usage via chatgpt.com backend API (subscription-based, no API key needed).
async fn fetch_codex_oauth_usage(token: &str) -> Result<Vec<serde_json::Value>, anyhow::Error> {
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

    if let Some(rate_limit) = data.get("rate_limit") {
        for window_key in &["primary_window", "secondary_window"] {
            if let Some(window) = rate_limit.get(window_key) {
                let used_percent = window
                    .get("used_percent")
                    .and_then(|value| value.as_f64())
                    .unwrap_or(0.0);
                let window_seconds = window
                    .get("limit_window_seconds")
                    .and_then(|value| value.as_i64())
                    .unwrap_or(0);
                let reset_at = window
                    .get("reset_at")
                    .and_then(|value| value.as_i64())
                    .unwrap_or(0);

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

/// Background task that periodically syncs GitHub issues for all registered repos.
pub(super) async fn github_sync_loop(db: Db, engine: PolicyEngine, interval_minutes: u64) {
    use std::time::Duration;

    if !crate::github::gh_available() {
        tracing::warn!("[github-sync] gh CLI not available — periodic sync disabled");
        return;
    }

    tracing::info!(
        "[github-sync] Periodic sync enabled (every {} minutes)",
        interval_minutes
    );

    let interval = Duration::from_secs(interval_minutes * 60);

    loop {
        tokio::time::sleep(interval).await;

        tracing::debug!("[github-sync] Running periodic sync...");

        let repos = match crate::github::list_repos(&db) {
            Ok(repos) => repos,
            Err(error) => {
                tracing::error!("[github-sync] Failed to list repos: {error}");
                continue;
            }
        };

        for repo in &repos {
            if !repo.sync_enabled {
                continue;
            }

            let issues = match crate::github::sync::fetch_issues(&repo.id) {
                Ok(issues) => issues,
                Err(error) => {
                    tracing::warn!("[github-sync] Fetch failed for {}: {error}", repo.id);
                    continue;
                }
            };

            match crate::github::triage::triage_new_issues(&db, &repo.id, &issues) {
                Ok(count) if count > 0 => {
                    tracing::info!("[github-sync] Triaged {count} new issues for {}", repo.id);
                }
                Err(error) => {
                    tracing::warn!("[github-sync] Triage failed for {}: {error}", repo.id);
                }
                _ => {}
            }

            match crate::github::sync::sync_github_issues_for_repo(&db, &engine, &repo.id, &issues)
            {
                Ok(result) => {
                    if result.closed_count > 0 || result.inconsistency_count > 0 {
                        tracing::info!(
                            "[github-sync] {}: closed={}, inconsistencies={}",
                            repo.id,
                            result.closed_count,
                            result.inconsistency_count
                        );
                    }
                }
                Err(error) => {
                    tracing::error!("[github-sync] Sync failed for {}: {error}", repo.id);
                }
            }
        }
    }
}

/// Async worker that drains the message_outbox table and delivers via /api/send (#120).
/// Runs every 2 seconds, processes up to 10 messages per tick.
pub(super) async fn message_outbox_loop(db: Db, port: u16) {
    use std::time::Duration;

    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            tracing::error!("[outbox] Failed to create HTTP client: {error}");
            return;
        }
    };

    let url = crate::config::local_api_url(port, "/api/send");

    tokio::time::sleep(Duration::from_secs(3)).await;
    tracing::info!("[outbox] Message outbox worker started (adaptive backoff 500ms-5s)");

    let mut poll_interval = Duration::from_millis(500);
    let max_interval = Duration::from_secs(5);

    loop {
        tokio::time::sleep(poll_interval).await;

        let pending: Vec<(i64, String, String, String, String)> = {
            let conn = match db.lock() {
                Ok(conn) => conn,
                Err(_) => continue,
            };
            let mut stmt = match conn.prepare(
                "SELECT id, target, content, bot, source FROM message_outbox \
                 WHERE status = 'pending' ORDER BY id ASC LIMIT 10",
            ) {
                Ok(stmt) => stmt,
                Err(_) => continue,
            };
            stmt.query_map([], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            })
            .ok()
            .map(|rows| rows.filter_map(|row| row.ok()).collect())
            .unwrap_or_default()
        };

        if pending.is_empty() {
            poll_interval = (poll_interval.mul_f64(1.5)).min(max_interval);
            continue;
        }
        poll_interval = Duration::from_millis(500);

        for (id, target, content, bot, source) in pending {
            let body = serde_json::json!({
                "target": target,
                "content": content,
                "bot": bot,
                "source": source,
            });

            match client.post(&url).json(&body).send().await {
                Ok(resp) if resp.status().is_success() => {
                    if let Ok(conn) = db.lock() {
                        conn.execute(
                            "UPDATE message_outbox SET status = 'sent', sent_at = datetime('now') WHERE id = ?1",
                            [id],
                        )
                        .ok();
                    }
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::debug!("[{ts}] [outbox] delivered msg {id} -> {target}");
                }
                Ok(resp) => {
                    let status = resp.status();
                    let err_text = resp.text().await.unwrap_or_default();
                    if let Ok(conn) = db.lock() {
                        conn.execute(
                            "UPDATE message_outbox SET status = 'failed', error = ?1 WHERE id = ?2",
                            rusqlite::params![format!("{status}: {err_text}"), id],
                        )
                        .ok();
                    }
                    tracing::warn!("[outbox] msg {id} -> {target} failed: {status}");
                }
                Err(error) => {
                    if let Ok(conn) = db.lock() {
                        conn.execute(
                            "UPDATE message_outbox SET status = 'failed', error = ?1 WHERE id = ?2",
                            rusqlite::params![error.to_string(), id],
                        )
                        .ok();
                    }
                    tracing::warn!("[outbox] msg {id} -> {target} error: {error}");
                }
            }
        }
    }
}

pub(super) async fn dm_reply_retry_loop(db: Db) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(300));
    interval.tick().await;
    loop {
        interval.tick().await;
        crate::services::discord::retry_failed_dm_notifications(&db).await;
    }
}
