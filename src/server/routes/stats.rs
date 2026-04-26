use axum::{
    Json,
    extract::{Query, State},
    http::StatusCode,
};
use serde::Deserialize;
use serde_json::json;
use sqlx::{PgPool, Row};
use std::collections::{HashMap, HashSet};

use super::AppState;
use super::session_activity::SessionActivityResolver;

#[derive(Debug, Deserialize)]
pub struct StatsQuery {
    #[serde(rename = "officeId")]
    pub office_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct MementoStatsQuery {
    pub hours: Option<usize>,
}

#[derive(Clone)]
struct AgentStatsRow {
    id: String,
    name: String,
    name_ko: Option<String>,
    avatar_emoji: Option<String>,
    xp: i64,
    department_id: Option<String>,
    status: Option<String>,
    sprite_number: Option<i64>,
    tasks_done: i64,
    tokens: i64,
}

async fn load_agent_stats_pg(
    pool: &PgPool,
    office_id: Option<&str>,
) -> Result<Vec<AgentStatsRow>, String> {
    let sql_with_office = "
        SELECT a.id,
               a.name,
               a.name_ko,
               a.avatar_emoji,
               COALESCE(a.xp, 0)::BIGINT AS xp,
               a.department,
               a.status,
               a.sprite_number::BIGINT AS sprite_number,
               (
                   SELECT COUNT(DISTINCT kc.id)::BIGINT
                     FROM kanban_cards kc
                    WHERE kc.assigned_agent_id = a.id
                      AND kc.status = 'done'
               ) AS tasks_done,
               (
                   SELECT COALESCE(SUM(s.tokens), 0)::BIGINT
                     FROM sessions s
                    WHERE s.agent_id = a.id
               ) AS total_tokens
          FROM agents a
          JOIN office_agents oa
            ON oa.agent_id = a.id
         WHERE oa.office_id = $1
         ORDER BY a.id";
    let sql_all = "
        SELECT a.id,
               a.name,
               a.name_ko,
               a.avatar_emoji,
               COALESCE(a.xp, 0)::BIGINT AS xp,
               a.department,
               a.status,
               a.sprite_number::BIGINT AS sprite_number,
               (
                   SELECT COUNT(DISTINCT kc.id)::BIGINT
                     FROM kanban_cards kc
                    WHERE kc.assigned_agent_id = a.id
                      AND kc.status = 'done'
               ) AS tasks_done,
               (
                   SELECT COALESCE(SUM(s.tokens), 0)::BIGINT
                     FROM sessions s
                    WHERE s.agent_id = a.id
               ) AS total_tokens
          FROM agents a
         ORDER BY a.id";

    let rows = match office_id {
        Some(office_id) => {
            sqlx::query(sql_with_office)
                .bind(office_id)
                .fetch_all(pool)
                .await
        }
        None => sqlx::query(sql_all).fetch_all(pool).await,
    }
    .map_err(|error| format!("query postgres stats agents: {error}"))?;

    Ok(rows
        .into_iter()
        .map(|row| AgentStatsRow {
            id: row.try_get::<String, _>("id").unwrap_or_default(),
            name: row.try_get::<String, _>("name").unwrap_or_default(),
            name_ko: row.try_get::<Option<String>, _>("name_ko").ok().flatten(),
            avatar_emoji: row
                .try_get::<Option<String>, _>("avatar_emoji")
                .ok()
                .flatten(),
            xp: row.try_get::<i64, _>("xp").unwrap_or(0),
            department_id: row
                .try_get::<Option<String>, _>("department")
                .ok()
                .flatten(),
            status: row.try_get::<Option<String>, _>("status").ok().flatten(),
            sprite_number: row
                .try_get::<Option<i64>, _>("sprite_number")
                .ok()
                .flatten(),
            tasks_done: row
                .try_get::<Option<i64>, _>("tasks_done")
                .ok()
                .flatten()
                .unwrap_or(0),
            tokens: row
                .try_get::<Option<i64>, _>("total_tokens")
                .ok()
                .flatten()
                .unwrap_or(0),
        })
        .collect())
}

async fn load_working_session_rows_pg(
    pool: &PgPool,
    office_id: Option<&str>,
) -> Result<
    Vec<(
        Option<String>,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
    )>,
    String,
> {
    let sql_with_office = "
        SELECT s.session_key,
               s.agent_id,
               s.status,
               s.active_dispatch_id,
               TO_CHAR(s.last_heartbeat AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS') AS last_heartbeat
          FROM sessions s
          JOIN office_agents oa
            ON oa.agent_id = s.agent_id
         WHERE oa.office_id = $1
           AND s.agent_id IS NOT NULL
           AND s.status != 'disconnected'";
    let sql_all = "
        SELECT session_key,
               agent_id,
               status,
               active_dispatch_id,
               TO_CHAR(last_heartbeat AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS') AS last_heartbeat
          FROM sessions
         WHERE agent_id IS NOT NULL
           AND status != 'disconnected'";

    let rows = match office_id {
        Some(office_id) => {
            sqlx::query(sql_with_office)
                .bind(office_id)
                .fetch_all(pool)
                .await
        }
        None => sqlx::query(sql_all).fetch_all(pool).await,
    }
    .map_err(|error| format!("query postgres stats sessions: {error}"))?;

    Ok(rows
        .into_iter()
        .map(|row| {
            (
                row.try_get::<Option<String>, _>("session_key")
                    .ok()
                    .flatten(),
                row.try_get::<String, _>("agent_id").unwrap_or_default(),
                row.try_get::<Option<String>, _>("status").ok().flatten(),
                row.try_get::<Option<String>, _>("active_dispatch_id")
                    .ok()
                    .flatten(),
                row.try_get::<Option<String>, _>("last_heartbeat")
                    .ok()
                    .flatten(),
            )
        })
        .collect())
}

async fn load_departments_pg(
    pool: &PgPool,
    office_id: Option<&str>,
    agent_rows: &[AgentStatsRow],
    working_session_agents: &HashSet<String>,
) -> Result<Vec<serde_json::Value>, String> {
    let mut stats_by_dept: HashMap<String, (i64, i64, i64)> = HashMap::new();
    for agent in agent_rows {
        let Some(dept_id) = agent.department_id.as_ref() else {
            continue;
        };
        let entry = stats_by_dept.entry(dept_id.clone()).or_insert((0, 0, 0));
        entry.0 += 1;
        entry.2 += agent.xp;
        let effective_working = working_session_agents.contains(&agent.id)
            || agent.status.as_deref() == Some("working");
        if effective_working {
            entry.1 += 1;
        }
    }

    let sql_with_office = "
        SELECT id, name, name_ko, icon, color
          FROM departments
         WHERE id IN (
               SELECT DISTINCT department_id
                 FROM office_agents
                WHERE office_id = $1
                  AND department_id IS NOT NULL
         )
         ORDER BY sort_order, id";
    let sql_all = "
        SELECT id, name, name_ko, icon, color
          FROM departments
         ORDER BY sort_order, id";

    let rows = match office_id {
        Some(office_id) => {
            sqlx::query(sql_with_office)
                .bind(office_id)
                .fetch_all(pool)
                .await
        }
        None => sqlx::query(sql_all).fetch_all(pool).await,
    }
    .map_err(|error| format!("query postgres stats departments: {error}"))?;

    Ok(rows
        .into_iter()
        .map(|row| {
            let dept_id = row.try_get::<String, _>("id").unwrap_or_default();
            let stats = stats_by_dept.get(&dept_id).copied().unwrap_or((0, 0, 0));
            json!({
                "id": dept_id,
                "name": row.try_get::<Option<String>, _>("name").ok().flatten(),
                "name_ko": row.try_get::<Option<String>, _>("name_ko").ok().flatten(),
                "icon": row.try_get::<Option<String>, _>("icon").ok().flatten(),
                "color": row.try_get::<Option<String>, _>("color").ok().flatten(),
                "total_agents": stats.0,
                "working_agents": stats.1,
                "sum_xp": stats.2,
            })
        })
        .collect())
}

async fn load_kanban_stats_pg(pool: &PgPool) -> Result<serde_json::Value, String> {
    let open_total: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::BIGINT
           FROM kanban_cards
          WHERE status NOT IN ('done', 'cancelled')",
    )
    .fetch_one(pool)
    .await
    .map_err(|error| format!("query postgres open_total: {error}"))?;

    let review_queue: i64 =
        sqlx::query_scalar("SELECT COUNT(*)::BIGINT FROM kanban_cards WHERE status = 'review'")
            .fetch_one(pool)
            .await
            .map_err(|error| format!("query postgres review_queue: {error}"))?;

    let blocked_rows = sqlx::query("SELECT review_status, blocked_reason FROM kanban_cards")
        .fetch_all(pool)
        .await
        .map_err(|error| format!("query postgres blocked cards: {error}"))?;
    let blocked = blocked_rows
        .into_iter()
        .filter(|row| {
            crate::manual_intervention::requires_manual_intervention(
                row.try_get::<Option<String>, _>("review_status")
                    .ok()
                    .flatten()
                    .as_deref(),
                row.try_get::<Option<String>, _>("blocked_reason")
                    .ok()
                    .flatten()
                    .as_deref(),
            )
        })
        .count() as i64;

    let failed: i64 =
        sqlx::query_scalar("SELECT COUNT(*)::BIGINT FROM kanban_cards WHERE status = 'failed'")
            .fetch_one(pool)
            .await
            .map_err(|error| format!("query postgres failed cards: {error}"))?;

    let waiting_acceptance: i64 =
        sqlx::query_scalar("SELECT COUNT(*)::BIGINT FROM kanban_cards WHERE status = 'requested'")
            .fetch_one(pool)
            .await
            .map_err(|error| format!("query postgres waiting_acceptance: {error}"))?;

    let stale_in_progress: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::BIGINT
           FROM kanban_cards kc
           LEFT JOIN task_dispatches td ON td.id = kc.latest_dispatch_id
          WHERE kc.status = 'in_progress'
            AND GREATEST(
                COALESCE(td.created_at, '-infinity'::timestamptz),
                COALESCE(kc.updated_at, '-infinity'::timestamptz),
                COALESCE(kc.started_at, '-infinity'::timestamptz)
            ) < NOW() - INTERVAL '100 minutes'",
    )
    .fetch_one(pool)
    .await
    .map_err(|error| format!("query postgres stale_in_progress: {error}"))?;

    let status_rows = sqlx::query(
        "SELECT status, COUNT(*)::BIGINT AS count
           FROM kanban_cards
          GROUP BY status",
    )
    .fetch_all(pool)
    .await
    .map_err(|error| format!("query postgres by_status: {error}"))?;
    let mut status_counts: HashMap<String, i64> = HashMap::new();
    for row in status_rows {
        if let Some(status) = row.try_get::<Option<String>, _>("status").ok().flatten() {
            status_counts.insert(
                status,
                row.try_get::<Option<i64>, _>("count")
                    .ok()
                    .flatten()
                    .unwrap_or(0),
            );
        }
    }

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
        by_status.insert(
            status.to_string(),
            json!(status_counts.get(*status).copied().unwrap_or(0)),
        );
    }

    let top_repo_rows = sqlx::query(
        "SELECT repo_id, COUNT(*)::BIGINT AS cnt
           FROM kanban_cards
          WHERE repo_id IS NOT NULL
            AND status NOT IN ('done', 'cancelled')
          GROUP BY repo_id
          ORDER BY cnt DESC, repo_id ASC
          LIMIT 5",
    )
    .fetch_all(pool)
    .await
    .map_err(|error| format!("query postgres top_repos: {error}"))?;
    let top_repos = top_repo_rows
        .into_iter()
        .map(|row| {
            let open_count = row.try_get::<Option<i64>, _>("cnt").ok().flatten().unwrap_or(0);
            json!({
                "github_repo": row.try_get::<Option<String>, _>("repo_id").ok().flatten().unwrap_or_default(),
                "open_count": open_count,
                "pressure_count": open_count,
            })
        })
        .collect::<Vec<_>>();

    Ok(json!({
        "open_total": open_total,
        "review_queue": review_queue,
        "blocked": blocked,
        "failed": failed,
        "waiting_acceptance": waiting_acceptance,
        "stale_in_progress": stale_in_progress,
        "by_status": by_status,
        "top_repos": top_repos,
    }))
}

async fn load_github_closed_today_pg(pool: &PgPool) -> Result<i64, String> {
    sqlx::query_scalar(
        "SELECT COUNT(*)::BIGINT
           FROM kanban_cards
          WHERE status = 'done'
            AND updated_at::date = CURRENT_DATE
            AND github_issue_url IS NOT NULL",
    )
    .fetch_one(pool)
    .await
    .map_err(|error| format!("query postgres github_closed_today: {error}"))
}

async fn get_stats_pg(pool: &PgPool, office_id: Option<&str>) -> Result<serde_json::Value, String> {
    let agent_rows = load_agent_stats_pg(pool, office_id).await?;
    let session_rows = load_working_session_rows_pg(pool, office_id).await?;

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
    for agent in &agent_rows {
        let effective_working = working_session_agents.contains(&agent.id)
            || agent.status.as_deref() == Some("working");
        if effective_working {
            working += 1;
            continue;
        }
        match agent.status.as_deref() {
            Some("break") => on_break += 1,
            Some("offline") => offline += 1,
            _ => idle += 1,
        }
    }

    let mut top_agents_src = agent_rows.clone();
    top_agents_src.sort_by(|a, b| b.xp.cmp(&a.xp).then_with(|| a.id.cmp(&b.id)));
    let top_agents = top_agents_src
        .into_iter()
        .take(10)
        .map(|agent| {
            json!({
                "id": agent.id,
                "name": agent.name,
                "name_ko": agent.name_ko,
                "avatar_emoji": agent.avatar_emoji,
                "sprite_number": agent.sprite_number,
                "stats_xp": agent.xp,
                "stats_tasks_done": agent.tasks_done,
                "stats_tokens": agent.tokens,
            })
        })
        .collect::<Vec<_>>();

    let departments =
        load_departments_pg(pool, office_id, &agent_rows, &working_session_agents).await?;
    let kanban = load_kanban_stats_pg(pool).await?;
    let github_closed_today = load_github_closed_today_pg(pool).await?;

    Ok(json!({
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
    }))
}

/// GET /api/stats
pub async fn get_stats(
    State(state): State<AppState>,
    Query(params): Query<StatsQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "postgres pool unavailable"})),
        );
    };
    match get_stats_pg(pool, params.office_id.as_deref()).await {
        Ok(body) => (StatusCode::OK, Json(body)),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": error})),
        ),
    }
}

/// GET /api/stats/memento
pub async fn get_memento_stats(
    Query(params): Query<MementoStatsQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    let hours = params.hours.unwrap_or(24).clamp(1, 24 * 7);
    (
        StatusCode::OK,
        Json(crate::services::memory::memento_call_metrics_snapshot(
            hours,
        )),
    )
}
