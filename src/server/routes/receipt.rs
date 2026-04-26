use axum::{
    Json,
    extract::State,
    http::{HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use chrono::{Datelike, Local, TimeZone};
use serde::Deserialize;
use serde_json::json;
use std::time::Instant;

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
            let ws = state
                .db
                .lock()
                .ok()
                .and_then(|conn| receipt::ratelimit_window_start(&conn));
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

/// GET /api/token-analytics?period=30d
pub async fn get_token_analytics(
    State(_state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<TokenAnalyticsQuery>,
) -> Response {
    let started = Instant::now();
    let period = params.period.as_deref().unwrap_or("30d");
    let now = chrono::Utc::now();
    let local_now = now.with_timezone(&Local);

    let (days, label, period_id) = match period {
        "7d" => (7_i64, "Last 7 Days", "7d"),
        "90d" => (90_i64, "Last 90 Days", "90d"),
        _ => (30_i64, "Last 30 Days", "30d"),
    };
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
    tracing::info!(period = period_id, elapsed_ms, "token-analytics responded");

    let mut response = (StatusCode::OK, Json(json!(data))).into_response();
    let headers = response.headers_mut();
    // Codex review (PR #1258, 3rd pass): stale-while-revalidate=120 still let
    // browsers that honor SWR serve a stale body for up to 2 min on explicit
    // refreshes. Switch to no-cache + must-revalidate so the Stats refresh
    // button always re-validates with the origin. Dashboard-side SWR via
    // sessionStorage (StatsPageView.tsx) covers the perceived-speed need.
    headers.insert(
        "Cache-Control",
        HeaderValue::from_static("private, no-cache, must-revalidate"),
    );
    if let Ok(value) = HeaderValue::from_str(&elapsed_ms.to_string()) {
        headers.insert("X-Response-Time-Ms", value);
    }
    response
}
