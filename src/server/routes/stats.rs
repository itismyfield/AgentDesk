use axum::{
    Json,
    extract::{Query, State},
    http::StatusCode,
};
use serde::Deserialize;
use serde_json::json;
use std::collections::{HashMap, HashSet};

use super::AppState;
use super::kanban::STALLED_ACTIVITY_AT_SQL;
use super::session_activity::SessionActivityResolver;

#[derive(Debug, Deserialize)]
pub struct StatsQuery {
    #[serde(rename = "officeId")]
    pub office_id: Option<String>,
}

/// GET /api/stats
pub async fn get_stats(
    State(state): State<AppState>,
    Query(params): Query<StatsQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    // Determine agent filter based on officeId
    let agent_ids: Option<Vec<String>> = if let Some(ref oid) = params.office_id {
        let mut stmt = conn
            .prepare("SELECT agent_id FROM office_agents WHERE office_id = ?1")
            .unwrap();
        let ids: Vec<String> = stmt
            .query_map([oid], |row| row.get(0))
            .ok()
            .map(|iter| iter.filter_map(|r| r.ok()).collect())
            .unwrap_or_default();
        Some(ids)
    } else {
        None
    };

    // Helper: build WHERE clause for agent filtering
    let agent_where = |col: &str| -> String {
        match &agent_ids {
            Some(ids) if !ids.is_empty() => {
                let placeholders: Vec<String> = ids
                    .iter()
                    .map(|id| format!("'{}'", id.replace('\'', "''")))
                    .collect();
                format!("{} IN ({})", col, placeholders.join(","))
            }
            Some(_) => format!("{} = '__none__'", col), // empty office
            None => "1=1".to_string(),
        }
    };

    let agents_sql = format!(
        "SELECT id, name, name_ko, avatar_emoji, xp, department, status, sprite_number
         FROM agents WHERE {} ORDER BY id",
        agent_where("id")
    );
    let mut agents_stmt = match conn.prepare(&agents_sql) {
        Ok(stmt) => stmt,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("query prepare failed: {e}")})),
            );
        }
    };

    let agent_rows: Vec<(
        String,
        String,
        Option<String>,
        Option<String>,
        i64,
        Option<String>,
        Option<String>,
        Option<i64>,
    )> = agents_stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, f64>(4).unwrap_or(0.0) as i64,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, Option<i64>>(7)?,
            ))
        })
        .ok()
        .map(|iter| iter.filter_map(|r| r.ok()).collect())
        .unwrap_or_default();

    let mut working_session_stmt = match conn.prepare(
        "SELECT session_key, agent_id, status, active_dispatch_id, last_heartbeat
         FROM sessions
         WHERE agent_id IS NOT NULL
           AND status != 'disconnected'",
    ) {
        Ok(stmt) => stmt,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("query prepare failed: {e}")})),
            );
        }
    };
    let session_rows: Vec<(
        Option<String>,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
    )> = working_session_stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
            ))
        })
        .ok()
        .map(|iter| iter.filter_map(|r| r.ok()).collect())
        .unwrap_or_default();
    let mut resolver = SessionActivityResolver::new();
    let mut working_session_agents: HashSet<String> = HashSet::new();
    let mut dispatched_count = 0i64;
    for (session_key, agent_id, status, active_dispatch_id, last_heartbeat) in session_rows {
        let effective = resolver.resolve(
            session_key.as_deref(),
            status.as_deref(),
            active_dispatch_id.as_deref(),
            last_heartbeat.as_deref(),
        );
        if effective.is_working {
            working_session_agents.insert(agent_id);
            dispatched_count += 1;
        }
    }

    let total = agent_rows.len() as i64;
    let mut working = 0i64;
    let mut on_break = 0i64;
    let mut offline = 0i64;
    let mut idle = 0i64;

    for (agent_id, _, _, _, _, _, base_status, _) in &agent_rows {
        let effective_working =
            working_session_agents.contains(agent_id) || base_status.as_deref() == Some("working");
        if effective_working {
            working += 1;
            continue;
        }
        match base_status.as_deref() {
            Some("break") => on_break += 1,
            Some("offline") => offline += 1,
            _ => idle += 1,
        }
    }

    // ── top_agents (by XP, top 10) ──
    let mut top_agents_src = agent_rows.clone();
    top_agents_src.sort_by(|a, b| b.4.cmp(&a.4).then_with(|| a.0.cmp(&b.0)));
    let top_agents: Vec<serde_json::Value> = top_agents_src
        .into_iter()
        .take(10)
        .map(|(id, name, name_ko, avatar_emoji, xp, _, _, sprite_number)| {
            let tasks_done: i64 = conn
                .query_row(
                    "SELECT COUNT(DISTINCT kc.id) FROM kanban_cards kc WHERE kc.assigned_agent_id = ?1 AND kc.status = 'done'",
                    [&id],
                    |row| row.get(0),
                )
                .unwrap_or(0);
            let tokens: i64 = conn
                .query_row(
                    "SELECT COALESCE(SUM(tokens), 0) FROM sessions WHERE agent_id = ?1",
                    [&id],
                    |row| row.get(0),
                )
                .unwrap_or(0);
            json!({
                "id": id,
                "name": name,
                "name_ko": name_ko,
                "avatar_emoji": avatar_emoji,
                "sprite_number": sprite_number,
                "stats_xp": xp,
                "stats_tasks_done": tasks_done,
                "stats_tokens": tokens,
            })
        })
        .collect();

    // ── departments stats ──
    let departments = {
        let mut stats_by_dept: HashMap<String, (i64, i64, i64)> = HashMap::new();
        for (agent_id, _, _, _, xp, department_id, base_status, _) in &agent_rows {
            let Some(dept_id) = department_id else {
                continue;
            };
            let entry = stats_by_dept.entry(dept_id.clone()).or_insert((0, 0, 0));
            entry.0 += 1;
            entry.2 += *xp;
            let effective_working = working_session_agents.contains(agent_id)
                || base_status.as_deref() == Some("working");
            if effective_working {
                entry.1 += 1;
            }
        }

        let mut bind_values: Vec<Box<dyn libsql_rusqlite::types::ToSql>> = Vec::new();
        let mut dept_sql = String::from("SELECT id, name, name_ko, icon, color FROM departments");
        if let Some(ref oid) = params.office_id {
            dept_sql.push_str(
                " WHERE id IN (
                    SELECT DISTINCT department_id
                    FROM office_agents
                    WHERE office_id = ?1 AND department_id IS NOT NULL
                )",
            );
            bind_values.push(Box::new(oid.clone()));
        }
        dept_sql.push_str(" ORDER BY sort_order, id");

        let params_ref: Vec<&dyn libsql_rusqlite::types::ToSql> =
            bind_values.iter().map(|v| v.as_ref()).collect();
        let mut stmt = match conn.prepare(&dept_sql) {
            Ok(stmt) => stmt,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("query prepare failed: {e}")})),
                );
            }
        };
        stmt.query_map(params_ref.as_slice(), |row| {
            let dept_id: String = row.get(0)?;
            let stats = stats_by_dept.get(&dept_id).copied().unwrap_or((0, 0, 0));
            Ok(json!({
                "id": dept_id,
                "name": row.get::<_, Option<String>>(1)?,
                "name_ko": row.get::<_, Option<String>>(2)?,
                "icon": row.get::<_, Option<String>>(3)?,
                "color": row.get::<_, Option<String>>(4)?,
                "total_agents": stats.0,
                "working_agents": stats.1,
                "sum_xp": stats.2,
            }))
        })
        .ok()
        .map(|iter| iter.filter_map(|r| r.ok()).collect::<Vec<_>>())
        .unwrap_or_default()
    };

    // ── kanban stats ──
    let kanban = {
        let open_total: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM kanban_cards WHERE status NOT IN ('done', 'cancelled')",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);

        let review_queue: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM kanban_cards WHERE status = 'review'",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);

        let blocked: i64 = conn
            .prepare("SELECT review_status, blocked_reason FROM kanban_cards")
            .ok()
            .and_then(|mut stmt| {
                stmt.query_map([], |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                    ))
                })
                .ok()
                .map(|rows| {
                    rows.filter_map(|row| row.ok())
                        .filter(|(review_status, blocked_reason)| {
                            crate::manual_intervention::requires_manual_intervention(
                                review_status.as_deref(),
                                blocked_reason.as_deref(),
                            )
                        })
                        .count() as i64
                })
            })
            .unwrap_or(0);

        let failed: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM kanban_cards WHERE status = 'failed'",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);

        // by_status
        let mut by_status = serde_json::Map::new();
        let statuses = [
            "backlog",
            "ready",
            "requested",
            "in_progress",
            "review",
            "failed",
            "done",
            "cancelled",
        ];
        for status in &statuses {
            let count: i64 = conn
                .query_row(
                    &format!(
                        "SELECT COUNT(*) FROM kanban_cards WHERE status = '{}'",
                        status
                    ),
                    [],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            by_status.insert(status.to_string(), json!(count));
        }

        // top_repos
        let top_repos = {
            let mut stmt = conn
                .prepare(
                    "SELECT repo_id, COUNT(*) as cnt FROM kanban_cards
                     WHERE repo_id IS NOT NULL AND status NOT IN ('done', 'cancelled')
                     GROUP BY repo_id ORDER BY cnt DESC LIMIT 5",
                )
                .unwrap();
            let rows: Vec<serde_json::Value> = stmt
                .query_map([], |row| {
                    Ok(json!({
                        "github_repo": row.get::<_, String>(0)?,
                        "open_count": row.get::<_, i64>(1)?,
                        "pressure_count": row.get::<_, i64>(1)?,
                    }))
                })
                .ok()
                .map(|iter| iter.filter_map(|r| r.ok()).collect())
                .unwrap_or_default();
            rows
        };

        let waiting_acceptance: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM kanban_cards WHERE status = 'requested'",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);
        let stale_in_progress: i64 = conn
            .query_row(
                &format!(
                    "SELECT COUNT(*) \
                     FROM kanban_cards kc \
                     LEFT JOIN task_dispatches td ON td.id = kc.latest_dispatch_id \
                     WHERE kc.status = 'in_progress' \
                     AND {STALLED_ACTIVITY_AT_SQL} < datetime('now', '-100 minutes')"
                ),
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);

        json!({
            "open_total": open_total,
            "review_queue": review_queue,
            "blocked": blocked,
            "failed": failed,
            "waiting_acceptance": waiting_acceptance,
            "stale_in_progress": stale_in_progress,
            "by_status": by_status,
            "top_repos": top_repos,
        })
    };

    // ── github_closed_today ──
    let github_closed_today: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM kanban_cards WHERE status = 'done' AND date(updated_at) = date('now') AND github_issue_url IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);

    (
        StatusCode::OK,
        Json(json!({
            "agents": {
                "total": total,
                "working": working,
                "idle": idle,
                "break": on_break,
                "offline": offline,
            },
            "top_agents": top_agents,
            "departments": departments,
            "dispatched_count": dispatched_count,
            "kanban": kanban,
            "github_closed_today": github_closed_today,
        })),
    )
}
