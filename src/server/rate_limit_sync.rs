//! Periodic provider rate-limit sync (`rate_limit_sync_loop`) and the Claude leg's fetch/backoff
//! wiring. Slice A of #5727 moved these bodies here verbatim; this slice adds the 429 backoff
//! ([`self::backoff`]) and the pressure floor that keeps the base cadence while the
//! dispatch gate still defers on Claude's cached telemetry.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use sqlx::PgPool;

use crate::services::dispatch_gate;

pub(crate) mod backoff;
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
) -> backoff::ClaudeSyncOutcome {
    use backoff::{ClaudeSyncOutcome, ClaudeUsageRateLimited};
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

/// One tick's resolver → pressure → hold decision, shared by the loop and its regression test.
async fn claude_tick_should_attempt(
    backoff: &mut backoff::ClaudeSyncBackoff,
    now: std::time::Instant,
    pg_pool: &PgPool,
    pressure: impl FnOnce(Option<u64>) -> bool,
) -> bool {
    let danger = dispatch_gate::effective_danger_pct_pg(pg_pool).await;
    if pressure(danger) {
        backoff.release_hold();
    }
    backoff.should_attempt(now)
}

pub(super) async fn rate_limit_sync_loop(pg_pool: Arc<PgPool>) {
    use backoff::{
        ClaudeSyncBackoff, ClaudeSyncOutcome, RATE_LIMIT_SYNC_BASE_INTERVAL,
        RATE_LIMIT_SYNC_MAX_BACKOFF,
    };
    use std::time::Instant;

    let interval = RATE_LIMIT_SYNC_BASE_INTERVAL;
    // Immediately on startup, then every 2 minutes; only the Claude leg backs
    // off after 429s.
    let mut first = true;
    let mut claude_backoff = ClaudeSyncBackoff::new(interval, RATE_LIMIT_SYNC_MAX_BACKOFF);

    loop {
        if !first {
            tokio::time::sleep(interval).await;
        }
        first = false;

        let now = Instant::now();
        // #5727: while the gate still defers on Claude's cached telemetry, keep the base cadence
        // — nothing bounds one iteration, so no longer hold is *provably* short enough to
        // re-observe that pressure before the stale window expires, and base is the pre-PR floor.
        let now_unix = chrono::Utc::now().timestamp();
        if claude_tick_should_attempt(&mut claude_backoff, now, pg_pool.as_ref(), |danger| {
            dispatch_gate::is_deferring("claude", danger, now_unix)
        })
        .await
        {
            let claude_result =
                sync_claude_rate_limit_cache_once_serialized(pg_pool.as_ref()).await;
            let outcome = classify_claude_sync_result(&claude_result);
            let is_rate_limited = matches!(outcome, ClaudeSyncOutcome::RateLimited { .. });
            let at = Instant::now();
            let delay = claude_backoff.record(outcome, at);
            if is_rate_limited {
                // First 429 of a streak is WARN, the rest INFO.
                let consecutive_429 = claude_backoff.consecutive_rate_limits();
                let backoff_secs = delay.as_secs();
                if consecutive_429 <= 1 {
                    tracing::warn!(
                        backoff_secs,
                        "[rate-limit-sync] Claude rate_limit fetch rate limited (429); backing off"
                    );
                } else {
                    tracing::info!(
                        backoff_secs,
                        consecutive_429,
                        "[rate-limit-sync] Claude still rate limited (429); backing off"
                    );
                }
            }
        } else {
            tracing::debug!(
                remaining_secs = claude_backoff.remaining(now).as_secs(),
                "[rate-limit-sync] Claude fetch skipped: 429 backoff in effect"
            );
        }

        // --- Codex: ~/.codex/auth.json (CLI subscription), else OPENAI_API_KEY ---
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
                // Only the genuine "not configured / file missing" case is suppressed, classified
                // at the source by `io::ErrorKind`; matching on "oauth_creds.json" here would
                // also swallow permission, I/O and corrupt creds (#3566 over-suppress fix).
                let creds_missing =
                    crate::services::provider_auth::is_gemini_unconfigured_error(&e);
                if creds_missing {
                    // Log once, then DEBUG so the loop doesn't spam it (#3566).
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

        // feature: rate-limit-aware-dispatch-gate — refresh the pressure +
        // agent→provider snapshots the gate reads O(1) off the dispatch path.
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
            // Telemetry is independent of retry scheduling: a 429 carrying limit headers is
            // cached anyway, so the gate sees the exhaustion. A 429 with no buckets (OAuth)
            // writes nothing.
            match e.downcast_ref::<backoff::ClaudeUsageRateLimited>() {
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

/// Builds the typed 429 error, carrying any pressure buckets the response advertised.
fn claude_usage_rate_limited_error(
    headers: &reqwest::header::HeaderMap,
    buckets: Buckets,
) -> backoff::ClaudeUsageRateLimited {
    let retry_after = headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| backoff::parse_retry_after(value, chrono::Utc::now()));
    backoff::ClaudeUsageRateLimited {
        retry_after,
        buckets,
    }
}

/// Maps one `count_tokens` response onto buckets. A 429 keeps its telemetry in the typed error;
/// every other non-2xx is an error too, so the loop reads it as `OtherError` (preserving a 429
/// streak), not an empty success.
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
    use super::backoff::{ClaudeSyncBackoff, ClaudeSyncOutcome, ClaudeUsageRateLimited};
    use super::{
        anthropic_rate_limit_response, classify_claude_sync_result, claude_tick_should_attempt,
        claude_usage_rate_limited_error,
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
        // An exhausted requests bucket, with no readable reset. Its telemetry
        // has to ride along inside the typed error.
        let mut headers = HeaderMap::new();
        headers.insert("anthropic-ratelimit-requests-limit", "100".parse().unwrap());
        let zero = "0".parse().expect("valid header value");
        headers.insert("anthropic-ratelimit-requests-remaining", zero);
        headers.insert(reqwest::header::RETRY_AFTER, "600".parse().unwrap());
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

    /// r5 regression: while the cached row still defers dispatch, no tick spacing may push the
    /// next Claude attempt past the base cadence — an OAuth 429 streak included, since it caches
    /// nothing and can only be re-observed by trying. With no pressure the 30-minute ladder
    /// stands, and an unreadable runtime-config counts as pressure.
    #[test]
    fn pressure_keeps_the_base_cadence_whatever_the_tick_spacing() {
        let now = 1_000_000_i64;
        let limited = |after: Option<u64>| ClaudeSyncOutcome::RateLimited {
            retry_after: after.map(secs),
        };
        let defers = |row: &gate::ProviderPressureSnapshot, danger, at| {
            gate::is_deferring_snapshot("claude", Some(row), danger, at)
        };
        // Wake-ups are irregular (the loop sleeps `base` *after* the other providers' fetches),
        // so 310 and 620 outrun any fixed slack; the OAuth row caches nothing, so it ages past
        // the 600 s window and still must be re-observed. Row 2's wake-ups sit *off* the
        // 120/240/480 ladder on purpose — 300 and 360 fall inside the hold a surviving ladder
        // would impose, so they fire only because the floor drops it. (row, `Retry-After`,
        // wake-ups — all of which must fetch)
        for (row, first, ticks) in [
            (
                cached(100, now + 3600, now),
                Some(1800_u64),
                vec![120_i64, 159, 310, 620],
            ),
            (
                cached(100, now + 3600, now - 300),
                None,
                vec![120, 240, 300, 360],
            ),
        ] {
            let mut backoff = ClaudeSyncBackoff::new(secs(120), secs(1800));
            let t0 = Instant::now();
            if let Some(after) = first {
                assert_eq!(backoff.record(limited(Some(after)), t0), secs(after));
            }
            let mut fired = Vec::new();
            for tick in &ticks {
                let at = t0 + secs(*tick as u64);
                if defers(&row, Some(100), now + tick) {
                    backoff.release_hold();
                }
                if backoff.should_attempt(at) {
                    fired.push(*tick);
                    backoff.record(limited(None), at);
                }
            }
            assert_eq!(fired, ticks, "row fetched at {}", row.fetched_at);
        }

        // No pressure: the 30-minute hold stands. An unreadable config does not.
        let calm = cached(40, now + 3600, now);
        let mut backoff = ClaudeSyncBackoff::new(secs(120), secs(1800));
        let t0 = Instant::now();
        assert_eq!(backoff.record(limited(Some(1800)), t0), secs(1800));
        for tick in [120_i64, 1799] {
            assert!(!defers(&calm, Some(100), now + tick));
            assert!(!backoff.should_attempt(t0 + secs(tick as u64)));
        }
        assert!(defers(&calm, None, now));
        assert!(backoff.should_attempt(t0 + secs(1800)));
    }

    /// r5 follow-up: the threshold the loop feeds the predicate is the gate's
    /// EFFECTIVE one — the persisted runtime-config the activation path
    /// resolves, not the YAML accessor — and the persisted staleness window is
    /// deliberately not one of the predicate's inputs.
    #[tokio::test]
    async fn effective_config_comes_from_the_persisted_runtime_overrides() {
        let now = 1_000_000_i64;
        let persisted = |raw: &str| {
            let value: serde_json::Value = serde_json::from_str(raw).expect("runtime-config");
            gate::persisted_runtime_overrides(Some(&value))
        };
        let defers = |row: &gate::ProviderPressureSnapshot, danger| {
            gate::is_deferring_snapshot("claude", Some(row), danger, now)
        };
        // Persisting danger 95 makes 97 % pressure; the YAML default of 100
        // does not, so reading the persisted row is what keeps the floor alive.
        let (_enabled, danger, stale) =
            persisted(r#"{"dispatchRateLimitGateDangerPct": 95, "rateLimitStaleSec": 300}"#);
        assert_eq!((danger, stale), (Some(95), Some(300)));
        let hot = cached(97, now + 3600, now);
        assert!(defers(&hot, danger));
        assert!(!defers(&hot, Some(100)));
        let t0 = Instant::now();
        let mut backoff = ClaudeSyncBackoff::new(secs(120), secs(1800));
        backoff.record(
            ClaudeSyncOutcome::RateLimited {
                retry_after: Some(secs(1800)),
            },
            t0,
        );
        let unusable = sqlx::postgres::PgPoolOptions::new()
            .acquire_timeout(secs(2))
            .connect_lazy("postgres://agentdesk:agentdesk@127.0.0.1:1/agentdesk")
            .expect("a lazy pool never dials on construction");
        // The real resolver must fail to None, not supply the YAML threshold.
        assert!(
            claude_tick_should_attempt(&mut backoff, t0 + secs(120), &unusable, |threshold| {
                assert_eq!(threshold, None, "tick must read the persisted config");
                defers(&hot, threshold)
            })
            .await
        );
        // The staleness window comes back from the same parser but is not an
        // input: a row older than 300 s — or than the 600 s default — still has
        // to be re-observed, so it must not switch the base cadence off.
        assert!(defers(&cached(100, now + 3600, now - 900), Some(100)));
    }

    /// An unreadable runtime-config yields None, which polling treats as pressure.
    #[tokio::test]
    async fn unreadable_runtime_config_resolves_to_no_threshold() {
        let unusable = sqlx::postgres::PgPoolOptions::new()
            .acquire_timeout(secs(2))
            .connect_lazy("postgres://agentdesk:agentdesk@127.0.0.1:1/agentdesk")
            .expect("a lazy pool never dials on construction");
        // The YAML accessor always answers, so `None` here can only have come from the persisted
        // read failing — and it is not what `danger_pct()` would have returned, whatever the
        // process's YAML snapshot happens to hold.
        let resolved = gate::effective_danger_pct_pg(&unusable).await;
        assert_eq!(resolved, None);
        // And `None` is pressure, so the loop keeps the base cadence rather than failing open.
        assert!(gate::is_deferring_snapshot(
            "claude",
            Some(&cached(40, 1_000_000 + 3600, 1_000_000)),
            None,
            1_000_000,
        ));
    }
}
