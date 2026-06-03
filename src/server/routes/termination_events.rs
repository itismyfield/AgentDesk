use axum::{
    Json,
    extract::{Query, State},
};
use serde::Deserialize;
use serde_json::json;
use sqlx::Row;

use super::AppState;

#[derive(Deserialize)]
pub struct TerminationQueryParams {
    pub dispatch_id: Option<String>,
    pub card_id: Option<String>,
    pub session_key: Option<String>,
    #[serde(default = "default_limit")]
    pub limit: u32,
}

fn default_limit() -> u32 {
    50
}

pub async fn list_termination_events(
    State(state): State<AppState>,
    Query(params): Query<TerminationQueryParams>,
) -> Json<serde_json::Value> {
    let limit = params.limit.min(500);

    let Some(pool) = state.pg_pool_ref() else {
        return Json(json!({
            "error": "PostgreSQL pool unavailable for termination events",
            "events": []
        }));
    };

    match list_termination_events_pg(pool, &params, limit).await {
        Ok(events) => Json(json!({"events": events})),
        Err(error) => Json(json!({"error": format!("Query error: {error}"), "events": []})),
    }
}

async fn list_termination_events_pg(
    pool: &sqlx::PgPool,
    params: &TerminationQueryParams,
    limit: u32,
) -> Result<Vec<serde_json::Value>, String> {
    let limit = limit as i64;

    let rows = if let Some(card_id) = params.card_id.as_deref() {
        sqlx::query(
            "SELECT ste.id,
                    ste.session_key,
                    ste.dispatch_id,
                    ste.killer_component,
                    ste.reason_code,
                    ste.reason_text,
                    ste.probe_snapshot,
                    ste.last_offset::BIGINT AS last_offset,
                    ste.tmux_alive,
                    TO_CHAR(ste.created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') AS created_at
             FROM session_termination_events ste
             LEFT JOIN task_dispatches td ON ste.dispatch_id = td.id
             WHERE td.kanban_card_id = $1
             ORDER BY ste.created_at DESC, ste.id DESC
             LIMIT $2",
        )
        .bind(card_id)
        .bind(limit)
        .fetch_all(pool)
        .await
    } else if let Some(dispatch_id) = params.dispatch_id.as_deref() {
        sqlx::query(
            "SELECT id,
                    session_key,
                    dispatch_id,
                    killer_component,
                    reason_code,
                    reason_text,
                    probe_snapshot,
                    last_offset::BIGINT AS last_offset,
                    tmux_alive,
                    TO_CHAR(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') AS created_at
             FROM session_termination_events
             WHERE dispatch_id = $1
             ORDER BY created_at DESC, id DESC
             LIMIT $2",
        )
        .bind(dispatch_id)
        .bind(limit)
        .fetch_all(pool)
        .await
    } else if let Some(session_key) = params.session_key.as_deref() {
        sqlx::query(
            "SELECT id,
                    session_key,
                    dispatch_id,
                    killer_component,
                    reason_code,
                    reason_text,
                    probe_snapshot,
                    last_offset::BIGINT AS last_offset,
                    tmux_alive,
                    TO_CHAR(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') AS created_at
             FROM session_termination_events
             WHERE session_key = $1
             ORDER BY created_at DESC, id DESC
             LIMIT $2",
        )
        .bind(session_key)
        .bind(limit)
        .fetch_all(pool)
        .await
    } else {
        sqlx::query(
            "SELECT id,
                    session_key,
                    dispatch_id,
                    killer_component,
                    reason_code,
                    reason_text,
                    probe_snapshot,
                    last_offset::BIGINT AS last_offset,
                    tmux_alive,
                    TO_CHAR(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') AS created_at
             FROM session_termination_events
             ORDER BY created_at DESC, id DESC
             LIMIT $1",
        )
        .bind(limit)
        .fetch_all(pool)
        .await
    }
    .map_err(|error| format!("{error}"))?;

    rows.into_iter()
        .map(|row| {
            Ok(json!({
                "id": row.try_get::<i64, _>("id").map_err(|error| format!("{error}"))?,
                "session_key": row.try_get::<String, _>("session_key").map_err(|error| format!("{error}"))?,
                "dispatch_id": row.try_get::<Option<String>, _>("dispatch_id").map_err(|error| format!("{error}"))?,
                "killer_component": row.try_get::<String, _>("killer_component").map_err(|error| format!("{error}"))?,
                "reason_code": row.try_get::<String, _>("reason_code").map_err(|error| format!("{error}"))?,
                "reason_text": row.try_get::<Option<String>, _>("reason_text").map_err(|error| format!("{error}"))?,
                "probe_snapshot": row.try_get::<Option<String>, _>("probe_snapshot").map_err(|error| format!("{error}"))?,
                "last_offset": row.try_get::<Option<i64>, _>("last_offset").map_err(|error| format!("{error}"))?,
                "tmux_alive": row.try_get::<Option<i64>, _>("tmux_alive").map_err(|error| format!("{error}"))?.map(|value| value != 0),
                "created_at": row.try_get::<String, _>("created_at").map_err(|error| format!("{error}"))?,
            }))
        })
        .collect()
}
