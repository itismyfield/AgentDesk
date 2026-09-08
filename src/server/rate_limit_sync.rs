//! Periodic provider rate-limit sync (`rate_limit_sync_loop`) and the Claude
//! leg's fetch/backoff wiring, split out of `server/mod.rs` so the 429 backoff
//! (#5727) does not grow that giant file. The shared helpers stay there.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use sqlx::PgPool;

use crate::services::dispatch_gate;

use super::rate_limit_backoff;
use super::{
    CLAUDE_RATE_LIMIT_FORCED_REFRESH_TIMEOUT, GEMINI_CREDS_MISSING_WARNED,
    claude_rate_limit_refresh_lock, fetch_codex_oauth_usage, fetch_gemini_rate_limits,
    fetch_openai_rate_limits, parse_claude_oauth_usage_buckets, parse_header_i64,
    parse_header_reset, refresh_dispatch_gate_snapshots, upsert_rate_limit_cache_entry,
};

type Buckets = Vec<serde_json::Value>;

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
    // Run immediately on startup, then every 2 minutes. The Claude leg also
    // backs off after 429s while the other providers keep that cadence.
    let mut first = true;
    let mut claude_backoff = ClaudeSyncBackoff::new(interval, RATE_LIMIT_SYNC_MAX_BACKOFF);

    loop {
        if !first {
            tokio::time::sleep(interval).await;
        }
        first = false;

        let now = Instant::now();
        // #5727: pressure the gate still Defers on must be re-observed inside
        // its stale window. The snapshot was refreshed at the end of the last
        // tick, so reading it here costs no DB round trip.
        let now_unix = chrono::Utc::now().timestamp();
        if let Some(cap) = observation_deadline_cap(
            dispatch_gate::deferring_observation("claude", now_unix),
            now_unix,
            interval,
            Duration::from_secs(dispatch_gate::stale_sec().max(0) as u64),
        ) {
            claude_backoff.cap_hold(cap, now);
        }
        if claude_backoff.should_attempt(now) {
            let claude_result =
                sync_claude_rate_limit_cache_once_serialized(pg_pool.as_ref()).await;
            let outcome = classify_claude_sync_result(&claude_result);
            let is_rate_limited = matches!(outcome, ClaudeSyncOutcome::RateLimited { .. });
            let at = Instant::now();
            let delay = claude_backoff.record(outcome, at);
            if is_rate_limited {
                // First 429 of a streak is WARN, the rest INFO.
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

        // --- Gemini rate limits (OAuth2 creds from ~/.gemini/oauth_creds.json;
        // RPM/RPD buckets with known quota limits, usage fields -1) ---
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
                // Only the genuine "not configured / file missing" case is
                // suppressed, classified at the source by `io::ErrorKind`:
                // matching on "oauth_creds.json" here would also swallow
                // permission, I/O and corrupt-credential failures, which keep
                // WARNing (#3566 over-suppress fix, codex r2).
                let creds_missing =
                    crate::services::provider_auth::is_gemini_unconfigured_error(&e);
                if creds_missing {
                    // Not configured: log once, then DEBUG so the 2-minute
                    // loop doesn't spam an identical WARN (#3566).
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
                    // Transient (network/API/token refresh) and corrupt creds.
                    tracing::warn!("[rate-limit-sync] Gemini rate_limit fetch failed: {e}");
                }
            }
        }

        // feature: rate-limit-aware-dispatch-gate — refresh the process-wide
        // pressure + agent→provider snapshots the auto-queue dispatch gate
        // reads O(1) off the hot path (no DB on dispatch).
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
            // Telemetry is independent of retry scheduling: a 429 carrying
            // limit headers is cached anyway, so the gate sees the exhaustion.
            // A 429 with no buckets (OAuth) writes nothing — not exhausted.
            match e.downcast_ref::<rate_limit_backoff::ClaudeUsageRateLimited>() {
                Some(limited) => {
                    if !limited.buckets.is_empty() {
                        let data = serde_json::json!({ "buckets": limited.buckets }).to_string();
                        let now = chrono::Utc::now().timestamp();
                        upsert_rate_limit_cache_entry(pg_pool, "claude", &data, now).await;
                    }
                    // The loop logs 429s with backoff context (WARN, then INFO).
                    tracing::debug!("[rate-limit-sync] Claude rate_limit fetch failed: {e}");
                }
                // Other failures: one WARN shared with forced refreshes.
                None => tracing::warn!("[rate-limit-sync] Claude rate_limit fetch failed: {e}"),
            }
            Err(e)
        }
    }
}

/// How long the Claude fetch may stay held off, given when the still-deferring
/// telemetry was last observed. The gate re-Allows once that row outlives its
/// stale window, so the next attempt is pinned to a deadline off the
/// observation, not to `Retry-After`; two ticks of slack absorb a loop period
/// that is `base` *plus* the other providers' fetches. Past the deadline the
/// cap is zero (base cadence, the fail-safe); `None` leaves the hold alone.
fn observation_deadline_cap(
    observed_at: Option<i64>,
    now: i64,
    base: Duration,
    stale: Duration,
) -> Option<Duration> {
    let deadline = observed_at? + stale.as_secs() as i64 - 2 * base.as_secs() as i64;
    let left = deadline.saturating_sub(now).max(0) as u64;
    Some(Duration::from_secs(left))
}

/// Builds the typed 429 error, carrying any pressure buckets the response advertised.
fn claude_usage_rate_limited_error(
    headers: &reqwest::header::HeaderMap,
    buckets: Buckets,
) -> rate_limit_backoff::ClaudeUsageRateLimited {
    let retry_after = headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| rate_limit_backoff::parse_retry_after(value, chrono::Utc::now()));
    rate_limit_backoff::ClaudeUsageRateLimited {
        retry_after,
        buckets,
    }
}

/// Maps one `count_tokens` response onto buckets. A 429 keeps its telemetry in
/// the typed error; every other non-2xx is an error too, so the loop reads it
/// as `OtherError` (preserving a 429 streak), not an empty success.
fn anthropic_rate_limit_response(
    status: reqwest::StatusCode,
    headers: &reqwest::header::HeaderMap,
) -> Result<Buckets, anyhow::Error> {
    let mut buckets = Vec::new();
    for name in ["requests", "tokens"] {
        let key = |field| format!("anthropic-ratelimit-{name}-{field}");
        let Some(limit) = parse_header_i64(headers, &key("limit")) else {
            continue;
        };
        let remaining = parse_header_i64(headers, &key("remaining")).unwrap_or(limit);
        buckets.push(serde_json::json!({
            "name": name,
            "limit": limit,
            "used": limit - remaining,
            "remaining": remaining,
            "reset": parse_header_reset(headers, &key("reset")),
        }));
    }

    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        return Err(anyhow::Error::new(claude_usage_rate_limited_error(
            headers, buckets,
        )));
    }
    if !status.is_success() {
        return Err(anyhow::anyhow!("Anthropic count_tokens returned {status}"));
    }
    Ok(buckets)
}

/// Fetch rate limits via the Anthropic count_tokens endpoint (free, no tokens).
async fn fetch_anthropic_rate_limits(api_key: &str) -> Result<Buckets, anyhow::Error> {
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

    anthropic_rate_limit_response(resp.status(), resp.headers())
}

/// Fetch Claude usage via the OAuth API (subscription): 5h/7d utilization.
async fn fetch_claude_oauth_usage(token: &str) -> Result<Buckets, anyhow::Error> {
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
            Vec::new(),
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
    use super::super::rate_limit_backoff::{
        ClaudeSyncBackoff, ClaudeSyncOutcome, ClaudeUsageRateLimited,
    };
    use super::{
        anthropic_rate_limit_response, classify_claude_sync_result,
        claude_usage_rate_limited_error, observation_deadline_cap,
    };
    use crate::services::dispatch_gate as gate;
    use reqwest::{StatusCode, header::HeaderMap};
    use std::time::{Duration, Instant};

    fn secs(value: u64) -> Duration {
        Duration::from_secs(value)
    }

    fn rate_limited(retry_after: Option<Duration>) -> anyhow::Error {
        let buckets = Vec::new();
        anyhow::Error::new(ClaudeUsageRateLimited {
            retry_after,
            buckets,
        })
    }

    #[test]
    fn classifies_claude_sync_results_for_backoff() {
        let classify = |result| classify_claude_sync_result(&result);
        let after = |retry_after| ClaudeSyncOutcome::RateLimited { retry_after };
        assert_eq!(classify(Ok(2)), ClaudeSyncOutcome::Success);
        assert_eq!(
            classify(Err(anyhow::anyhow!("no Claude credentials found"))),
            ClaudeSyncOutcome::OtherError
        );
        let held = secs(90);
        assert_eq!(classify(Err(rate_limited(Some(held)))), after(Some(held)));
        // Context wrapping must not hide the typed 429.
        let wrapped = rate_limited(None).context("forced refresh");
        assert_eq!(classify(Err(wrapped)), after(None));
    }

    #[test]
    fn api_key_429_keeps_pressure_buckets_and_the_backoff_streak() {
        // An exhausted requests bucket, with no readable reset.
        let mut headers = HeaderMap::new();
        headers.insert("anthropic-ratelimit-requests-limit", "100".parse().unwrap());
        let zero = "0".parse().expect("valid header value");
        headers.insert("anthropic-ratelimit-requests-remaining", zero);
        headers.insert(reqwest::header::RETRY_AFTER, "600".parse().unwrap());
        // The exhaustion telemetry rides along inside the typed error.
        let error = anthropic_rate_limit_response(StatusCode::TOO_MANY_REQUESTS, &headers)
            .expect_err("a 429 must still schedule a retry");
        let limited = error
            .downcast_ref::<ClaudeUsageRateLimited>()
            .expect("429 is the typed rate-limit error");
        assert_eq!(limited.buckets.len(), 1);
        assert_eq!(limited.buckets[0]["used"], 100);
        assert_eq!(limited.buckets[0]["remaining"], 0);
        // A 429 without the header leaves the delay to the ladder.
        assert_eq!(limited.retry_after, Some(secs(600)));
        let no_header = claude_usage_rate_limited_error(&HeaderMap::new(), Vec::new());
        assert_eq!(no_header.retry_after, None);

        // 429 -> 500 -> 429: the 500 must not read as a successful sync (which
        // would reset the ladder), so the last 429 has to resume at 240 s.
        headers.remove(reqwest::header::RETRY_AFTER);
        let mut backoff = ClaudeSyncBackoff::new(secs(120), secs(1800));
        let t0 = Instant::now();
        for (status, at, delay) in [
            (StatusCode::TOO_MANY_REQUESTS, 0, 120),
            (StatusCode::INTERNAL_SERVER_ERROR, 120, 120),
            (StatusCode::TOO_MANY_REQUESTS, 240, 240),
        ] {
            let synced = anthropic_rate_limit_response(status, &headers).map(|b| b.len());
            let recorded = backoff.record(classify_claude_sync_result(&synced), t0 + secs(at));
            assert_eq!(recorded, secs(delay), "{status} at {at}s");
        }
        assert_eq!(backoff.consecutive_rate_limits(), 2);
    }

    /// One cached Claude row, as the gate parses it (`used` is the percent).
    fn cached(used: i64, reset: i64, fetched_at: i64) -> gate::ProviderPressureSnapshot {
        let payload = serde_json::json!({
            "provider": "claude",
            "buckets": [{"name": "5h", "limit": 100, "used": used, "reset": reset}],
            "fetched_at": fetched_at,
        });
        let parsed = gate::snapshot_from_provider_payload(&payload);
        parsed.expect("claude row").1
    }

    /// r4 regression: while the cached row still defers, the next attempt is
    /// pinned to an observation deadline, so the pressure cannot age out of the
    /// gate's window and fail open. With no pressure the long hold stands.
    #[test]
    fn deferring_telemetry_pins_the_next_attempt_to_an_observation_deadline() {
        let (base, stale, now) = (secs(120), secs(600), 1_000_000_i64);
        let cap = |snapshot: &gate::ProviderPressureSnapshot, at| {
            let observed = gate::deferring_observation_of("claude", Some(snapshot), at);
            observation_deadline_cap(observed, at, base, stale)
        };
        let limited = |after: Option<u64>| ClaudeSyncOutcome::RateLimited {
            retry_after: after.map(secs),
        };
        // No pressure, and exhaustion whose window is provably over, leave the
        // backoff alone; an unreadable reset defers, so it is capped.
        assert_eq!(cap(&cached(40, now + 3600, now), now), None);
        assert_eq!(cap(&cached(100, now - 1, now), now), None);
        assert_eq!(cap(&cached(100, 0, now), now), Some(secs(360)));

        // Fetch schedule under a 429 streak, as (row age, first `Retry-After`,
        // loop wake-ups, ticks that must fetch). Wake-ups are 159 s apart —
        // the loop sleeps `base` *after* the other providers — yet the
        // re-observation must beat the 600 s window; an OAuth 429 caches
        // nothing, so its deadline stays at the last success.
        let ladder = vec![0_i64, 120, 240, 360, 480];
        for (fetched_at, first, ticks, want) in [
            (now, Some(1800_u64), vec![159_i64, 318, 477], vec![477_i64]),
            (now - 120, None, ladder.clone(), ladder.clone()),
        ] {
            let snapshot = cached(100, now + 3600, fetched_at);
            let mut backoff = ClaudeSyncBackoff::new(base, secs(1800));
            let t0 = Instant::now();
            if let Some(after) = first {
                assert_eq!(backoff.record(limited(Some(after)), t0), secs(after));
            }
            let mut fired = Vec::new();
            for tick in ticks {
                let at = t0 + secs(tick as u64);
                if let Some(shorter) = cap(&snapshot, now + tick) {
                    backoff.cap_hold(shorter, at);
                }
                if backoff.should_attempt(at) {
                    fired.push(tick);
                    backoff.record(limited(None), at);
                }
            }
            assert_eq!(fired, want, "row fetched at {fetched_at}");
        }
    }
}
