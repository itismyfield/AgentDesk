use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use serde::Deserialize;
use serde_json::json;

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
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return Json(json!({
                "count": 0,
                "issues": [],
                "error": format!("db lock: {e}")
            }));
        }
    };

    let mut stmt = match conn.prepare(
        "SELECT id, title, github_issue_url, github_issue_number, updated_at
         FROM kanban_cards
         WHERE status = 'done'
           AND github_issue_url IS NOT NULL
           AND date(updated_at) = date('now')
         ORDER BY updated_at DESC",
    ) {
        Ok(s) => s,
        Err(e) => {
            return Json(json!({
                "count": 0,
                "issues": [],
                "error": format!("prepare: {e}")
            }));
        }
    };

    let rows: Vec<serde_json::Value> = stmt
        .query_map([], |row| {
            Ok(json!({
                "id": row.get::<_, String>(0)?,
                "title": row.get::<_, String>(1)?,
                "github_issue_url": row.get::<_, Option<String>>(2)?,
                "github_issue_number": row.get::<_, Option<i64>>(3)?,
                "updated_at": row.get::<_, Option<String>>(4)?,
            }))
        })
        .ok()
        .map(|iter| iter.filter_map(|r| r.ok()).collect())
        .unwrap_or_default();

    let count = rows.len();
    Json(json!({
        "count": count,
        "issues": rows,
    }))
}
