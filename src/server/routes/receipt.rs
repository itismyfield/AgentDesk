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
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration as StdDuration, Instant};

use super::AppState;
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
    /// the freshest possible numbers.
    fresh: Option<bool>,
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
    data: Arc<receipt::TokenAnalyticsData>,
}

static TOKEN_ANALYTICS_CACHE: OnceLock<Mutex<HashMap<String, CachedAnalytics>>> = OnceLock::new();

fn token_analytics_cache() -> &'static Mutex<HashMap<String, CachedAnalytics>> {
    TOKEN_ANALYTICS_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn read_cached_token_analytics(period: &str) -> Option<Arc<receipt::TokenAnalyticsData>> {
    let cache = token_analytics_cache().lock().ok()?;
    let entry = cache.get(period)?;
    if entry.cached_at.elapsed() > TOKEN_ANALYTICS_CACHE_TTL {
        return None;
    }
    Some(Arc::clone(&entry.data))
}

fn write_cached_token_analytics(period: &str, data: Arc<receipt::TokenAnalyticsData>) {
    if let Ok(mut cache) = token_analytics_cache().lock() {
        cache.insert(
            period.to_string(),
            CachedAnalytics {
                cached_at: Instant::now(),
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
/// Returns `None` only when the blocking task panics; the caller is expected
/// to surface a graceful fallback (empty arrays + warn log) rather than 5xx.
pub(crate) async fn cached_or_collect_token_analytics(
    period_id: &str,
    days: i64,
    label: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<Arc<receipt::TokenAnalyticsData>> {
    if let Some(cached) = read_cached_token_analytics(period_id) {
        return Some(cached);
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
    let data = match tokio::task::spawn_blocking(move || {
        receipt::collect_token_analytics(start, now, &label_owned, &period_owned)
    })
    .await
    {
        Ok(data) => Arc::new(data),
        Err(error) => {
            tracing::warn!(period = period_id, error = %error, "token-analytics scan failed");
            return None;
        }
    };
    write_cached_token_analytics(period_id, Arc::clone(&data));
    Some(data)
}

/// Pre-warm the token-analytics in-process cache for every period the
/// dashboard requests on first paint. The first home/stats visit on a fresh
/// dcserver previously paid the ~9s filesystem scan synchronously while the
/// user watched a placeholder; with this prewarm running off a detached
/// `tokio::spawn` shortly after boot, the cache is already populated before
/// the user lands on /home, so the first request hits the 30s in-process
/// cache and returns in single-digit ms.
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
        let local_now = now.with_timezone(&Local);
        let start_date = local_now.date_naive() - chrono::Duration::days(days.saturating_sub(1));
        let start = Local
            .from_local_datetime(&start_date.and_hms_opt(0, 0, 0).unwrap())
            .single()
            .map(|dt| dt.with_timezone(&chrono::Utc))
            .unwrap_or_else(|| now - chrono::Duration::days(days));
        let label_owned = label.to_string();
        let period_owned = period.to_string();
        let started = Instant::now();
        let data = match tokio::task::spawn_blocking(move || {
            receipt::collect_token_analytics(start, now, &label_owned, &period_owned)
        })
        .await
        {
            Ok(data) => data,
            Err(error) => {
                tracing::warn!(period, error = %error, "token-analytics prewarm failed");
                continue;
            }
        };
        write_cached_token_analytics(period, Arc::new(data));
        tracing::info!(
            period,
            elapsed_ms = started.elapsed().as_millis(),
            "token-analytics prewarm done"
        );
    }
}

/// GET /api/receipt?period=month
pub async fn get_receipt(
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<ReceiptQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
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
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("collection failed: {e}")})),
            );
        }
    };

    (StatusCode::OK, Json(json!(data)))
}

/// GET /api/token-analytics?period=30d&fresh=1
pub async fn get_token_analytics(
    State(_state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<TokenAnalyticsQuery>,
) -> Response {
    let started = Instant::now();
    let period = params.period.as_deref().unwrap_or("30d");
    let bypass_cache = params.fresh.unwrap_or(false);
    let now = chrono::Utc::now();
    let local_now = now.with_timezone(&Local);

    let (days, label, period_id) = match period {
        "7d" => (7_i64, "Last 7 Days", "7d"),
        "90d" => (90_i64, "Last 90 Days", "90d"),
        _ => (30_i64, "Last 30 Days", "30d"),
    };

    if !bypass_cache {
        if let Some(cached) = read_cached_token_analytics(period_id) {
            let elapsed_ms = started.elapsed().as_millis();
            tracing::debug!(period = period_id, elapsed_ms, "token-analytics cache hit");
            return build_token_analytics_response(&cached, period_id, elapsed_ms, "hit");
        }
    }

    let start_date = local_now.date_naive() - chrono::Duration::days(days.saturating_sub(1));
    let start = Local
        .from_local_datetime(&start_date.and_hms_opt(0, 0, 0).unwrap())
        .single()
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .unwrap_or_else(|| now - chrono::Duration::days(days));

    let label_owned = label.to_string();
    let period_owned = period_id.to_string();
    let data = match tokio::task::spawn_blocking(move || {
        receipt::collect_token_analytics(start, now, &label_owned, &period_owned)
    })
    .await
    {
        Ok(d) => d,
        Err(e) => {
            let elapsed_ms = started.elapsed().as_millis();
            tracing::warn!(period = period_id, elapsed_ms, error = %e, "token-analytics failed");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("collection failed: {e}")})),
            )
                .into_response();
        }
    };

    let elapsed_ms = started.elapsed().as_millis();
    tracing::info!(
        period = period_id,
        elapsed_ms,
        bypass_cache,
        "token-analytics responded"
    );

    let arc_data = Arc::new(data);
    write_cached_token_analytics(period_id, Arc::clone(&arc_data));
    let cache_state = if bypass_cache { "bypass" } else { "miss" };
    build_token_analytics_response(&arc_data, period_id, elapsed_ms, cache_state)
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
