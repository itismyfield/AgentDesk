//! Periodic provider rate-limit sync (`rate_limit_sync_loop`) and the Claude
//! leg's fetch/backoff wiring. Split out of `server/mod.rs` so the 429
//! backoff (#5727) does not grow that giant file; the shared helpers it
//! depends on (`upsert_rate_limit_cache_entry`, header parsers, the other
//! providers' fetchers, the refresh lock) remain in the parent module.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use sqlx::PgPool;

use super::rate_limit_backoff;
use super::{
    CLAUDE_RATE_LIMIT_FORCED_REFRESH_TIMEOUT, GEMINI_CREDS_MISSING_WARNED,
    claude_rate_limit_refresh_lock, fetch_codex_oauth_usage, fetch_gemini_rate_limits,
    fetch_openai_rate_limits, parse_claude_oauth_usage_buckets, parse_header_i64,
    parse_header_reset, refresh_dispatch_gate_snapshots, upsert_rate_limit_cache_entry,
};

/// Classifies one Claude sync result for the backoff schedule.
fn classify_claude_sync_result(
    result: &Result<usize, anyhow::Error>,
) -> rate_limit_backoff::ClaudeSyncOutcome {
    use rate_limit_backoff::{ClaudeSyncOutcome, ClaudeUsageRateLimited};
    match result {
        Ok(_) => ClaudeSyncOutcome::Success,
        Err(error) => match error.downcast_ref::<ClaudeUsageRateLimited>() {
            Some(rate_limited) => ClaudeSyncOutcome::RateLimited {
                retry_after: rate_limited.retry_after,
            },
            None => ClaudeSyncOutcome::OtherError,
        },
    }
}

pub(super) async fn rate_limit_sync_loop(pg_pool: Arc<PgPool>) {
    use rate_limit_backoff::{
        ClaudeSyncBackoff, ClaudeSyncOutcome, RATE_LIMIT_SYNC_BASE_INTERVAL,
        RATE_LIMIT_SYNC_MAX_BACKOFF,
    };
    use std::time::Instant;

    let interval = RATE_LIMIT_SYNC_BASE_INTERVAL;
    // Run immediately on startup, then every 2 minutes. The Claude leg
    // additionally backs off after 429s (Retry-After or exponential up to
    // 30 min) while the other providers keep the 2-minute cadence.
    let mut first = true;
    let mut claude_backoff = ClaudeSyncBackoff::new(interval, RATE_LIMIT_SYNC_MAX_BACKOFF);

    loop {
        if !first {
            tokio::time::sleep(interval).await;
        }
        first = false;

        let now = Instant::now();
        if claude_backoff.should_attempt(now) {
            let claude_result =
                sync_claude_rate_limit_cache_once_serialized(pg_pool.as_ref()).await;
            let outcome = classify_claude_sync_result(&claude_result);
            let is_rate_limited = matches!(outcome, ClaudeSyncOutcome::RateLimited { .. });
            let delay = claude_backoff.record(outcome, Instant::now());
            if is_rate_limited {
                // First 429 of a streak is WARN; the rest are INFO so a
                // sustained rate limit does not flood the log every tick.
                let consecutive = claude_backoff.consecutive_rate_limits();
                if consecutive <= 1 {
                    tracing::warn!(
                        backoff_secs = delay.as_secs(),
                        "[rate-limit-sync] Claude rate_limit fetch rate limited (429); backing off"
                    );
                } else {
                    tracing::info!(
                        backoff_secs = delay.as_secs(),
                        consecutive_429 = consecutive,
                        "[rate-limit-sync] Claude rate_limit fetch still rate limited (429); backing off"
                    );
                }
            }
        } else {
            tracing::debug!(
                remaining_secs = claude_backoff.remaining(now).as_secs(),
                "[rate-limit-sync] Claude fetch skipped: 429 backoff in effect"
            );
        }

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

pub(super) async fn sync_claude_rate_limit_cache_once_serialized(
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

pub(super) async fn refresh_dispatch_gate_snapshots_serialized(pg_pool: &PgPool) {
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
            // 429s are logged by the periodic loop with backoff context
            // (WARN on the first of a streak, INFO afterwards); everything
            // else stays a WARN here so dashboard-triggered refreshes and the
            // loop share one message.
            if e.downcast_ref::<rate_limit_backoff::ClaudeUsageRateLimited>()
                .is_some()
            {
                tracing::debug!("[rate-limit-sync] Claude rate_limit fetch failed: {e}");
            } else {
                tracing::warn!("[rate-limit-sync] Claude rate_limit fetch failed: {e}");
            }
            Err(e)
        }
    }
}

/// Builds the typed 429 error from a rate-limited response's headers.
fn claude_usage_rate_limited_error(
    headers: &reqwest::header::HeaderMap,
) -> rate_limit_backoff::ClaudeUsageRateLimited {
    let retry_after = headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| rate_limit_backoff::parse_retry_after(value, chrono::Utc::now()));
    rate_limit_backoff::ClaudeUsageRateLimited { retry_after }
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

    if resp.status() == 429 {
        return Err(anyhow::Error::new(claude_usage_rate_limited_error(
            resp.headers(),
        )));
    }

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
        return Err(anyhow::Error::new(claude_usage_rate_limited_error(
            resp.headers(),
        )));
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

#[cfg(test)]
mod tests {
    use super::super::rate_limit_backoff::{ClaudeSyncOutcome, ClaudeUsageRateLimited};
    use super::{classify_claude_sync_result, claude_usage_rate_limited_error};
    use std::time::Duration;

    #[test]
    fn classifies_claude_sync_results_for_backoff() {
        assert_eq!(
            classify_claude_sync_result(&Ok(2)),
            ClaudeSyncOutcome::Success
        );
        assert_eq!(
            classify_claude_sync_result(&Err(anyhow::anyhow!("no Claude credentials found"))),
            ClaudeSyncOutcome::OtherError
        );
        let rate_limited = anyhow::Error::new(ClaudeUsageRateLimited {
            retry_after: Some(Duration::from_secs(90)),
        });
        assert_eq!(
            classify_claude_sync_result(&Err(rate_limited)),
            ClaudeSyncOutcome::RateLimited {
                retry_after: Some(Duration::from_secs(90)),
            }
        );
        // Context wrapping must not hide the typed 429.
        let wrapped = anyhow::Error::new(ClaudeUsageRateLimited { retry_after: None })
            .context("forced refresh");
        assert_eq!(
            classify_claude_sync_result(&Err(wrapped)),
            ClaudeSyncOutcome::RateLimited { retry_after: None }
        );
    }

    #[test]
    fn rate_limited_error_reads_retry_after_header() {
        let mut headers = reqwest::header::HeaderMap::new();
        assert_eq!(claude_usage_rate_limited_error(&headers).retry_after, None);
        headers.insert(reqwest::header::RETRY_AFTER, "45".parse().unwrap());
        assert_eq!(
            claude_usage_rate_limited_error(&headers).retry_after,
            Some(Duration::from_secs(45))
        );
        headers.insert(reqwest::header::RETRY_AFTER, "garbage".parse().unwrap());
        assert_eq!(claude_usage_rate_limited_error(&headers).retry_after, None);
    }
}
