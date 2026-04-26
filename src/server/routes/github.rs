use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use serde::Deserialize;
use serde_json::json;
use sqlx::{PgPool, Row};

use super::AppState;
use crate::db::kanban::{IssueCardUpsert, upsert_card_from_issue_pg};
use crate::github;

// ── Body types ─────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct RegisterRepoBody {
    pub id: String,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum IssueDependencyInput {
    IssueNumber(i64),
    Reference(String),
    Detailed {
        issue_number: i64,
        description: Option<String>,
    },
}

#[derive(Debug, Deserialize)]
pub struct CreateIssueBody {
    pub repo: String,
    pub title: String,
    pub background: String,
    pub content: Vec<String>,
    pub dod: Vec<String>,
    pub agent_id: Option<String>,
    pub dependencies: Option<Vec<IssueDependencyInput>>,
    pub risks: Option<Vec<String>>,
    pub hints: Option<Vec<String>>,
    pub auto_dispatch: Option<bool>,
    pub block_on: Option<Vec<i64>>,
}

const PMD_FORMAT_VERSION: u32 = 1;

fn trim_non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn normalize_string_list(values: &[String]) -> Vec<String> {
    values
        .iter()
        .filter_map(|value| trim_non_empty(value))
        .collect()
}

fn labels_metadata_json(labels: &[String]) -> Option<String> {
    let labels = labels
        .iter()
        .filter_map(|label| trim_non_empty(label))
        .collect::<Vec<_>>();

    if labels.is_empty() {
        None
    } else {
        Some(json!({ "labels": labels.join(",") }).to_string())
    }
}

async fn resolve_known_agent_id_pg(
    pool: &PgPool,
    agent_id: Option<&str>,
) -> Result<Option<String>, String> {
    let Some(agent_id) = agent_id.and_then(trim_non_empty) else {
        return Ok(None);
    };

    let exists = sqlx::query_scalar::<_, String>("SELECT id FROM agents WHERE id = $1 LIMIT 1")
        .bind(&agent_id)
        .fetch_optional(pool)
        .await
        .map_err(|error| format!("resolve agent {agent_id}: {error}"))?;

    if exists.is_none() {
        tracing::warn!("[issues] ignoring unknown assignee '{agent_id}' for linked kanban card");
    }

    Ok(exists)
}

fn resolve_issue_repo(input: &str) -> Result<String, String> {
    let repo = input.trim();
    if repo.is_empty() {
        return Err("repo is required".to_string());
    }

    match repo.to_ascii_uppercase().as_str() {
        "ADK" => Ok("itismyfield/AgentDesk".to_string()),
        "CH" => Ok("itismyfield/CookingHeart".to_string()),
        _ if repo.contains('/') => Ok(repo.to_string()),
        _ => Err("repo must be ADK, CH, or owner/repo".to_string()),
    }
}

fn render_dependency_line(value: &IssueDependencyInput) -> Option<String> {
    match value {
        IssueDependencyInput::IssueNumber(issue_number) => {
            (*issue_number > 0).then(|| format!("- #{issue_number}"))
        }
        IssueDependencyInput::Reference(reference) => {
            trim_non_empty(reference).map(|reference| format!("- {reference}"))
        }
        IssueDependencyInput::Detailed {
            issue_number,
            description,
        } => {
            if *issue_number <= 0 {
                return None;
            }
            let suffix = description
                .as_deref()
                .and_then(trim_non_empty)
                .map(|description| format!(" ({description})"))
                .unwrap_or_default();
            Some(format!("- #{issue_number}{suffix}"))
        }
    }
}

fn build_pmd_issue_body(body: &CreateIssueBody) -> Result<String, String> {
    let background =
        trim_non_empty(&body.background).ok_or_else(|| "background is required".to_string())?;
    let content = normalize_string_list(&body.content);
    if content.is_empty() {
        return Err("content must contain at least one item".to_string());
    }
    let dod = normalize_string_list(&body.dod);
    if dod.is_empty() {
        return Err("dod must contain at least one item".to_string());
    }
    if dod.len() > 10 {
        return Err("dod items must be 10 or fewer".to_string());
    }

    let dependencies = body
        .dependencies
        .as_deref()
        .map(|items| {
            items
                .iter()
                .filter_map(render_dependency_line)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let risks = body
        .risks
        .as_deref()
        .map(normalize_string_list)
        .unwrap_or_default();
    let hints = body
        .hints
        .as_deref()
        .map(normalize_string_list)
        .unwrap_or_default();

    let mut lines = vec![
        "## 배경".to_string(),
        background,
        String::new(),
        "## 내용".to_string(),
    ];
    lines.extend(content.into_iter().map(|item| format!("- {item}")));

    if !dependencies.is_empty() {
        lines.push(String::new());
        lines.push("## 의존성".to_string());
        lines.extend(dependencies);
    }

    if !risks.is_empty() {
        lines.push(String::new());
        lines.push("## 리스크".to_string());
        lines.extend(risks.into_iter().map(|risk| format!("- {risk}")));
    }

    if !hints.is_empty() {
        lines.push(String::new());
        lines.push("## 착수 힌트".to_string());
        lines.push(
            "> ⚠️ 이 힌트는 참고용이며 전적으로 의존하지 마세요. 반드시 직접 코드를 확인한 후 작업하세요."
                .to_string(),
        );
        lines.push(String::new());
        lines.extend(hints.into_iter().map(|hint| format!("- {hint}")));
    }

    lines.push(String::new());
    lines.push("## DoD".to_string());
    lines.extend(dod.into_iter().map(|item| format!("- [ ] {item}")));

    Ok(lines.join("\n"))
}

// ── Handlers ───────────────────────────────────────────────────

/// POST /api/issues
pub async fn create_issue(
    State(state): State<AppState>,
    Json(body): Json<CreateIssueBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    if body.auto_dispatch.unwrap_or(false) {
        return (
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error": "auto_dispatch is not implemented yet"})),
        );
    }
    if body
        .block_on
        .as_ref()
        .is_some_and(|items| !items.is_empty())
    {
        return (
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error": "block_on is not implemented yet"})),
        );
    }

    let repo = match resolve_issue_repo(&body.repo) {
        Ok(repo) => repo,
        Err(error) => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({"error": error})),
            );
        }
    };
    let title = match trim_non_empty(&body.title) {
        Some(title) => title,
        None => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({"error": "title is required"})),
            );
        }
    };
    let issue_body = match build_pmd_issue_body(&body) {
        Ok(issue_body) => issue_body,
        Err(error) => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({"error": error})),
            );
        }
    };

    if !github::gh_available() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "gh CLI is not available on this system"})),
        );
    }

    let applied_labels = body
        .agent_id
        .as_deref()
        .and_then(trim_non_empty)
        .map(|agent_id| vec![format!("agent:{agent_id}")])
        .unwrap_or_default();

    match github::create_issue_with_labels(&repo, &title, &issue_body, &applied_labels).await {
        Ok(created) => {
            let metadata_json = labels_metadata_json(&applied_labels);
            let (kanban_card_id, kanban_card_sync_error) = if let Some(pool) = state.pg_pool_ref() {
                let assigned_agent_id = match resolve_known_agent_id_pg(
                    pool,
                    body.agent_id.as_deref(),
                )
                .await
                {
                    Ok(agent_id) => agent_id,
                    Err(error) => {
                        tracing::error!(
                            "[issues] created GitHub issue {}#{} but failed to resolve assignee: {}",
                            repo,
                            created.number,
                            error
                        );
                        return (
                            StatusCode::CREATED,
                            Json(json!({
                                "issue": {
                                    "number": created.number,
                                    "url": created.url,
                                    "repo": repo,
                                },
                                "kanban_card_id": serde_json::Value::Null,
                                "kanban_card_sync_error": error,
                                "applied_labels": applied_labels,
                                "pmd_format_version": PMD_FORMAT_VERSION,
                            })),
                        );
                    }
                };
                match upsert_card_from_issue_pg(
                    pool,
                    IssueCardUpsert {
                        repo_id: repo.clone(),
                        issue_number: created.number,
                        issue_url: Some(created.url.clone()),
                        title: title.clone(),
                        description: Some(issue_body.clone()),
                        priority: None,
                        assigned_agent_id,
                        metadata_json: metadata_json.clone(),
                        status_on_create: Some("backlog".to_string()),
                    },
                )
                .await
                {
                    Ok(upserted) => (Some(upserted.card_id), None),
                    Err(error) => {
                        tracing::error!(
                            "[issues] created GitHub issue {}#{} but failed to sync kanban card: {}",
                            repo,
                            created.number,
                            error
                        );
                        (None, Some(error))
                    }
                }
            } else {
                (None, Some("postgres pool unavailable".to_string()))
            };

            (
                StatusCode::CREATED,
                Json(json!({
                    "issue": {
                        "number": created.number,
                        "url": created.url,
                        "repo": repo,
                    },
                    "kanban_card_id": kanban_card_id,
                    "kanban_card_sync_error": kanban_card_sync_error,
                    "applied_labels": applied_labels,
                    "pmd_format_version": PMD_FORMAT_VERSION,
                })),
            )
        }
        Err(error) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({"error": format!("gh issue create failed: {error}")})),
        ),
    }
}

/// GET /api/github/repos
pub async fn list_repos(State(state): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "postgres pool unavailable"})),
        );
    };
    let rows = match sqlx::query(
        "SELECT id, display_name, sync_enabled, last_synced_at::text AS last_synced_at
         FROM github_repos
         ORDER BY id",
    )
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{error}")})),
            );
        }
    };
    let items: Vec<serde_json::Value> = rows
        .into_iter()
        .map(|row| {
            json!({
                "id": row.try_get::<String, _>("id").unwrap_or_default(),
                "display_name": row.try_get::<Option<String>, _>("display_name").ok().flatten(),
                "sync_enabled": row.try_get::<Option<bool>, _>("sync_enabled").ok().flatten().unwrap_or(true),
                "last_synced_at": row.try_get::<Option<String>, _>("last_synced_at").ok().flatten(),
            })
        })
        .collect();
    (StatusCode::OK, Json(json!({"repos": items})))
}

/// POST /api/github/repos
pub async fn register_repo(
    State(state): State<AppState>,
    Json(body): Json<RegisterRepoBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    if body.id.is_empty() || !body.id.contains('/') {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "id must be in 'owner/repo' format"})),
        );
    }

    let Some(pool) = state.pg_pool_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "postgres pool unavailable"})),
        );
    };
    if let Err(error) = crate::db::postgres::register_repo(pool, &body.id).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": error})),
        );
    }

    match sqlx::query(
        "SELECT id, display_name, sync_enabled, last_synced_at::text AS last_synced_at
         FROM github_repos
         WHERE id = $1",
    )
    .bind(&body.id)
    .fetch_one(pool)
    .await
    {
        Ok(row) => (
            StatusCode::CREATED,
            Json(json!({
                "repo": {
                    "id": row.try_get::<String, _>("id").unwrap_or_default(),
                    "display_name": row.try_get::<Option<String>, _>("display_name").ok().flatten(),
                    "sync_enabled": row.try_get::<Option<bool>, _>("sync_enabled").ok().flatten().unwrap_or(true),
                    "last_synced_at": row.try_get::<Option<String>, _>("last_synced_at").ok().flatten(),
                }
            })),
        ),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{error}")})),
        ),
    }
}

/// POST /api/github/repos/:owner/:repo/sync
pub async fn sync_repo(
    State(state): State<AppState>,
    Path((owner, repo)): Path<(String, String)>,
) -> (StatusCode, Json<serde_json::Value>) {
    let repo_id = format!("{owner}/{repo}");

    // Check repo exists
    let Some(pool) = state.pg_pool_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "postgres pool unavailable"})),
        );
    };
    let exists =
        match sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM github_repos WHERE id = $1")
            .bind(&repo_id)
            .fetch_one(pool)
            .await
        {
            Ok(count) => count > 0,
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{error}")})),
                );
            }
        };

    if !exists {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": format!("repo '{}' not registered", repo_id)})),
        );
    }

    // Check if gh is available
    if !github::gh_available() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "gh CLI is not available on this system"})),
        );
    }

    // Fetch issues
    let issues = match github::sync::fetch_issues(&repo_id) {
        Ok(i) => i,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({"error": format!("gh fetch failed: {e}")})),
            );
        }
    };

    let triaged = match github::triage::triage_new_issues_pg(pool, &repo_id, &issues).await {
        Ok(count) => count,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("triage failed: {error}")})),
            );
        }
    };
    let sync_result =
        match github::sync::sync_github_issues_for_repo_pg(pool, &repo_id, &issues).await {
            Ok(result) => result,
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("sync failed: {error}")})),
                );
            }
        };

    (
        StatusCode::OK,
        Json(json!({
            "synced": true,
            "repo": repo_id,
            "issues_fetched": issues.len(),
            "cards_created": triaged,
            "cards_closed": sync_result.closed_count,
            "inconsistencies": sync_result.inconsistency_count,
        })),
    )
}
