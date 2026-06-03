//! Home dashboard KPI trend endpoint (#1242).
//!
//! Surfaces the 4 sparkline series the home KPI tiles need in a single
//! response so the dashboard can hydrate every tile with one round-trip:
//!
//!   - `tokens`       — daily total tokens (mirrors /api/token-analytics.daily)
//!   - `cost`         — daily USD cost (same source as tokens)
//!   - `in_progress`  — daily count of `task_dispatches` rows created on the
//!                      day, used as a proxy for "active card throughput"
//!                      (no historical snapshot of `kanban_cards.status` is
//!                      kept yet — see issue #1242 risk note).
//!   - `rate_limit`   — current per-provider utilization plus a flat 14-day
//!                      sparkline derived from the latest cached value.
//!                      `rate_limit_cache` only stores the most recent
//!                      snapshot per provider, so the sparkline replays the
//!                      current value across the window. Providers without
//!                      data (e.g. unsupported, no recent session) come back
//!                      with `unsupported: true` + an empty `values` array
//!                      so the dashboard can render a placeholder.
//!
//! All four series share the same length (`days`, default 14, clamped to
//! [1, 30]) so a sparkline component can render any of them with the same
//! axis.

use axum::{
    Json,
    extract::{Query, State},
    http::StatusCode,
};
use chrono::{Duration, Local, NaiveDate, TimeZone};
use serde::Deserialize;
use serde_json::json;
use sqlx::Row;
use std::collections::BTreeMap;

use super::{AppState, analytics};

/// Default lookback window for the home KPI sparklines.
const DEFAULT_DAYS: i64 = 14;
/// Hard ceiling. Any larger request would force the caller to use the heavier
/// `/api/token-analytics?period=30d|90d` endpoint instead.
const MAX_DAYS: i64 = 30;
/// Minimum window — anything below this would render a degenerate sparkline.
const MIN_DAYS: i64 = 1;

#[derive(Debug, Default, Deserialize)]
pub struct HomeKpiTrendsQuery {
    pub days: Option<i64>,
}

/// GET /api/home/kpi-trends?days=14
///
/// Returns the four KPI sparkline series in a single payload.
pub async fn home_kpi_trends(
    State(state): State<AppState>,
    Query(params): Query<HomeKpiTrendsQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    let days = params
        .days
        .unwrap_or(DEFAULT_DAYS)
        .clamp(MIN_DAYS, MAX_DAYS);

    let Some(pool) = state.pg_pool_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "postgres pool unavailable"})),
        );
    };

    let now = chrono::Utc::now();
    let local_today = now.with_timezone(&Local).date_naive();
    let date_keys = day_window(local_today, days);

    // ── Tokens + cost ─────────────────────────────────────────────────────
    // Reuse the shared receipt::token-analytics 30 s in-process cache (#1303)
    // so cold dashboard loads don't pay two ~9 s filesystem scans (one here,
    // one for /api/token-analytics).
    //
    // The cache is keyed by the canonical analytics periods (7d / 30d / 90d)
    // that `prewarm_token_analytics_cache` populates and `/api/token-analytics`
    // writes. Round the home-trends `days` value up to the nearest covering
    // canonical period so a `days=14` request hits the same `30d` cache slot
    // that the token-analytics endpoint and prewarm already populate. The
    // sparkline slice afterwards is already keyed off `date_keys`, so the
    // wider cached payload naturally narrows down to the requested window.
    let (cache_period_id, cache_days, cache_label) = canonical_cache_window(days);
    let analytics_data = super::receipt::cached_or_collect_token_analytics(
        cache_period_id,
        cache_days,
        cache_label,
        now,
    )
    .await;

    let (tokens_values, cost_values) = match analytics_data.as_deref() {
        Some(data) => {
            let mut by_day: BTreeMap<String, (u64, f64)> = BTreeMap::new();
            for day in &data.daily {
                by_day.insert(day.date.clone(), (day.total_tokens, day.cost));
            }
            let tokens = date_keys
                .iter()
                .map(|d| by_day.get(d).map(|(t, _)| *t).unwrap_or(0))
                .map(|v| json!(v))
                .collect::<Vec<_>>();
            let costs = date_keys
                .iter()
                .map(|d| by_day.get(d).map(|(_, c)| *c).unwrap_or(0.0))
                .map(|v| json!(v))
                .collect::<Vec<_>>();
            (tokens, costs)
        }
        None => (
            vec![json!(0); date_keys.len()],
            vec![json!(0.0); date_keys.len()],
        ),
    };

    // ── In-progress (daily dispatch count from PG) ────────────────────────
    let in_progress_values = collect_in_progress_trend_pg(pool, &date_keys).await;

    // ── Rate-limit current value + flat sparkline ─────────────────────────
    let rate_limit_providers =
        analytics::build_rate_limit_provider_payloads_pg(pool, now.timestamp()).await;
    let rate_limit_payload = build_rate_limit_kpi(&rate_limit_providers, date_keys.len());

    let body = json!({
        "days": days,
        "generated_at": now.to_rfc3339(),
        "dates": date_keys,
        "tokens": {
            "label": "Today's tokens",
            "unit": "tokens",
            "values": tokens_values,
        },
        "cost": {
            "label": "API cost",
            "unit": "usd",
            "values": cost_values,
        },
        "in_progress": {
            "label": "In progress",
            "unit": "dispatches",
            "values": in_progress_values,
        },
        "rate_limit": rate_limit_payload,
    });

    (StatusCode::OK, Json(body))
}

/// Round the requested home-trends `days` value up to the nearest canonical
/// token-analytics period (7d / 30d / 90d) so the cache slot collides with
/// what `prewarm_token_analytics_cache` and `/api/token-analytics` populate.
/// Returns (period_id, days_for_scan, label) consumed by
/// `cached_or_collect_token_analytics`. The home sparkline already slices
/// down to the requested window via `date_keys`, so a wider cached payload
/// is harmless.
fn canonical_cache_window(days: i64) -> (&'static str, i64, &'static str) {
    if days <= 7 {
        ("7d", 7, "Last 7 Days")
    } else if days <= 30 {
        ("30d", 30, "Last 30 Days")
    } else {
        ("90d", 90, "Last 90 Days")
    }
}

/// Build the ordered list of YYYY-MM-DD keys covering the trailing
/// `days`-day window ending at `today` (inclusive).
fn day_window(today: NaiveDate, days: i64) -> Vec<String> {
    let mut out = Vec::with_capacity(days.max(0) as usize);
    let span = days.saturating_sub(1);
    for offset in (0..=span).rev() {
        let date = today - Duration::days(offset);
        out.push(date.format("%Y-%m-%d").to_string());
    }
    out
}

/// Returns one entry per date in `date_keys` containing the count of
/// `task_dispatches` rows whose `created_at` falls on that local date.
/// Rows with a NULL `created_at` are ignored.
///
/// Codex P2 on #1298: previous implementation cast `created_at::date` in PG,
/// which uses the PG session timezone — when that differs from the server's
/// `chrono::Local`, dispatches near midnight are bucketed under the wrong
/// day and the in-progress sparkline silently miscounts. Fetch the raw
/// TIMESTAMPTZ values (filtered by a UTC lower bound derived from the first
/// local date) and bucket them in Rust using `chrono::Local` so the bucket
/// boundaries match `date_keys` regardless of PG TZ config.
async fn collect_in_progress_trend_pg(
    pool: &sqlx::PgPool,
    date_keys: &[String],
) -> Vec<serde_json::Value> {
    if date_keys.is_empty() {
        return Vec::new();
    }
    let Some(first_key) = date_keys.first() else {
        return Vec::new();
    };
    let Ok(first_local_date) = chrono::NaiveDate::parse_from_str(first_key, "%Y-%m-%d") else {
        tracing::warn!(
            day = %first_key,
            "home_kpi_trends in-progress: first date_key is not a valid YYYY-MM-DD"
        );
        return vec![json!(0); date_keys.len()];
    };
    let Some(local_start_naive) = first_local_date.and_hms_opt(0, 0, 0) else {
        return vec![json!(0); date_keys.len()];
    };
    let Some(local_start) = Local.from_local_datetime(&local_start_naive).single() else {
        // Skip ambiguous DST transitions — the upper bound only narrows the
        // result set, so falling back to "no lower bound" still produces a
        // correct (just larger) row scan.
        return collect_in_progress_trend_pg_with_lower_bound(pool, date_keys, None).await;
    };
    let utc_lower = local_start.with_timezone(&chrono::Utc);
    collect_in_progress_trend_pg_with_lower_bound(pool, date_keys, Some(utc_lower)).await
}

async fn collect_in_progress_trend_pg_with_lower_bound(
    pool: &sqlx::PgPool,
    date_keys: &[String],
    lower_bound_utc: Option<chrono::DateTime<chrono::Utc>>,
) -> Vec<serde_json::Value> {
    let rows_result = match lower_bound_utc {
        Some(lower) => {
            sqlx::query("SELECT created_at FROM task_dispatches WHERE created_at >= $1")
                .bind(lower)
                .fetch_all(pool)
                .await
        }
        None => {
            sqlx::query("SELECT created_at FROM task_dispatches")
                .fetch_all(pool)
                .await
        }
    };
    let rows = match rows_result {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(error = %error, "home_kpi_trends in-progress query failed");
            return vec![json!(0); date_keys.len()];
        }
    };

    let mut by_day: BTreeMap<String, i64> = BTreeMap::new();
    for row in rows {
        let Ok(created_at) = row.try_get::<chrono::DateTime<chrono::Utc>, _>("created_at") else {
            continue;
        };
        let local_day = created_at
            .with_timezone(&Local)
            .date_naive()
            .format("%Y-%m-%d")
            .to_string();
        *by_day.entry(local_day).or_insert(0) += 1;
    }

    date_keys
        .iter()
        .map(|d| json!(by_day.get(d).copied().unwrap_or(0)))
        .collect()
}

/// Convert the existing `/api/rate-limits` payload into the home-KPI shape:
/// each provider gets a `current_pct` (max bucket utilization 0..100), a flat
/// `values` sparkline filled with that current_pct (or empty for unsupported
/// providers), and the original `unsupported` / `stale` flags.
fn build_rate_limit_kpi(
    providers: &[serde_json::Value],
    sparkline_len: usize,
) -> serde_json::Value {
    let entries: Vec<serde_json::Value> = providers
        .iter()
        .map(|provider| {
            let name = provider
                .get("provider")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let unsupported = provider
                .get("unsupported")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let stale = provider
                .get("stale")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let reason = provider
                .get("reason")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let current_pct = if unsupported {
                None
            } else {
                bucket_max_utilization_pct(provider)
            };
            // No historical rate-limit snapshot exists yet (#1242 risk note),
            // so we paint a flat sparkline using the current value. When data
            // is missing the dashboard can render an empty placeholder.
            let values: Vec<serde_json::Value> = match current_pct {
                Some(pct) => vec![json!(pct); sparkline_len],
                None => Vec::new(),
            };
            json!({
                "provider": name,
                "current_pct": current_pct,
                "unsupported": unsupported,
                "stale": stale,
                "reason": reason,
                "values": values,
            })
        })
        .collect();

    json!({
        "label": "Rate limit",
        "unit": "percent",
        "providers": entries,
    })
}

/// Pick the largest `used / limit` ratio across this provider's buckets.
/// Returns a 0..100 percentage, or `None` if no bucket carries usable
/// numeric fields.
fn bucket_max_utilization_pct(provider: &serde_json::Value) -> Option<f64> {
    let buckets = provider.get("buckets").and_then(|v| v.as_array())?;
    let mut max_pct: Option<f64> = None;
    for bucket in buckets {
        let limit = bucket
            .get("limit")
            .and_then(value_as_f64)
            .filter(|v| *v > 0.0);
        let used = bucket.get("used").and_then(value_as_f64);
        if let (Some(limit), Some(used)) = (limit, used) {
            let pct = (used / limit * 100.0).clamp(0.0, 100.0);
            max_pct = Some(max_pct.map_or(pct, |current| current.max(pct)));
        }
    }
    max_pct
}

fn value_as_f64(value: &serde_json::Value) -> Option<f64> {
    match value {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => s.parse::<f64>().ok(),
        _ => None,
    }
}
