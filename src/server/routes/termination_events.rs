use axum::{
    Json,
    extract::{Query, State},
};
use serde::Deserialize;
use serde_json::json;

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

    let conn = match state.db.read_conn() {
        Ok(c) => c,
        Err(e) => {
            return Json(json!({"error": format!("DB error: {e}"), "events": []}));
        }
    };

    // Build query based on filters
    let (sql, sql_params): (String, Vec<Box<dyn rusqlite::types::ToSql>>) =
        if let Some(ref card_id) = params.card_id {
            (
                "SELECT ste.id, ste.session_key, ste.dispatch_id, ste.killer_component, \
                 ste.reason_code, ste.reason_text, ste.probe_snapshot, ste.last_offset, \
                 ste.tmux_alive, ste.created_at \
                 FROM session_termination_events ste \
                 LEFT JOIN task_dispatches td ON ste.dispatch_id = td.id \
                 WHERE td.kanban_card_id = ?1 \
                 ORDER BY ste.created_at DESC LIMIT ?2"
                    .to_string(),
                vec![
                    Box::new(card_id.clone()) as Box<dyn rusqlite::types::ToSql>,
                    Box::new(limit as i64),
                ],
            )
        } else if let Some(ref dispatch_id) = params.dispatch_id {
            (
                "SELECT id, session_key, dispatch_id, killer_component, \
                 reason_code, reason_text, probe_snapshot, last_offset, \
                 tmux_alive, created_at \
                 FROM session_termination_events \
                 WHERE dispatch_id = ?1 \
                 ORDER BY created_at DESC LIMIT ?2"
                    .to_string(),
                vec![
                    Box::new(dispatch_id.clone()) as Box<dyn rusqlite::types::ToSql>,
                    Box::new(limit as i64),
                ],
            )
        } else if let Some(ref session_key) = params.session_key {
            (
                "SELECT id, session_key, dispatch_id, killer_component, \
                 reason_code, reason_text, probe_snapshot, last_offset, \
                 tmux_alive, created_at \
                 FROM session_termination_events \
                 WHERE session_key = ?1 \
                 ORDER BY created_at DESC LIMIT ?2"
                    .to_string(),
                vec![
                    Box::new(session_key.clone()) as Box<dyn rusqlite::types::ToSql>,
                    Box::new(limit as i64),
                ],
            )
        } else {
            (
                "SELECT id, session_key, dispatch_id, killer_component, \
                 reason_code, reason_text, probe_snapshot, last_offset, \
                 tmux_alive, created_at \
                 FROM session_termination_events \
                 ORDER BY created_at DESC LIMIT ?1"
                    .to_string(),
                vec![Box::new(limit as i64) as Box<dyn rusqlite::types::ToSql>],
            )
        };

    let params_refs: Vec<&dyn rusqlite::types::ToSql> = sql_params.iter().map(|p| &**p).collect();

    let result = conn.prepare(&sql).and_then(|mut stmt| {
        let rows = stmt.query_map(params_refs.as_slice(), |row| {
            Ok(json!({
                "id": row.get::<_, i64>(0)?,
                "session_key": row.get::<_, String>(1)?,
                "dispatch_id": row.get::<_, Option<String>>(2)?,
                "killer_component": row.get::<_, String>(3)?,
                "reason_code": row.get::<_, String>(4)?,
                "reason_text": row.get::<_, Option<String>>(5)?,
                "probe_snapshot": row.get::<_, Option<String>>(6)?,
                "last_offset": row.get::<_, Option<i64>>(7)?,
                "tmux_alive": row.get::<_, Option<i32>>(8)?.map(|v| v != 0),
                "created_at": row.get::<_, Option<String>>(9)?,
            }))
        })?;
        rows.collect::<Result<Vec<_>, _>>()
    });

    match result {
        Ok(events) => Json(json!({"events": events})),
        Err(e) => Json(json!({"error": format!("Query error: {e}"), "events": []})),
    }
}
