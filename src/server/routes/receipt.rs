use axum::{
    Json,
    extract::State,
    http::{HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use chrono::{Datelike, Local, TimeZone};
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
use std::sync::{
    Arc, Mutex, OnceLock,
    atomic::{AtomicU64, Ordering},
};
use std::time::{Duration as StdDuration, Instant};
use tokio::sync::Mutex as AsyncMutex;

use super::AppState;
use crate::error::{AppError, AppResult};
use crate::receipt;

#[derive(Debug, Deserialize)]
pub struct ReceiptQuery {
    /// Period: "today", "week", "month", "ratelimit", or "all"
    period: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct TokenAnalyticsQuery {
    /// Period: "7d", "30d", or "90d"
    period: Option<String>,
    /// When true, bypass the in-process cache and re-scan disk. Set by the
    /// dashboard's explicit Refresh button so manual refreshes always see
    /// the freshest possible numbers. Accepts `fresh=1` as emitted by the
    /// dashboard Refresh button, plus common boolean spellings.
    fresh: Option<String>,
}

/// In-process cache for the heavy token-analytics computation. Each call to
/// `collect_token_analytics` does a multi-hundred-MB filesystem scan across
/// `~/.claude/projects` + `~/.codex/sessions`, which takes ~1-9 s depending
/// on workspace age. The HTTP-level SWR cache (Cache-Control: max-age=15)
/// already absorbs same-tab re-entry, but cross-tab / new-session loads still
/// hit the origin and pay the full cost.
///
/// 30 s TTL is short enough that "live" data is still close-to-current
/// (token usage doesn't materially shift in 30 s) and long enough to cover
/// the typical "open dashboard, navigate around, settle on /stats" pattern.
/// The cache holds at most 3 entries (7d / 30d / 90d), so no LRU eviction
/// is needed.
const TOKEN_ANALYTICS_CACHE_TTL: StdDuration = StdDuration::from_secs(30);

struct CachedAnalytics {
    cached_at: Instant,
    refresh_generation: u64,
    data: Arc<receipt::TokenAnalyticsData>,
}

static TOKEN_ANALYTICS_CACHE: OnceLock<Mutex<HashMap<String, CachedAnalytics>>> = OnceLock::new();
static TOKEN_ANALYTICS_REFRESH_GENERATION: AtomicU64 = AtomicU64::new(0);

fn token_analytics_cache() -> &'static Mutex<HashMap<String, CachedAnalytics>> {
    TOKEN_ANALYTICS_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

struct TokenAnalyticsCollectLocks {
    seven_day: AsyncMutex<()>,
    thirty_day: AsyncMutex<()>,
    ninety_day: AsyncMutex<()>,
}

static TOKEN_ANALYTICS_COLLECT_LOCKS: OnceLock<TokenAnalyticsCollectLocks> = OnceLock::new();

fn token_analytics_collect_locks() -> &'static TokenAnalyticsCollectLocks {
    TOKEN_ANALYTICS_COLLECT_LOCKS.get_or_init(|| TokenAnalyticsCollectLocks {
        seven_day: AsyncMutex::new(()),
        thirty_day: AsyncMutex::new(()),
        ninety_day: AsyncMutex::new(()),
    })
}

fn token_analytics_collect_lock(period: &str) -> &'static AsyncMutex<()> {
    let locks = token_analytics_collect_locks();
    match period {
        "7d" => &locks.seven_day,
        "90d" => &locks.ninety_day,
        _ => &locks.thirty_day,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TokenAnalyticsCacheState {
    Hit,
    Miss,
    Bypass,
}

impl TokenAnalyticsCacheState {
    fn as_header_value(self) -> &'static str {
        match self {
            Self::Hit => "hit",
            Self::Miss => "miss",
            Self::Bypass => "bypass",
        }
    }
}

pub(crate) struct TokenAnalyticsCacheResult {
    pub(crate) data: Arc<receipt::TokenAnalyticsData>,
    pub(crate) cache_state: TokenAnalyticsCacheState,
}

fn read_cached_token_analytics(period: &str) -> Option<Arc<receipt::TokenAnalyticsData>> {
    let cache = token_analytics_cache().lock().ok()?;
    let entry = cache.get(period)?;
    if entry.cached_at.elapsed() > TOKEN_ANALYTICS_CACHE_TTL {
        return None;
    }
    Some(Arc::clone(&entry.data))
}

fn cached_token_analytics_refresh_generation(period: &str) -> u64 {
    token_analytics_cache()
        .lock()
        .ok()
        .and_then(|cache| cache.get(period).map(|entry| entry.refresh_generation))
        .unwrap_or(0)
}

fn read_cached_token_analytics_refreshed_after(
    period: &str,
    refresh_generation: u64,
) -> Option<Arc<receipt::TokenAnalyticsData>> {
    let cache = token_analytics_cache().lock().ok()?;
    let entry = cache.get(period)?;
    if entry.refresh_generation <= refresh_generation
        || entry.cached_at.elapsed() > TOKEN_ANALYTICS_CACHE_TTL
    {
        return None;
    }
    Some(Arc::clone(&entry.data))
}

fn write_cached_token_analytics(
    period: &str,
    data: Arc<receipt::TokenAnalyticsData>,
    refreshed: bool,
) {
    if let Ok(mut cache) = token_analytics_cache().lock() {
        let refresh_generation = if refreshed {
            TOKEN_ANALYTICS_REFRESH_GENERATION.fetch_add(1, Ordering::Relaxed) + 1
        } else {
            0
        };
        cache.insert(
            period.to_string(),
            CachedAnalytics {
                cached_at: Instant::now(),
                refresh_generation,
                data,
            },
        );
    }
}

/// Shared cache-or-compute helper for the token-analytics filesystem scan.
/// Used by both `/api/token-analytics` and `/api/home/kpi-trends` so the
/// dashboard's first paint pays at most one ~9 s scan per period instead of
/// one per endpoint (#1303).
///
/// Cache misses are single-flighted per canonical period. If `/api/home/kpi-trends`
/// and `/api/token-analytics` miss concurrently, one request performs the scan
/// while the other waits, rechecks the cache, and reuses the result. The first
/// explicit `fresh=1` request also scans, but concurrent refreshes for the same
/// period coalesce behind the period lock and reuse the new cache write.
///
/// Returns `None` only when the blocking task panics; the caller is expected
/// to surface a graceful fallback (empty arrays + warn log) rather than 5xx.
pub(crate) async fn cached_or_collect_token_analytics(
    period_id: &str,
    days: i64,
    label: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<TokenAnalyticsCacheResult> {
    if let Some(cached) = read_cached_token_analytics(period_id) {
        return Some(TokenAnalyticsCacheResult {
            data: cached,
            cache_state: TokenAnalyticsCacheState::Hit,
        });
    }
    collect_token_analytics_with_lock(period_id, days, label, now, false).await
}

async fn collect_token_analytics_with_lock(
    period_id: &str,
    days: i64,
    label: &str,
    now: chrono::DateTime<chrono::Utc>,
    bypass_cache: bool,
) -> Option<TokenAnalyticsCacheResult> {
    collect_token_analytics_with_lock_using(
        period_id,
        days,
        label,
        now,
        bypass_cache,
        |start, now, label_owned, period_owned| {
            receipt::collect_token_analytics(start, now, &label_owned, &period_owned)
        },
    )
    .await
}

async fn collect_token_analytics_with_lock_using<Collect>(
    period_id: &str,
    days: i64,
    label: &str,
    now: chrono::DateTime<chrono::Utc>,
    bypass_cache: bool,
    collect: Collect,
) -> Option<TokenAnalyticsCacheResult>
where
    Collect: FnOnce(
            chrono::DateTime<chrono::Utc>,
            chrono::DateTime<chrono::Utc>,
            String,
            String,
        ) -> receipt::TokenAnalyticsData
        + Send
        + 'static,
{
    let refresh_generation_before_wait = if bypass_cache {
        cached_token_analytics_refresh_generation(period_id)
    } else {
        0
    };
    let _guard = token_analytics_collect_lock(period_id).lock().await;

    let cached = if bypass_cache {
        read_cached_token_analytics_refreshed_after(period_id, refresh_generation_before_wait)
    } else {
        read_cached_token_analytics(period_id)
    };
    if let Some(cached) = cached {
        return Some(TokenAnalyticsCacheResult {
            data: cached,
            cache_state: if bypass_cache {
                TokenAnalyticsCacheState::Bypass
            } else {
                TokenAnalyticsCacheState::Hit
            },
        });
    }

    let local_now = now.with_timezone(&Local);
    let start_date = local_now.date_naive() - chrono::Duration::days(days.saturating_sub(1));
    let start = Local
        .from_local_datetime(&start_date.and_hms_opt(0, 0, 0).unwrap())
        .single()
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .unwrap_or_else(|| now - chrono::Duration::days(days));
    let label_owned = label.to_string();
    let period_owned = period_id.to_string();
    let data =
        match tokio::task::spawn_blocking(move || collect(start, now, label_owned, period_owned))
            .await
        {
            Ok(data) => Arc::new(data),
            Err(error) => {
                tracing::warn!(period = period_id, error = %error, "token-analytics scan failed");
                return None;
            }
        };
    write_cached_token_analytics(period_id, Arc::clone(&data), bypass_cache);
    Some(TokenAnalyticsCacheResult {
        data,
        cache_state: if bypass_cache {
            TokenAnalyticsCacheState::Bypass
        } else {
            TokenAnalyticsCacheState::Miss
        },
    })
}

/// Spawn a detached post-boot prewarm for the token-analytics in-process cache.
/// This is intentionally best-effort: the server starts accepting requests
/// without waiting for filesystem scans, and the single-flight cache path below
/// still protects first requests if they beat the prewarm.
pub(crate) fn spawn_token_analytics_cache_prewarm() {
    tokio::spawn(async {
        tokio::time::sleep(StdDuration::from_secs(2)).await;
        prewarm_token_analytics_cache().await;
    });
}

/// Pre-warm the token-analytics in-process cache for every period the
/// dashboard requests on first paint. The first home/stats visit on a fresh
/// dcserver previously paid the ~9s filesystem scan synchronously while the
/// user watched a placeholder; the detached boot hook now calls this shortly
/// after boot, and concurrent first requests share the same single-flight
/// collector if they arrive before the prewarm completes.
///
/// We deliberately stagger periods so the three blocking scans don't pile up
/// onto the same blocking pool slot at the same moment, and we tolerate
/// individual failures so a transient parse error in one period doesn't
/// kill the prewarm for the others.
pub async fn prewarm_token_analytics_cache() {
    for period in ["7d", "30d", "90d"] {
        let (days, label) = match period {
            "7d" => (7_i64, "Last 7 Days"),
            "90d" => (90_i64, "Last 90 Days"),
            _ => (30_i64, "Last 30 Days"),
        };
        let now = chrono::Utc::now();
        let started = Instant::now();
        match cached_or_collect_token_analytics(period, days, label, now).await {
            Some(result) => {
                tracing::info!(
                    period,
                    elapsed_ms = started.elapsed().as_millis(),
                    cache_state = result.cache_state.as_header_value(),
                    "token-analytics prewarm done"
                );
            }
            None => {
                tracing::warn!(period, "token-analytics prewarm failed");
                continue;
            }
        }
        tokio::time::sleep(StdDuration::from_millis(250)).await;
    }
}

/// GET /api/receipt?period=month
pub async fn get_receipt(
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<ReceiptQuery>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let period = params.period.as_deref().unwrap_or("month");
    let now = chrono::Utc::now();
    let local_now = now.with_timezone(&Local);

    let (start, label) = match period {
        "today" => {
            // Local midnight (not UTC) so "Today" matches the user's calendar day.
            let local_midnight = Local
                .with_ymd_and_hms(
                    local_now.year(),
                    local_now.month(),
                    local_now.day(),
                    0,
                    0,
                    0,
                )
                .single()
                .map(|dt| dt.with_timezone(&chrono::Utc))
                .unwrap_or_else(|| now - chrono::Duration::hours(24));
            (local_midnight, "Today")
        }
        "week" => {
            // Calendar week: Monday 00:00 local time.
            let days_since_mon = local_now.weekday().num_days_from_monday();
            let monday = local_now.date_naive() - chrono::Duration::days(days_since_mon as i64);
            let local_monday = Local
                .from_local_datetime(&monday.and_hms_opt(0, 0, 0).unwrap())
                .single()
                .map(|dt| dt.with_timezone(&chrono::Utc))
                .unwrap_or_else(|| now - chrono::Duration::days(7));
            (local_monday, "This Week")
        }
        "month" => {
            // Calendar month: 1st day 00:00 local time.
            let first = Local
                .with_ymd_and_hms(local_now.year(), local_now.month(), 1, 0, 0, 0)
                .single()
                .map(|dt| dt.with_timezone(&chrono::Utc))
                .unwrap_or_else(|| now - chrono::Duration::days(30));
            (first, "This Month")
        }
        "ratelimit" => {
            let ws = match state.pg_pool.as_ref() {
                Some(pool) => receipt::ratelimit_window_start_pg(pool).await,
                None => None,
            };
            (
                ws.unwrap_or_else(|| now - chrono::Duration::days(7)),
                "Rate Limit Window",
            )
        }
        "all" => (
            chrono::DateTime::from_timestamp(0, 0).unwrap_or(now - chrono::Duration::days(3650)),
            "All Time",
        ),
        _ => (now - chrono::Duration::days(30), "Last 30 Days"),
    };

    let label_owned = label.to_string();
    let data = match tokio::task::spawn_blocking(move || receipt::collect(start, now, &label_owned))
        .await
    {
        Ok(d) => d,
        Err(e) => {
            return Err(AppError::internal(format!("collection failed: {e}")));
        }
    };

    Ok((StatusCode::OK, Json(json!(data))))
}

/// GET /api/token-analytics?period=30d&fresh=1
pub async fn get_token_analytics(
    State(_state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<TokenAnalyticsQuery>,
) -> Response {
    let started = Instant::now();
    let period = params.period.as_deref().unwrap_or("30d");
    let bypass_cache = parse_token_analytics_fresh_param(params.fresh.as_deref());
    let now = chrono::Utc::now();

    let (days, label, period_id) = match period {
        "7d" => (7_i64, "Last 7 Days", "7d"),
        "90d" => (90_i64, "Last 90 Days", "90d"),
        _ => (30_i64, "Last 30 Days", "30d"),
    };

    let result = match if bypass_cache {
        collect_token_analytics_with_lock(period_id, days, label, now, true).await
    } else {
        cached_or_collect_token_analytics(period_id, days, label, now).await
    } {
        Some(result) => result,
        None => {
            let elapsed_ms = started.elapsed().as_millis();
            tracing::warn!(period = period_id, elapsed_ms, "token-analytics failed");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "collection failed"})),
            )
                .into_response();
        }
    };

    let elapsed_ms = started.elapsed().as_millis();
    tracing::info!(
        period = period_id,
        elapsed_ms,
        cache_state = result.cache_state.as_header_value(),
        "token-analytics responded"
    );

    build_token_analytics_response(
        &result.data,
        period_id,
        elapsed_ms,
        result.cache_state.as_header_value(),
    )
}

fn build_token_analytics_response(
    data: &receipt::TokenAnalyticsData,
    period_id: &str,
    elapsed_ms: u128,
    cache_state: &'static str,
) -> Response {
    let mut response = (StatusCode::OK, Json(json!(data))).into_response();
    let headers = response.headers_mut();
    // Browser-side SWR window: served instantly within 15 s, falls back to
    // stale-while-revalidate for 5 min so cross-page navigation paints
    // immediately while the in-process cache below absorbs the actual
    // origin work. The frontend explicit Refresh button passes `?fresh=1`
    // (and `cache: "reload"`) so the response also bypasses both layers.
    headers.insert(
        "Cache-Control",
        HeaderValue::from_static("private, max-age=15, stale-while-revalidate=300"),
    );
    if let Ok(value) = HeaderValue::from_str(period_id) {
        // Vary on the period query parameter so the 7d / 30d / 90d entries
        // never collide in the cache.
        headers.insert("X-Token-Analytics-Period", value);
    }
    if let Ok(value) = HeaderValue::from_str(&elapsed_ms.to_string()) {
        headers.insert("X-Response-Time-Ms", value);
    }
    headers.insert(
        "X-Token-Analytics-Cache",
        HeaderValue::from_static(cache_state),
    );
    response
}

fn parse_token_analytics_fresh_param(value: Option<&str>) -> bool {
    matches!(
        value
            .map(str::trim)
            .map(|value| value.to_ascii_lowercase())
            .as_deref(),
        Some("1" | "true" | "yes" | "y" | "on")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::receipt::{ReceiptData, ReceiptStats, TokenAnalyticsData, TokenAnalyticsSummary};
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static TEST_PERIOD_SUFFIX: AtomicUsize = AtomicUsize::new(0);

    fn empty_receipt_data(label: &str) -> ReceiptData {
        ReceiptData {
            period_label: label.to_string(),
            period_start: "2026-01-01T00:00:00Z".to_string(),
            period_end: "2026-01-01T00:00:00Z".to_string(),
            models: Vec::new(),
            subtotal: 0.0,
            cache_discount: 0.0,
            total: 0.0,
            stats: ReceiptStats {
                total_messages: 0,
                total_sessions: 0,
                per_provider: HashMap::new(),
                per_provider_agents: HashMap::new(),
            },
            providers: Vec::new(),
            agents: Vec::new(),
        }
    }

    fn empty_token_analytics_data(period: &str) -> TokenAnalyticsData {
        TokenAnalyticsData {
            period: period.to_string(),
            period_label: "Test Period".to_string(),
            days: 1,
            generated_at: "2026-01-01T00:00:00Z".to_string(),
            summary: TokenAnalyticsSummary {
                total_tokens: 0,
                total_cost: 0.0,
                cache_discount: 0.0,
                total_messages: 0,
                total_sessions: 0,
                active_days: 0,
                average_daily_tokens: 0,
                peak_day: None,
            },
            receipt: empty_receipt_data("Test Period"),
            daily: Vec::new(),
            heatmap: Vec::new(),
        }
    }

    #[test]
    fn token_analytics_cache_state_header_values_are_stable() {
        assert_eq!(TokenAnalyticsCacheState::Hit.as_header_value(), "hit");
        assert_eq!(TokenAnalyticsCacheState::Miss.as_header_value(), "miss");
        assert_eq!(TokenAnalyticsCacheState::Bypass.as_header_value(), "bypass");
    }

    #[test]
    fn token_analytics_fresh_param_accepts_dashboard_refresh_shape() {
        assert!(parse_token_analytics_fresh_param(Some("1")));
        assert!(parse_token_analytics_fresh_param(Some("true")));
        assert!(parse_token_analytics_fresh_param(Some("YES")));
        assert!(!parse_token_analytics_fresh_param(None));
        assert!(!parse_token_analytics_fresh_param(Some("0")));
        assert!(!parse_token_analytics_fresh_param(Some("false")));
    }

    #[test]
    fn token_analytics_response_sets_cache_and_period_headers() {
        let data = empty_token_analytics_data("30d");
        let response = build_token_analytics_response(
            &data,
            "30d",
            123,
            TokenAnalyticsCacheState::Hit.as_header_value(),
        );

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get("X-Token-Analytics-Period")
                .and_then(|value| value.to_str().ok()),
            Some("30d")
        );
        assert_eq!(
            response
                .headers()
                .get("X-Response-Time-Ms")
                .and_then(|value| value.to_str().ok()),
            Some("123")
        );
        assert_eq!(
            response
                .headers()
                .get("X-Token-Analytics-Cache")
                .and_then(|value| value.to_str().ok()),
            Some("hit")
        );
    }

    #[tokio::test]
    async fn concurrent_token_analytics_misses_are_single_flighted() {
        let suffix = TEST_PERIOD_SUFFIX.fetch_add(1, Ordering::SeqCst);
        let period = format!("test-single-flight-{suffix}");
        let collect_count = Arc::new(AtomicUsize::new(0));
        let now = chrono::Utc::now();

        let first_count = Arc::clone(&collect_count);
        let first = collect_token_analytics_with_lock_using(
            &period,
            1,
            "Test Period",
            now,
            false,
            move |_start, _now, _label, period| {
                first_count.fetch_add(1, Ordering::SeqCst);
                std::thread::sleep(StdDuration::from_millis(50));
                empty_token_analytics_data(&period)
            },
        );

        let second_count = Arc::clone(&collect_count);
        let second = collect_token_analytics_with_lock_using(
            &period,
            1,
            "Test Period",
            now,
            false,
            move |_start, _now, _label, period| {
                second_count.fetch_add(1, Ordering::SeqCst);
                empty_token_analytics_data(&period)
            },
        );

        let (first_result, second_result) = tokio::join!(first, second);

        assert_eq!(
            collect_count.load(Ordering::SeqCst),
            1,
            "the second concurrent miss should reuse the first collector's cache write"
        );
        let first_state = first_result.expect("first result").cache_state;
        let second_state = second_result.expect("second result").cache_state;
        assert!(
            matches!(first_state, TokenAnalyticsCacheState::Miss)
                || matches!(second_state, TokenAnalyticsCacheState::Miss)
        );
        assert!(
            matches!(first_state, TokenAnalyticsCacheState::Hit)
                || matches!(second_state, TokenAnalyticsCacheState::Hit)
        );
    }

    #[tokio::test]
    async fn token_analytics_refresh_ignores_preexisting_cache() {
        let suffix = TEST_PERIOD_SUFFIX.fetch_add(1, Ordering::SeqCst);
        let period = format!("test-refresh-bypass-existing-{suffix}");
        let collect_count = Arc::new(AtomicUsize::new(0));
        let now = chrono::Utc::now();

        let first_count = Arc::clone(&collect_count);
        let first = collect_token_analytics_with_lock_using(
            &period,
            1,
            "Test Period",
            now,
            false,
            move |_start, _now, _label, period| {
                first_count.fetch_add(1, Ordering::SeqCst);
                let mut data = empty_token_analytics_data(&period);
                data.generated_at = "initial-cache".to_string();
                data
            },
        )
        .await
        .expect("initial result");

        assert_eq!(first.cache_state, TokenAnalyticsCacheState::Miss);
        assert_eq!(first.data.generated_at, "initial-cache");

        let refresh_count = Arc::clone(&collect_count);
        let refresh = collect_token_analytics_with_lock_using(
            &period,
            1,
            "Test Period",
            now,
            true,
            move |_start, _now, _label, period| {
                refresh_count.fetch_add(1, Ordering::SeqCst);
                let mut data = empty_token_analytics_data(&period);
                data.generated_at = "manual-refresh".to_string();
                data
            },
        )
        .await
        .expect("refresh result");

        assert_eq!(
            collect_count.load(Ordering::SeqCst),
            2,
            "manual refresh should bypass cache entries written before it began waiting"
        );
        assert_eq!(refresh.cache_state, TokenAnalyticsCacheState::Bypass);
        assert_eq!(refresh.data.generated_at, "manual-refresh");
    }

    #[tokio::test]
    async fn token_analytics_refresh_does_not_reuse_concurrent_normal_miss() {
        let suffix = TEST_PERIOD_SUFFIX.fetch_add(1, Ordering::SeqCst);
        let period = format!("test-refresh-skips-normal-miss-{suffix}");
        let collect_count = Arc::new(AtomicUsize::new(0));
        let now = chrono::Utc::now();

        let first_count = Arc::clone(&collect_count);
        let normal_miss = collect_token_analytics_with_lock_using(
            &period,
            1,
            "Test Period",
            now,
            false,
            move |_start, _now, _label, period| {
                first_count.fetch_add(1, Ordering::SeqCst);
                std::thread::sleep(StdDuration::from_millis(50));
                let mut data = empty_token_analytics_data(&period);
                data.generated_at = "normal-miss".to_string();
                data
            },
        );

        let refresh_count = Arc::clone(&collect_count);
        let manual_refresh = collect_token_analytics_with_lock_using(
            &period,
            1,
            "Test Period",
            now,
            true,
            move |_start, _now, _label, period| {
                refresh_count.fetch_add(1, Ordering::SeqCst);
                let mut data = empty_token_analytics_data(&period);
                data.generated_at = "manual-refresh".to_string();
                data
            },
        );

        let (normal_result, refresh_result) = tokio::join!(normal_miss, manual_refresh);

        assert_eq!(
            collect_count.load(Ordering::SeqCst),
            2,
            "manual refresh should not reuse a cache entry written by a normal miss or prewarm"
        );
        assert_eq!(
            normal_result.expect("normal miss result").cache_state,
            TokenAnalyticsCacheState::Miss
        );
        let refresh = refresh_result.expect("refresh result");
        assert_eq!(refresh.cache_state, TokenAnalyticsCacheState::Bypass);
        assert_eq!(refresh.data.generated_at, "manual-refresh");
    }

    #[tokio::test]
    async fn concurrent_token_analytics_refreshes_are_coalesced() {
        let suffix = TEST_PERIOD_SUFFIX.fetch_add(1, Ordering::SeqCst);
        let period = format!("test-refresh-single-flight-{suffix}");
        let collect_count = Arc::new(AtomicUsize::new(0));
        let now = chrono::Utc::now();

        let first_count = Arc::clone(&collect_count);
        let first = collect_token_analytics_with_lock_using(
            &period,
            1,
            "Test Period",
            now,
            true,
            move |_start, _now, _label, period| {
                first_count.fetch_add(1, Ordering::SeqCst);
                std::thread::sleep(StdDuration::from_millis(50));
                empty_token_analytics_data(&period)
            },
        );

        let second_count = Arc::clone(&collect_count);
        let second = collect_token_analytics_with_lock_using(
            &period,
            1,
            "Test Period",
            now,
            true,
            move |_start, _now, _label, period| {
                second_count.fetch_add(1, Ordering::SeqCst);
                empty_token_analytics_data(&period)
            },
        );

        let (first_result, second_result) = tokio::join!(first, second);

        assert_eq!(
            collect_count.load(Ordering::SeqCst),
            1,
            "concurrent manual refreshes should share the first refresh scan"
        );
        assert_eq!(
            first_result.expect("first result").cache_state,
            TokenAnalyticsCacheState::Bypass
        );
        assert_eq!(
            second_result.expect("second result").cache_state,
            TokenAnalyticsCacheState::Bypass
        );
    }
}
