use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use serde::Deserialize;
use serde_json::json;
use sqlx::Row;
use std::collections::BTreeSet;

use super::AppState;
use crate::github;
use crate::services::github_issue_creation::{
    GitHubIssueCreateRequest, IssueAnnouncementSync, IssueAnnouncementSyncOptions,
    IssueCreationError, KanbanCardSync, KanbanCardSyncOptions,
    create_github_issue_with_side_effects, resolve_known_agent_id_pg,
};

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
    pub announcement_channel_id: Option<String>,
    pub dry_run: Option<bool>,
}

const ISSUE_FORMAT_VERSION: u32 = 1;
const ISSUE_CREATE_UNSUPPORTED_FEATURES: &[&str] = &["auto_dispatch"];

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

fn normalize_block_on_issue_numbers(body: &CreateIssueBody) -> Result<Vec<i64>, String> {
    let Some(items) = body.block_on.as_ref() else {
        return Ok(Vec::new());
    };

    let mut numbers = BTreeSet::new();
    for issue_number in items {
        if *issue_number <= 0 {
            return Err("block_on issue numbers must be positive".to_string());
        }
        numbers.insert(*issue_number);
    }
    Ok(numbers.into_iter().collect())
}

fn issue_metadata_json(labels: &[String], block_on_issue_numbers: &[i64]) -> Option<String> {
    let labels = labels
        .iter()
        .filter_map(|label| trim_non_empty(label))
        .collect::<Vec<_>>();

    if labels.is_empty() && block_on_issue_numbers.is_empty() {
        None
    } else {
        let mut metadata = serde_json::Map::new();
        if !labels.is_empty() {
            metadata.insert("labels".to_string(), json!(labels.join(",")));
        }
        if !block_on_issue_numbers.is_empty() {
            metadata.insert("depends_on".to_string(), json!(block_on_issue_numbers));
        }
        Some(serde_json::Value::Object(metadata).to_string())
    }
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

    let mut dependencies = body
        .dependencies
        .as_deref()
        .map(|items| {
            items
                .iter()
                .filter_map(render_dependency_line)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let block_on_issue_numbers = normalize_block_on_issue_numbers(body)?;
    dependencies.extend(
        block_on_issue_numbers
            .into_iter()
            .map(|issue_number| format!("- #{issue_number}")),
    );
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

fn collect_issue_body_validation_errors(body: &CreateIssueBody) -> Vec<String> {
    let mut errors = Vec::new();
    if trim_non_empty(&body.background).is_none() {
        errors.push("background is required".to_string());
    }
    if normalize_string_list(&body.content).is_empty() {
        errors.push("content must contain at least one item".to_string());
    }
    let dod = normalize_string_list(&body.dod);
    if dod.is_empty() {
        errors.push("dod must contain at least one item".to_string());
    } else if dod.len() > 10 {
        errors.push("dod items must be 10 or fewer".to_string());
    }
    if let Err(error) = normalize_block_on_issue_numbers(body) {
        errors.push(error);
    }
    errors
}

fn issue_create_capabilities() -> serde_json::Value {
    json!({
        "auto_dispatch": false,
        "block_on": true,
        "unsupported_features": ISSUE_CREATE_UNSUPPORTED_FEATURES,
    })
}

fn requested_unsupported_issue_create_features(body: &CreateIssueBody) -> Vec<&'static str> {
    let mut features = Vec::new();
    if body.auto_dispatch.unwrap_or(false) {
        features.push("auto_dispatch");
    }
    features
}

fn unsupported_issue_create_warnings(features: &[&str]) -> Vec<String> {
    features
        .iter()
        .map(|feature| {
            format!("{feature} is reserved and unsupported; non-dry-run issue creation rejects it")
        })
        .collect()
}

fn unsupported_issue_create_error(features: &[&str]) -> String {
    format!(
        "unsupported reserved issue-create field(s): {}; send dry_run=true to inspect capabilities before creating an issue",
        features.join(", ")
    )
}

fn issue_validation_error(error: String, dry_run: bool) -> (StatusCode, Json<serde_json::Value>) {
    if dry_run {
        let warning = error.clone();
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({
                "dry_run": true,
                "error": error,
                "validation_warnings": [warning],
                "capabilities": issue_create_capabilities(),
                "unsupported_features": [],
            })),
        )
    } else {
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"error": error})),
        )
    }
}

// ── Handlers ───────────────────────────────────────────────────

/// POST /api/github/issues/create
pub async fn create_issue(
    State(state): State<AppState>,
    Json(body): Json<CreateIssueBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let dry_run = body.dry_run.unwrap_or(false);
    let unsupported_features = requested_unsupported_issue_create_features(&body);
    if !dry_run && !unsupported_features.is_empty() {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({
                "error": unsupported_issue_create_error(&unsupported_features),
                "capabilities": issue_create_capabilities(),
                "unsupported_features": unsupported_features,
            })),
        );
    }

    if dry_run {
        let unsupported_feature_warnings = unsupported_issue_create_warnings(&unsupported_features);
        let mut validation_warnings = Vec::new();
        let repo = match resolve_issue_repo(&body.repo) {
            Ok(repo) => Some(repo),
            Err(error) => {
                validation_warnings.push(error);
                None
            }
        };
        if trim_non_empty(&body.title).is_none() {
            validation_warnings.push("title is required".to_string());
        }
        validation_warnings.extend(collect_issue_body_validation_errors(&body));
        let block_on_issue_numbers = normalize_block_on_issue_numbers(&body).unwrap_or_default();

        if !validation_warnings.is_empty() {
            let error = validation_warnings
                .first()
                .cloned()
                .unwrap_or_else(|| "validation failed".to_string());
            validation_warnings.extend(unsupported_feature_warnings);
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({
                    "dry_run": true,
                    "error": error,
                    "validation_warnings": validation_warnings,
                    "capabilities": issue_create_capabilities(),
                    "unsupported_features": unsupported_features,
                    "block_on": block_on_issue_numbers,
                })),
            );
        }

        let repo = repo.expect("validated repo must exist");
        let issue_body = build_pmd_issue_body(&body).expect("validated issue body must render");
        let applied_labels = body
            .agent_id
            .as_deref()
            .and_then(trim_non_empty)
            .map(|agent_id| vec![format!("agent:{agent_id}")])
            .unwrap_or_default();
        if let (Some(pool), Some(agent_id)) = (
            state.pg_pool_ref(),
            body.agent_id.as_deref().and_then(trim_non_empty),
        ) {
            match resolve_known_agent_id_pg(pool, Some(&agent_id)).await {
                Ok(Some(_)) => {}
                Ok(None) => {
                    validation_warnings.push(format!("unknown agent_id: {agent_id}"));
                }
                Err(error) => {
                    validation_warnings.push(format!("agent_id validation failed: {error}"));
                }
            }
        }
        validation_warnings.extend(unsupported_feature_warnings);
        let announcement_channel_id = body
            .announcement_channel_id
            .as_deref()
            .and_then(trim_non_empty);

        return (
            StatusCode::OK,
            Json(json!({
                "dry_run": true,
                "issue": {
                    "number": null,
                    "url": null,
                    "repo": repo,
                },
                "kanban_card_id": null,
                "kanban_card_sync_error": null,
                "announcement_channel_id": announcement_channel_id,
                "announcement_message_id": null,
                "announcement_sync_error": null,
                "applied_labels": applied_labels,
                "rendered_body": issue_body,
                "validation_warnings": validation_warnings,
                "capabilities": issue_create_capabilities(),
                "unsupported_features": unsupported_features,
                "block_on": block_on_issue_numbers,
                "issue_format_version": ISSUE_FORMAT_VERSION,
                // deprecated alias kept for transition; remove after clients migrate
                "pmd_format_version": ISSUE_FORMAT_VERSION,
            })),
        );
    }

    let repo = match resolve_issue_repo(&body.repo) {
        Ok(repo) => repo,
        Err(error) => return issue_validation_error(error, dry_run),
    };
    let title = match trim_non_empty(&body.title) {
        Some(title) => title,
        None => return issue_validation_error("title is required".to_string(), dry_run),
    };
    let issue_body = match build_pmd_issue_body(&body) {
        Ok(issue_body) => issue_body,
        Err(error) => return issue_validation_error(error, dry_run),
    };
    let block_on_issue_numbers = match normalize_block_on_issue_numbers(&body) {
        Ok(block_on_issue_numbers) => block_on_issue_numbers,
        Err(error) => return issue_validation_error(error, dry_run),
    };

    let applied_labels = body
        .agent_id
        .as_deref()
        .and_then(trim_non_empty)
        .map(|agent_id| vec![format!("agent:{agent_id}")])
        .unwrap_or_default();

    let issue_create_request = GitHubIssueCreateRequest::new(
        "api_github_issues_create",
        repo.clone(),
        title.clone(),
        issue_body.clone(),
    )
    .with_labels(applied_labels.clone())
    .with_kanban(KanbanCardSync::enabled(KanbanCardSyncOptions {
        agent_id: body.agent_id.as_deref().and_then(trim_non_empty),
        metadata_json: issue_metadata_json(&applied_labels, &block_on_issue_numbers),
        status_on_create: Some("backlog".to_string()),
    }))
    .with_announcement(IssueAnnouncementSync::enabled(
        IssueAnnouncementSyncOptions {
            agent_id: body.agent_id.as_deref().and_then(trim_non_empty),
            announcement_channel_id: body
                .announcement_channel_id
                .as_deref()
                .and_then(trim_non_empty),
            complete_if_closed: true,
        },
    ));

    match create_github_issue_with_side_effects(state.pg_pool_ref(), issue_create_request).await {
        Ok(result) => {
            let issue = result.issue;
            let kanban = result.kanban;
            let announcement = result.announcement;
            let kanban_card_id = kanban.card_id.clone();
            let kanban_card_sync_error = kanban.error.clone();
            let announcement_channel_id = announcement.channel_id.clone();
            let announcement_message_id = announcement.message_id.clone();
            let announcement_sync_error = announcement.error.clone();

            (
                StatusCode::CREATED,
                Json(json!({
                    "issue": {
                        "number": issue.number,
                        "url": issue.url,
                        "repo": repo,
                    },
                    "kanban_card_id": kanban_card_id,
                    "kanban_card_sync_error": kanban_card_sync_error,
                    "announcement_channel_id": announcement_channel_id,
                    "announcement_message_id": announcement_message_id,
                    "announcement_sync_error": announcement_sync_error,
                    "applied_labels": result.applied_labels,
                    "block_on": block_on_issue_numbers,
                    "issue_creation_origin": result.origin,
                    "issue_creation_mode": result.mode,
                    "kanban_card_sync": kanban,
                    "announcement_sync": announcement,
                    "issue_format_version": ISSUE_FORMAT_VERSION,
                    // deprecated alias kept for transition; remove after clients migrate
                    "pmd_format_version": ISSUE_FORMAT_VERSION,
                })),
            )
        }
        Err(IssueCreationError::GhUnavailable) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "gh CLI is not available on this system"})),
        ),
        Err(IssueCreationError::GitHub(error)) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({"error": format!("gh issue create failed: {error}")})),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn test_state() -> AppState {
        let config = crate::config::Config::default();
        let tx = crate::server::ws::new_broadcast();
        let buf = crate::server::ws::spawn_batch_flusher(tx.clone());
        AppState {
            pg_pool: None,
            engine: crate::engine::PolicyEngine::new(&config).expect("test policy engine"),
            config: Arc::new(config),
            broadcast_tx: tx,
            batch_buffer: buf,
            health_registry: None,
            cluster_instance_id: None,
        }
    }

    fn base_issue_body() -> CreateIssueBody {
        CreateIssueBody {
            repo: "ADK".to_string(),
            title: "Test issue".to_string(),
            background: "Background".to_string(),
            content: vec!["Do the thing".to_string()],
            dod: vec!["It works".to_string()],
            agent_id: None,
            dependencies: None,
            risks: None,
            hints: None,
            auto_dispatch: None,
            block_on: None,
            announcement_channel_id: None,
            dry_run: None,
        }
    }

    #[tokio::test]
    async fn create_issue_dry_run_reports_reserved_feature_capabilities_without_failing() {
        let mut body = base_issue_body();
        body.auto_dispatch = Some(true);
        body.block_on = Some(vec![3718]);
        body.dry_run = Some(true);

        let (status, Json(response)) = create_issue(State(test_state()), Json(body)).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(response["dry_run"], true);
        assert_eq!(response["unsupported_features"], json!(["auto_dispatch"]));
        assert_eq!(response["block_on"], json!([3718]));
        assert_eq!(response["capabilities"]["auto_dispatch"], false);
        assert_eq!(response["capabilities"]["block_on"], true);
        assert_eq!(
            response["capabilities"]["unsupported_features"],
            json!(["auto_dispatch"])
        );
        assert!(
            response["validation_warnings"]
                .as_array()
                .expect("warnings must be an array")
                .iter()
                .any(|warning| warning
                    .as_str()
                    .is_some_and(|message| message.contains("auto_dispatch is reserved"))),
            "dry_run must expose unsupported reserved-field warnings: {response}"
        );
        assert!(response["rendered_body"].is_string());
    }

    #[test]
    fn block_on_renders_dependency_section_and_kanban_metadata() {
        let mut body = base_issue_body();
        body.agent_id = Some("project-agentdesk".to_string());
        body.block_on = Some(vec![42, 7, 42]);

        let rendered = build_pmd_issue_body(&body).expect("issue body should render");
        assert!(rendered.contains("## 의존성"));
        assert!(rendered.contains("- #7"));
        assert!(rendered.contains("- #42"));

        let dependencies =
            normalize_block_on_issue_numbers(&body).expect("block_on should normalize");
        assert_eq!(dependencies, vec![7, 42]);
        let metadata = issue_metadata_json(&["agent:project-agentdesk".to_string()], &dependencies)
            .expect("metadata should exist");
        let metadata: serde_json::Value =
            serde_json::from_str(&metadata).expect("metadata should be JSON");
        assert_eq!(metadata["labels"], "agent:project-agentdesk");
        assert_eq!(metadata["depends_on"], json!([7, 42]));
    }

    #[test]
    fn block_on_rejects_non_positive_issue_numbers() {
        let mut body = base_issue_body();
        body.block_on = Some(vec![0]);

        assert_eq!(
            normalize_block_on_issue_numbers(&body),
            Err("block_on issue numbers must be positive".to_string())
        );
        assert_eq!(
            build_pmd_issue_body(&body),
            Err("block_on issue numbers must be positive".to_string())
        );
    }

    #[tokio::test]
    async fn create_issue_rejects_reserved_fields_without_gh_side_effects() {
        let mut body = base_issue_body();
        body.auto_dispatch = Some(true);

        let (status, Json(response)) = create_issue(State(test_state()), Json(body)).await;

        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(response["unsupported_features"], json!(["auto_dispatch"]));
        assert_eq!(response["capabilities"]["auto_dispatch"], false);
        assert!(
            response["error"]
                .as_str()
                .is_some_and(|message| message.contains("unsupported reserved issue-create")),
            "non-dry-run unsupported fields must fail as a contract error: {response}"
        );
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
            "stale_card_issue_checks": sync_result.stale_card_issue_check_count,
            "stale_card_issue_batches": sync_result.stale_card_issue_batch_count,
            "stale_card_issue_errors": sync_result.stale_card_issue_error_count,
        })),
    )
}
