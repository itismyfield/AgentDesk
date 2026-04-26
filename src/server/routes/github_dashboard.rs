use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use serde::Deserialize;
use serde_json::json;
use sqlx::Row;

use super::AppState;
use crate::github;

// ── Query types ─────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct IssuesQuery {
    pub repo: Option<String>,
    pub state: Option<String>,
    pub limit: Option<u32>,
}

// ── GET /api/github-repos ───────────────────────────────────────

/// Dashboard-oriented repo list via `gh` CLI.
/// Returns `{ viewer_login, repos }`.
pub async fn list_repos(State(_state): State<AppState>) -> Json<serde_json::Value> {
    if !github::gh_available() {
        return Json(json!({
            "viewer_login": "unknown",
            "repos": [],
            "error": "gh CLI is not available"
        }));
    }

    // viewer login
    let viewer_login = match github::viewer_login() {
        Ok(value) => value,
        Err(_) => "unknown".to_string(),
    };

    // repos
    let repos = match github::list_dashboard_repos() {
        Ok(repos) => match serde_json::to_value(repos) {
            Ok(value) => value,
            Err(_) => serde_json::Value::Array(vec![]),
        },
        Err(_) => serde_json::Value::Array(vec![]),
    };

    Json(json!({
        "viewer_login": viewer_login,
        "repos": repos,
    }))
}

// ── GET /api/github-issues ──────────────────────────────────────

/// List GitHub issues for a given repo via `gh issue list`.
/// Query params: `repo` (required for results), `state` (default "open"), `limit` (default 20).
pub async fn list_issues(
    State(_state): State<AppState>,
    Query(params): Query<IssuesQuery>,
) -> Json<serde_json::Value> {
    let repo = match params.repo {
        Some(r) if !r.is_empty() => r,
        _ => {
            return Json(json!({
                "issues": [],
                "repo": "",
                "error": "query param 'repo' is required"
            }));
        }
    };

    if !github::gh_available() {
        return Json(json!({
            "issues": [],
            "repo": repo,
            "error": "gh CLI is not available"
        }));
    }

    let state_filter = params.state.unwrap_or_else(|| "open".to_string());
    let limit = params.limit.unwrap_or(20);

    let issues = match github::list_issue_summaries(&repo, &state_filter, limit) {
        Ok(entries) => match serde_json::to_value(entries) {
            Ok(value) => value,
            Err(_) => serde_json::Value::Array(vec![]),
        },
        Err(_) => serde_json::Value::Array(vec![]),
    };

    Json(json!({
        "issues": issues,
        "repo": repo,
    }))
}

// ── PATCH /api/github-issues/:owner/:repo/:number/close ─────────

/// Close a GitHub issue via `gh issue close`.
pub async fn close_issue(
    State(_state): State<AppState>,
    Path((owner, repo, number)): Path<(String, String, u64)>,
) -> (StatusCode, Json<serde_json::Value>) {
    let full_repo = format!("{owner}/{repo}");

    if !github::gh_available() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "gh CLI is not available"})),
        );
    }

    match github::close_issue(&full_repo, number as i64) {
        Ok(_) => (
            StatusCode::OK,
            Json(json!({
                "ok": true,
                "repo": full_repo,
                "number": number,
            })),
        ),
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({
                "ok": false,
                "error": e,
                "repo": full_repo,
                "number": number,
            })),
        ),
    }
}

// ── GET /api/github-closed-today ────────────────────────────────

/// Returns kanban cards marked "done" today that have a github_issue_url.
pub async fn closed_today(State(state): State<AppState>) -> Json<serde_json::Value> {
    let Some(pool) = state.pg_pool_ref() else {
        return Json(json!({
            "count": 0,
            "issues": [],
            "error": "postgres pool unavailable"
        }));
    };

    let rows = match sqlx::query(
        "SELECT id, title, github_issue_url, github_issue_number, updated_at::text AS updated_at
         FROM kanban_cards
         WHERE status = 'done'
           AND github_issue_url IS NOT NULL
           AND updated_at::date = CURRENT_DATE
         ORDER BY updated_at DESC",
    )
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows,
        Err(e) => {
            return Json(json!({
                "count": 0,
                "issues": [],
                "error": format!("query: {e}")
            }));
        }
    };

    let rows: Vec<serde_json::Value> = rows
        .into_iter()
        .map(|row| {
            json!({
                "id": row.try_get::<String, _>("id").unwrap_or_default(),
                "title": row.try_get::<String, _>("title").unwrap_or_default(),
                "github_issue_url": row.try_get::<Option<String>, _>("github_issue_url").ok().flatten(),
                "github_issue_number": row.try_get::<Option<i64>, _>("github_issue_number").ok().flatten(),
                "updated_at": row.try_get::<Option<String>, _>("updated_at").ok().flatten(),
            })
        })
        .collect();

    let count = rows.len();
    Json(json!({
        "count": count,
        "issues": rows,
    }))
}
