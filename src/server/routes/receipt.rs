use axum::{Json, extract::State, http::StatusCode};
use serde::Deserialize;
use serde_json::json;

use super::AppState;
use crate::receipt;

#[derive(Debug, Deserialize)]
pub struct ReceiptQuery {
    /// Period: "today", "week", "month", "ratelimit", or "all"
    period: Option<String>,
}

/// GET /api/receipt?period=month
pub async fn get_receipt(
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<ReceiptQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    let period = params.period.as_deref().unwrap_or("month");
    let now = chrono::Utc::now();

    let (start, label) = match period {
        "today" => {
            let today = now
                .date_naive()
                .and_hms_opt(0, 0, 0)
                .map(|ndt| ndt.and_utc())
                .unwrap_or_else(|| now - chrono::Duration::hours(24));
            (today, "Today")
        }
        "week" => (now - chrono::Duration::days(7), "Last 7 Days"),
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
        "all" => {
            // Use Unix epoch as start to capture entire subscription history
            (
                chrono::DateTime::from_timestamp(0, 0)
                    .unwrap_or(now - chrono::Duration::days(3650)),
                "All Time",
            )
        }
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
