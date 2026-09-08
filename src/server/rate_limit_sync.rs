//! Periodic provider rate-limit sync: the `rate_limit_sync_loop` worker body plus the Claude
//! leg's credential selection and header/OAuth fetchers, lifted verbatim out of `server/mod.rs`
//! so the 429 backoff work tracked by #5727 does not grow that giant file any further.
//!
//! This module is a pure relocation. Every item below is byte-for-byte the body `server/mod.rs`
//! held at the parent commit; the only edits are the `pub(super)` markers the two cross-module
//! call sites now need: `worker_registry::registry` and the forced-refresh entry point in
//! `server/mod.rs`.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use sqlx::PgPool;

use super::{
    CLAUDE_RATE_LIMIT_FORCED_REFRESH_TIMEOUT, GEMINI_CREDS_MISSING_WARNED,
    claude_rate_limit_refresh_lock, fetch_codex_oauth_usage, fetch_gemini_rate_limits,
    fetch_openai_rate_limits, parse_claude_oauth_usage_buckets, parse_header_i64,
    parse_header_reset, refresh_dispatch_gate_snapshots, upsert_rate_limit_cache_entry,
};

pub(super) async fn rate_limit_sync_loop(pg_pool: Arc<PgPool>) {
    use std::time::Duration;

    let interval = Duration::from_secs(120);
    // Run immediately on startup, then every 2 minutes
    let mut first = true;

    loop {
        if !first {
            tokio::time::sleep(interval).await;
        }
        first = false;

        let _ = sync_claude_rate_limit_cache_once_serialized(pg_pool.as_ref()).await;

        // --- Codex rate limits ---
        // Priority: 1) ~/.codex/auth.json (Codex CLI subscription), 2) OPENAI_API_KEY
        let codex_result = if let Some(token) = crate::services::provider_auth::codex_access_token()
        {
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
                upsert_rate_limit_cache_entry(pg_pool.as_ref(), "codex", &data, now).await;
                tracing::info!("[rate-limit-sync] Codex: {} buckets cached", buckets.len());
            }
            Err(e) => {
                tracing::warn!("[rate-limit-sync] Codex rate_limit fetch failed: {e}");
            }
        }

        // --- Gemini rate limits ---
        // Uses OAuth2 creds from ~/.gemini/oauth_creds.json.
        // Returns RPM/RPD buckets with known quota limits; usage fields are -1 (unavailable).
        match fetch_gemini_rate_limits().await {
            Ok(buckets) => {
                let n = buckets.len();
                let data = serde_json::json!({ "buckets": buckets }).to_string();
                let now = chrono::Utc::now().timestamp();
                upsert_rate_limit_cache_entry(pg_pool.as_ref(), "gemini", &data, now).await;
                tracing::info!("[rate-limit-sync] Gemini: {} buckets cached", n);
            }
            Err(e) => {
                let msg = e.to_string();
                // Only suppress the genuine "not configured / file missing" case,
                // classified at the source (provider_auth) by `io::ErrorKind`:
                //   - "no home dir"            (no $HOME)
                //   - NotFound                 (oauth_creds.json does not exist)
                // PermissionDenied / IsADirectory / transient I/O are tagged
                // differently and corrupt/partial creds ("no access_token" /
                // "no refresh_token") are separate problems — all keep WARNing,
                // so we deliberately do NOT match on "oauth_creds.json" broadly
                // here (#3566 over-suppress fix, codex r2).
                let creds_missing =
                    crate::services::provider_auth::is_gemini_unconfigured_error(&e);
                if creds_missing {
                    // Gemini simply isn't configured — log once, then drop to DEBUG
                    // so the 2-minute sync loop doesn't spam an identical WARN (#3566).
                    if !GEMINI_CREDS_MISSING_WARNED.swap(true, Ordering::AcqRel) {
                        tracing::warn!(
                            "[rate-limit-sync] Gemini credentials not configured ({msg}); suppressing further repeats"
                        );
                    } else {
                        tracing::debug!(
                            "[rate-limit-sync] Gemini credentials absent; skipping (suppressed)"
                        );
                    }
                } else {
                    // Transient errors (network/API/token refresh) and corrupt/partial
                    // credentials keep WARNing.
                    tracing::warn!("[rate-limit-sync] Gemini rate_limit fetch failed: {e}");
                }
            }
        }

        // feature: rate-limit-aware-dispatch-gate — refresh the process-wide
        // in-memory pressure + agent→provider snapshots that the auto-queue
        // dispatch gate reads O(1) off the hot path (no DB on dispatch).
        refresh_dispatch_gate_snapshots_serialized(pg_pool.as_ref()).await;
    }
}

async fn sync_claude_rate_limit_cache_once_serialized(
    pg_pool: &PgPool,
) -> Result<usize, anyhow::Error> {
    let _guard = claude_rate_limit_refresh_lock().lock().await;
    sync_claude_rate_limit_cache_once(pg_pool).await
}

pub(super) async fn sync_claude_rate_limit_cache_once_and_refresh_dispatch_gate_serialized(
    pg_pool: &PgPool,
) -> Result<usize, anyhow::Error> {
    let _guard = claude_rate_limit_refresh_lock().lock().await;
    let bucket_count = sync_claude_rate_limit_cache_once(pg_pool).await?;
    refresh_dispatch_gate_snapshots(pg_pool).await;
    Ok(bucket_count)
}

async fn refresh_dispatch_gate_snapshots_serialized(pg_pool: &PgPool) {
    let _guard = claude_rate_limit_refresh_lock().lock().await;
    refresh_dispatch_gate_snapshots(pg_pool).await;
}

async fn sync_claude_rate_limit_cache_once(pg_pool: &PgPool) -> Result<usize, anyhow::Error> {
    // Priority: 1) OAuth token (Claude Code subscription), 2) ANTHROPIC_API_KEY.
    let claude_result =
        if let Some(token) = crate::services::provider_auth::claude_oauth_token_blocking().await {
            fetch_claude_oauth_usage(&token).await
        } else if let Ok(api_key) = std::env::var("ANTHROPIC_API_KEY") {
            fetch_anthropic_rate_limits(&api_key).await
        } else {
            Err(anyhow::anyhow!("no Claude credentials found"))
        };

    match claude_result {
        Ok(buckets) => {
            let bucket_count = buckets.len();
            let data = serde_json::json!({ "buckets": buckets }).to_string();
            let now = chrono::Utc::now().timestamp();
            upsert_rate_limit_cache_entry(pg_pool, "claude", &data, now).await;
            tracing::info!("[rate-limit-sync] Claude: {} buckets cached", bucket_count);
            Ok(bucket_count)
        }
        Err(e) => {
            tracing::warn!("[rate-limit-sync] Claude rate_limit fetch failed: {e}");
            Err(e)
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

    // Parse requests bucket
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

    // Parse tokens bucket
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

/// Fetch Claude usage via OAuth API (subscription-based, no API key needed).
/// Returns utilization-based buckets (5h, 7d).
async fn fetch_claude_oauth_usage(token: &str) -> Result<Vec<serde_json::Value>, anyhow::Error> {
    let client = reqwest::Client::builder()
        .timeout(CLAUDE_RATE_LIMIT_FORCED_REFRESH_TIMEOUT)
        .build()?;
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
    Ok(parse_claude_oauth_usage_buckets(&data))
}
