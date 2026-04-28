use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use regex::Regex;
use serde_json::{Value, json};
use sqlx::Row as _;
use std::collections::BTreeSet;
use std::future::Future;
use std::process::Command;
use std::sync::{Arc, OnceLock};

use crate::server::routes::AppState;

pub(crate) fn run_async<F>(future: F) -> Result<(), String>
where
    F: Future<Output = Result<(), String>>,
{
    let runtime = tokio::runtime::Runtime::new().map_err(|e| format!("tokio runtime: {e}"))?;
    runtime.block_on(future)
}

fn print_json(value: &Value) {
    println!(
        "{}",
        serde_json::to_string_pretty(value).unwrap_or_else(|_| "{}".to_string())
    );
}

fn extract_error_message(value: &Value) -> Option<String> {
    value
        .get("error")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|msg| !msg.is_empty())
        .map(str::to_string)
        .or_else(|| {
            value
                .get("message")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|msg| !msg.is_empty())
                .map(str::to_string)
        })
}

fn route_json(status: StatusCode, Json(value): Json<Value>) -> Result<Value, String> {
    if status.is_success() {
        Ok(value)
    } else {
        Err(extract_error_message(&value)
            .unwrap_or_else(|| format!("request failed ({})", status.as_u16())))
    }
}

fn parse_health_status_code(status: &str) -> StatusCode {
    match status {
        "200 OK" => StatusCode::OK,
        "400 Bad Request" => StatusCode::BAD_REQUEST,
        "403 Forbidden" => StatusCode::FORBIDDEN,
        "404 Not Found" => StatusCode::NOT_FOUND,
        "500 Internal Server Error" => StatusCode::INTERNAL_SERVER_ERROR,
        "503 Service Unavailable" => StatusCode::SERVICE_UNAVAILABLE,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

fn maybe_infer_repo_from_git() -> Option<String> {
    let repo_dir = crate::services::platform::resolve_repo_dir()?;
    let output = Command::new("git")
        .args(["config", "--get", "remote.origin.url"])
        .current_dir(repo_dir)
        .output()
        .ok()
        .filter(|output| output.status.success())?;
    crate::services::platform::shell::parse_github_repo_from_remote(&String::from_utf8_lossy(
        &output.stdout,
    ))
}

fn split_repo(repo: &str) -> Result<(String, String), String> {
    let trimmed = repo.trim();
    let Some((owner, name)) = trimmed.split_once('/') else {
        return Err(format!(
            "repo must be in owner/repo format, got '{trimmed}'"
        ));
    };
    if owner.is_empty() || name.is_empty() {
        return Err(format!(
            "repo must be in owner/repo format, got '{trimmed}'"
        ));
    }
    Ok((owner.to_string(), name.to_string()))
}

async fn load_repo_candidates_pg(pool: &sqlx::PgPool) -> Vec<String> {
    crate::github::list_repos_pg(pool)
        .await
        .map(|repos| repos.into_iter().map(|repo| repo.id).collect())
        .unwrap_or_default()
}

async fn resolve_repo_arg_pg(
    provided: Option<&str>,
    pool: &sqlx::PgPool,
) -> Result<String, String> {
    if let Some(repo) = provided.map(str::trim).filter(|repo| !repo.is_empty()) {
        return Ok(repo.to_string());
    }
    if let Some(repo) = maybe_infer_repo_from_git() {
        return Ok(repo);
    }

    let repos = load_repo_candidates_pg(pool).await;
    if repos.len() == 1 {
        return Ok(repos[0].clone());
    }

    Err("repo could not be inferred; pass --repo owner/repo".to_string())
}

async fn repo_default_agent_id_pg(pool: &sqlx::PgPool, repo: &str) -> Option<String> {
    sqlx::query_scalar::<_, Option<String>>(
        "SELECT default_agent_id FROM github_repos WHERE id = $1",
    )
    .bind(repo)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
    .flatten()
    .filter(|agent_id| !agent_id.trim().is_empty())
}

fn resolve_repo_dir(repo_id: Option<&str>) -> Result<Option<String>, String> {
    crate::services::platform::resolve_repo_dir_for_id(repo_id)
}

async fn build_app_state(with_health_registry: bool) -> Result<AppState, String> {
    let runtime_root = crate::config::runtime_root();

    if let Some(root) = runtime_root.as_ref() {
        crate::runtime_layout::ensure_runtime_layout(root)
            .map_err(|e| format!("prepare runtime layout: {e}"))?;
    }

    let legacy_scan = runtime_root
        .as_ref()
        .map(|root| crate::services::discord_config_audit::scan_legacy_sources(root))
        .unwrap_or_default();

    let loaded = if let Some(root) = runtime_root.as_ref() {
        crate::services::discord_config_audit::load_runtime_config(root)
            .map_err(|e| format!("load runtime config: {e}"))?
    } else {
        let config = crate::config::load().map_err(|e| format!("load config: {e}"))?;
        crate::services::discord_config_audit::LoadedRuntimeConfig {
            config,
            path: std::path::PathBuf::from("config/agentdesk.yaml"),
            existed: true,
        }
    };

    let mut config = if let Some(root) = runtime_root.as_ref() {
        crate::services::discord_config_audit::audit_and_reconcile_config_only(
            root,
            loaded.config,
            loaded.path,
            loaded.existed,
            &legacy_scan,
            false,
        )
        .map_err(|e| format!("audit runtime config: {e}"))?
        .config
    } else {
        loaded.config
    };

    let pipeline_path = config.policies.dir.join("default-pipeline.yaml");
    if pipeline_path.exists() {
        match crate::pipeline::load(&pipeline_path) {
            Ok(()) => {}
            Err(error) if error.to_string().contains("already loaded") => {}
            Err(error) => {
                return Err(format!(
                    "load pipeline {}: {error}",
                    pipeline_path.display()
                ));
            }
        }
    } else {
        crate::pipeline::ensure_loaded();
    }

    let pg_pool = crate::db::postgres::connect_and_migrate(&config).await?;
    if let Some(root) = runtime_root.as_ref() {
        let loaded = crate::services::discord_config_audit::load_runtime_config(root)
            .map_err(|e| format!("reload runtime config after pg migration: {e}"))?;
        config = crate::services::discord_config_audit::audit_and_reconcile_config_only(
            root,
            loaded.config,
            loaded.path,
            loaded.existed,
            &legacy_scan,
            false,
        )
        .map_err(|e| format!("persist runtime config audit after pg migration: {e}"))?
        .config;
    }
    if let Some(pool) = pg_pool.as_ref() {
        crate::db::postgres::startup_reseed(pool, &config)
            .await
            .map_err(|e| format!("startup reseed: {e}"))?;
    }
    crate::pipeline::refresh_override_health_report(pg_pool.as_ref()).await;
    crate::services::termination_audit::init_audit_db(pg_pool.clone());
    let engine = crate::engine::PolicyEngine::new_with_pg(&config, pg_pool.clone())
        .map_err(|e| format!("init policy engine: {e}"))?;
    let broadcast_tx = crate::server::ws::new_broadcast();
    let batch_buffer = crate::server::ws::spawn_batch_flusher(broadcast_tx.clone());

    let health_registry = if with_health_registry {
        let registry = Arc::new(crate::services::discord::health::HealthRegistry::new());
        registry.init_bot_tokens().await;
        Some(registry)
    } else {
        None
    };

    Ok(AppState {
        #[cfg(test)]
        legacy_db_override: None,
        pg_pool,
        engine,
        config: Arc::new(config),
        broadcast_tx,
        batch_buffer,
        health_registry,
    })
}

async fn run_command<F, Fut>(
    with_health_registry: bool,
    _writes_db: bool,
    operation: F,
) -> Result<(), String>
where
    F: FnOnce(AppState) -> Fut,
    Fut: Future<Output = Result<Value, String>>,
{
    let state = build_app_state(with_health_registry).await?;
    let value = operation(state).await?;
    print_json(&value);
    Ok(())
}

pub(crate) async fn cmd_send(
    target: &str,
    source: Option<&str>,
    bot: Option<&str>,
    content: &str,
) -> Result<(), String> {
    run_command(true, false, |state| async move {
        let registry = state
            .health_registry
            .as_ref()
            .ok_or_else(|| "Discord health registry not available".to_string())?;
        let (status, response) = crate::services::discord::health::send_message_with_backends(
            registry,
            None,
            state.pg_pool_ref(),
            target,
            content,
            source.unwrap_or("system"),
            bot.unwrap_or("announce"),
            None,
        )
        .await;
        let response = response.to_string();
        let value: Value =
            serde_json::from_str(&response).unwrap_or_else(|_| json!({"error": response}));
        let status = parse_health_status_code(status);
        if status.is_success() {
            Ok(value)
        } else {
            Err(extract_error_message(&value)
                .unwrap_or_else(|| format!("send failed ({})", status.as_u16())))
        }
    })
    .await
}

pub(crate) async fn cmd_review_verdict(
    dispatch_id: &str,
    verdict: &str,
    notes: Option<&str>,
    feedback: Option<&str>,
    provider: Option<&str>,
    commit: Option<&str>,
) -> Result<(), String> {
    run_command(false, true, |state| async move {
        let body = crate::server::routes::review_verdict::SubmitVerdictBody {
            dispatch_id: dispatch_id.to_string(),
            overall: verdict.to_string(),
            items: None,
            notes: notes.map(str::to_string),
            feedback: feedback.map(str::to_string),
            commit: commit.map(str::to_string),
            provider: provider.map(str::to_string),
        };
        let (status, body) =
            crate::server::routes::review_verdict::submit_verdict(State(state), Json(body)).await;
        route_json(status, body)
    })
    .await
}

fn review_decision_mode(
    decision: &str,
    dispatch_id: Option<&str>,
) -> Result<ReviewDecisionMode, String> {
    let decision = decision.trim().to_ascii_lowercase();
    match decision.as_str() {
        "accept" | "dispute" => Ok(ReviewDecisionMode::Agent {
            decision,
            dispatch_id: dispatch_id.map(str::to_string),
        }),
        "dismiss" if dispatch_id.is_some() => Ok(ReviewDecisionMode::Agent {
            decision,
            dispatch_id: dispatch_id.map(str::to_string),
        }),
        "approve" => Ok(ReviewDecisionMode::Pm {
            requested: decision,
            applied: "dismiss".to_string(),
        }),
        "escalate" => Ok(ReviewDecisionMode::Pm {
            requested: decision,
            applied: "requeue".to_string(),
        }),
        "rework" | "resume" | "dismiss" | "requeue" => Ok(ReviewDecisionMode::Pm {
            requested: decision.clone(),
            applied: decision,
        }),
        other => Err(format!(
            "unsupported decision '{other}' — use approve|rework|escalate for PM decisions, or accept|dispute|dismiss with --dispatch for review-decision dispatch replies"
        )),
    }
}

enum ReviewDecisionMode {
    Agent {
        decision: String,
        dispatch_id: Option<String>,
    },
    Pm {
        requested: String,
        applied: String,
    },
}

pub(crate) async fn cmd_review_decision(
    card_id: &str,
    decision: &str,
    comment: Option<&str>,
    dispatch_id: Option<&str>,
) -> Result<(), String> {
    let mode = review_decision_mode(decision, dispatch_id)?;
    run_command(false, true, move |state| async move {
        match mode {
            ReviewDecisionMode::Agent {
                decision,
                dispatch_id,
            } => {
                let body = crate::server::routes::review_verdict::ReviewDecisionBody {
                    card_id: card_id.to_string(),
                    decision,
                    comment: comment.map(str::to_string),
                    dispatch_id,
                };
                let (status, body) = crate::server::routes::review_verdict::submit_review_decision(
                    State(state),
                    Json(body),
                )
                .await;
                route_json(status, body)
            }
            ReviewDecisionMode::Pm { requested, applied } => {
                let body = crate::server::routes::kanban::PmDecisionBody {
                    card_id: card_id.to_string(),
                    decision: applied.clone(),
                    comment: comment.map(str::to_string),
                };
                let (status, Json(mut value)) =
                    crate::server::routes::kanban::pm_decision(State(state), Json(body)).await;
                if status.is_success() && requested != applied {
                    value["requested_decision"] = json!(requested);
                    value["applied_decision"] = json!(applied);
                }
                if status.is_success() {
                    Ok(value)
                } else {
                    Err(extract_error_message(&value)
                        .unwrap_or_else(|| format!("review decision failed ({})", status.as_u16())))
                }
            }
        }
    })
    .await
}

pub(crate) async fn cmd_docs(category: Option<&str>, flat: bool) -> Result<(), String> {
    let value = if let Some(category) = category.map(str::trim).filter(|value| !value.is_empty()) {
        let (status, body) =
            crate::server::routes::docs::api_docs_category(Path(category.to_string())).await;
        route_json(status, body)?
    } else {
        let query = crate::server::routes::docs::ApiDocsQuery {
            format: flat.then(|| "flat".to_string()),
        };
        let (status, body) = crate::server::routes::docs::api_docs(Query(query)).await;
        route_json(status, body)?
    };
    print_json(&value);
    Ok(())
}

pub(crate) async fn cmd_auto_queue_activate(
    run_id: Option<&str>,
    agent_id: Option<&str>,
    repo: Option<&str>,
    active_only: bool,
) -> Result<(), String> {
    run_command(false, true, |state| async move {
        let body = crate::server::routes::auto_queue::ActivateBody {
            run_id: run_id.map(str::to_string),
            repo: repo.map(str::to_string),
            agent_id: agent_id.map(str::to_string),
            thread_group: None,
            unified_thread: None,
            active_only: active_only.then_some(true),
        };
        let (status, body) =
            crate::server::routes::auto_queue::activate(State(state), Json(body)).await;
        route_json(status, body)
    })
    .await
}

pub(crate) async fn cmd_force_kill(session_key: &str, retry: bool) -> Result<(), String> {
    run_command(false, true, |state| async move {
        let (status, body) =
            crate::server::routes::dispatched_sessions::force_kill_session_impl_with_reason(
                &state,
                session_key,
                retry,
                "CLI force-kill 명령 실행",
            )
            .await;
        route_json(status, body)
    })
    .await
}

pub(crate) async fn cmd_github_sync(repo: Option<&str>) -> Result<(), String> {
    let state = build_app_state(false).await?;
    let repos = if let Some(repo) = repo.map(str::trim).filter(|repo| !repo.is_empty()) {
        vec![repo.to_string()]
    } else {
        let pool = state
            .pg_pool_ref()
            .ok_or_else(|| "postgres pool unavailable for github sync".to_string())?;
        crate::github::list_repos_pg(pool)
            .await
            .map_err(|e| format!("list registered repos: {e}"))?
            .into_iter()
            .filter(|repo| repo.sync_enabled)
            .map(|repo| repo.id)
            .collect::<Vec<_>>()
    };

    if repos.is_empty() {
        return Err("no registered GitHub repos to sync".to_string());
    }

    let mut results = Vec::new();
    let mut any_failed = false;
    for repo_id in &repos {
        let (owner, repo_name) = split_repo(repo_id)?;
        let (status, Json(value)) = crate::server::routes::github::sync_repo(
            State(state.clone()),
            Path((owner, repo_name)),
        )
        .await;
        if status.is_success() {
            results.push(value);
        } else {
            any_failed = true;
            results.push(json!({
                "repo": repo_id,
                "ok": false,
                "status": status.as_u16(),
                "error": extract_error_message(&value)
                    .unwrap_or_else(|| format!("sync failed ({})", status.as_u16())),
                "response": value,
            }));
        }
    }

    let value = json!({
        "ok": !any_failed,
        "repo_count": repos.len(),
        "results": results,
    });

    print_json(&value);

    if any_failed {
        Err("one or more GitHub repos failed to sync".to_string())
    } else {
        Ok(())
    }
}

pub(crate) async fn cmd_discord_read(
    channel_id: &str,
    limit: Option<u32>,
    before: Option<&str>,
    after: Option<&str>,
) -> Result<(), String> {
    let query = crate::server::routes::discord::MessagesQuery {
        limit,
        before: before.map(str::to_string),
        after: after.map(str::to_string),
    };
    let (status, body) = crate::server::routes::discord::channel_messages(
        Path(channel_id.to_string()),
        Query(query),
    )
    .await;
    let value = route_json(status, body)?;
    print_json(&value);
    Ok(())
}

fn fetch_github_issue(repo: &str, issue_number: i64) -> Result<crate::github::IssueView, String> {
    crate::github::fetch_issue(repo, issue_number)
}

fn infer_issue_priority(labels: &[crate::github::sync::GhLabel]) -> &'static str {
    for label in labels {
        let name = label.name.to_ascii_lowercase();
        if name.contains("critical") || name.contains("urgent") || name.contains("p0") {
            return "critical";
        }
        if name.contains("high") || name.contains("p1") {
            return "high";
        }
        if name.contains("low") || name.contains("p3") {
            return "low";
        }
    }
    "medium"
}

async fn upsert_backlog_card_from_issue_pg(
    pool: &sqlx::PgPool,
    repo: &str,
    issue: &crate::github::IssueView,
) -> Result<String, String> {
    let metadata = {
        let labels: Vec<&str> = issue
            .labels
            .iter()
            .map(|label| label.name.as_str())
            .collect();
        if labels.is_empty() {
            None
        } else {
            Some(json!({"labels": labels.join(",")}).to_string())
        }
    };

    if let Some(existing_id) = sqlx::query_scalar::<_, String>(
        "SELECT id FROM kanban_cards WHERE github_issue_number = $1 AND repo_id = $2",
    )
    .bind(issue.number)
    .bind(repo)
    .fetch_optional(pool)
    .await
    .map_err(|e| format!("query existing postgres card: {e}"))?
    {
        sqlx::query(
            "UPDATE kanban_cards
             SET title = $1,
                 github_issue_url = $2,
                 description = $3,
                 metadata = COALESCE($4, metadata),
                 updated_at = NOW()
             WHERE id = $5",
        )
        .bind(&issue.title)
        .bind(&issue.url)
        .bind(&issue.body)
        .bind(&metadata)
        .bind(&existing_id)
        .execute(pool)
        .await
        .map_err(|e| format!("update existing postgres card: {e}"))?;
        return Ok(existing_id);
    }

    let card_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO kanban_cards (
             id, repo_id, title, status, priority, github_issue_url, github_issue_number,
             description, metadata, created_at, updated_at
         )
         VALUES ($1, $2, $3, 'backlog', $4, $5, $6, $7, $8, NOW(), NOW())",
    )
    .bind(&card_id)
    .bind(repo)
    .bind(&issue.title)
    .bind(infer_issue_priority(&issue.labels))
    .bind(&issue.url)
    .bind(issue.number)
    .bind(&issue.body)
    .bind(&metadata)
    .execute(pool)
    .await
    .map_err(|e| format!("insert postgres backlog card: {e}"))?;
    Ok(card_id)
}

async fn resolve_card_id_pg(
    pool: &sqlx::PgPool,
    card_ref: &str,
    repo: Option<&str>,
) -> Result<String, String> {
    let trimmed = card_ref.trim();
    if trimmed.is_empty() {
        return Err("card reference is required".to_string());
    }
    if !trimmed.chars().all(|ch| ch.is_ascii_digit()) {
        return Ok(trimmed.to_string());
    }

    let issue_number = trimmed
        .parse::<i64>()
        .map_err(|e| format!("invalid issue number '{trimmed}': {e}"))?;
    let ids: Vec<String> = if let Some(repo) = repo.map(str::trim).filter(|repo| !repo.is_empty()) {
        sqlx::query_scalar(
            "SELECT id FROM kanban_cards WHERE github_issue_number = $1 AND repo_id = $2",
        )
        .bind(issue_number)
        .bind(repo)
        .fetch_all(pool)
        .await
        .map_err(|e| format!("query postgres card lookup: {e}"))?
    } else {
        sqlx::query_scalar("SELECT id FROM kanban_cards WHERE github_issue_number = $1")
            .bind(issue_number)
            .fetch_all(pool)
            .await
            .map_err(|e| format!("query postgres card lookup: {e}"))?
    };

    match ids.len() {
        0 => Err(format!("no card found for issue #{issue_number}")),
        1 => Ok(ids[0].clone()),
        _ => Err(format!(
            "multiple cards match issue #{issue_number}; pass --repo owner/repo or use the card id"
        )),
    }
}

pub(crate) async fn cmd_card_create_from_issue(
    issue_number: i64,
    repo: Option<&str>,
    status: Option<&str>,
    agent_id: Option<&str>,
) -> Result<(), String> {
    run_command(false, true, |state| async move {
        let pg_pool = state
            .pg_pool_ref()
            .ok_or_else(|| "postgres pool unavailable for card create".to_string())?
            .clone();
        let repo = resolve_repo_arg_pg(repo, &pg_pool).await?;
        let issue = fetch_github_issue(&repo, issue_number)?;

        let requested_status = status.map(str::trim).filter(|value| !value.is_empty());
        let target_status = requested_status.unwrap_or(if agent_id.is_some() {
            "ready"
        } else {
            "backlog"
        });

        match target_status {
            "backlog" => {
                let card_id = upsert_backlog_card_from_issue_pg(&pg_pool, &repo, &issue)
                    .await
                    .map_err(|e| format!("upsert backlog card in postgres: {e}"))?;
                let (status, body) =
                    crate::server::routes::kanban::get_card(State(state), Path(card_id)).await;
                route_json(status, body)
            }
            "ready" => {
                let default_agent_id = repo_default_agent_id_pg(&pg_pool, &repo).await;
                let effective_agent_id = agent_id
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string)
                    .or(default_agent_id)
                    .ok_or_else(|| {
                        format!(
                            "ready card creation requires --agent or github_repos.default_agent_id for {repo}"
                        )
                    })?;
                let body = crate::server::routes::kanban::AssignIssueBody {
                    github_repo: repo,
                    github_issue_number: issue.number,
                    github_issue_url: Some(issue.url),
                    title: issue.title,
                    description: issue.body,
                    assignee_agent_id: effective_agent_id,
                };
                let (status, body) =
                    crate::server::routes::kanban::assign_issue(State(state), Json(body)).await;
                route_json(status, body)
            }
            other => Err(format!(
                "unsupported --status '{other}' for card create --from-issue; use backlog or ready"
            )),
        }
    })
    .await
}

fn collect_dispatch_commits(dispatches: &[Value]) -> Vec<String> {
    let mut commits = BTreeSet::new();
    for dispatch in dispatches {
        for section in ["context", "result"] {
            let Some(section_value) = dispatch.get(section) else {
                continue;
            };
            for key in ["completed_commit", "reviewed_commit", "commit", "head_sha"] {
                if let Some(commit) = section_value.get(key).and_then(Value::as_str) {
                    let trimmed = commit.trim();
                    if !trimmed.is_empty() {
                        commits.insert(trimmed.to_string());
                    }
                }
            }
        }
    }
    commits.into_iter().collect()
}

fn commit_merged_to_main(repo_dir: &str, commit: &str) -> Result<bool, String> {
    let status = Command::new("git")
        .args(["rev-parse", "--verify", "origin/main"])
        .current_dir(repo_dir)
        .status()
        .map_err(|e| format!("git rev-parse origin/main: {e}"))?;
    let base = if status.success() {
        "origin/main"
    } else {
        "main"
    };

    let merge_status = Command::new("git")
        .args(["merge-base", "--is-ancestor", commit, base])
        .current_dir(repo_dir)
        .status()
        .map_err(|e| format!("git merge-base: {e}"))?;
    Ok(merge_status.success())
}

async fn load_pr_tracking_pg(pool: &sqlx::PgPool, card_id: &str) -> Option<Value> {
    let row = sqlx::query(
        "SELECT repo_id, worktree_path, branch, pr_number, head_sha, state, last_error, created_at, updated_at
         FROM pr_tracking WHERE card_id = $1",
    )
    .bind(card_id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()?;

    Some(json!({
        "repo_id": row.try_get::<Option<String>, _>("repo_id").ok().flatten(),
        "worktree_path": row.try_get::<Option<String>, _>("worktree_path").ok().flatten(),
        "branch": row.try_get::<Option<String>, _>("branch").ok().flatten(),
        "pr_number": row.try_get::<Option<i64>, _>("pr_number").ok().flatten(),
        "head_sha": row.try_get::<Option<String>, _>("head_sha").ok().flatten(),
        "state": row.try_get::<String, _>("state").unwrap_or_default(),
        "last_error": row.try_get::<Option<String>, _>("last_error").ok().flatten(),
        "created_at": row.try_get::<Option<chrono::DateTime<chrono::Utc>>, _>("created_at").ok().flatten().map(|value| value.to_rfc3339()),
        "updated_at": row.try_get::<Option<chrono::DateTime<chrono::Utc>>, _>("updated_at").ok().flatten().map(|value| value.to_rfc3339()),
    }))
}

pub(crate) async fn cmd_card_status(card_ref: &str, repo: Option<&str>) -> Result<(), String> {
    run_command(false, false, |state| async move {
        let pool = state
            .pg_pool_ref()
            .ok_or_else(|| "postgres pool unavailable for card status".to_string())?;
        let card_id = resolve_card_id_pg(pool, card_ref, repo).await?;
        let (status, Json(value)) =
            crate::server::routes::kanban::get_card(State(state.clone()), Path(card_id.clone()))
                .await;
        let card_value = route_json(status, Json(value))?;
        let card = card_value
            .get("card")
            .cloned()
            .ok_or_else(|| "missing card in response".to_string())?;

        let dispatches = state
            .dispatch_service()
            .list_dispatches(None, Some(&card_id))
            .map_err(|e| {
                let (_, Json(value)) = e.into_json_response();
                extract_error_message(&value).unwrap_or_else(|| value.to_string())
            })?;

        let repo_id = card.get("repo_id").and_then(Value::as_str);
        let issue_number = card.get("github_issue_number").and_then(Value::as_i64);
        let repo_dir = resolve_repo_dir(repo_id)?;
        let worktree = match (repo_dir.as_deref(), issue_number) {
            (Some(repo_dir), Some(issue_number)) => {
                crate::services::platform::find_worktree_for_issue(repo_dir, issue_number).map(
                    |info| {
                        json!({
                            "path": info.path,
                            "branch": info.branch,
                            "head_commit": info.commit,
                        })
                    },
                )
            }
            _ => None,
        };

        let mut commits = collect_dispatch_commits(&dispatches);
        if let (Some(repo_dir), Some(issue_number)) = (repo_dir.as_deref(), issue_number) {
            if let Some(commit) =
                crate::services::platform::find_latest_commit_for_issue(repo_dir, issue_number)
            {
                if !commits.iter().any(|existing| existing == &commit) {
                    commits.push(commit);
                }
            }
        }
        if let Some(commit) = worktree
            .as_ref()
            .and_then(|value| value.get("head_commit"))
            .and_then(Value::as_str)
            .map(str::to_string)
        {
            if !commits.iter().any(|existing| existing == &commit) {
                commits.push(commit);
            }
        }

        let merged_to_main = match (repo_dir.as_deref(), commits.first()) {
            (Some(repo_dir), Some(commit)) => Some(commit_merged_to_main(repo_dir, commit)?),
            _ => None,
        };

        let github_issue = match (repo_id, issue_number) {
            (Some(repo_id), Some(issue_number)) if crate::github::gh_available() => {
                fetch_github_issue(repo_id, issue_number)
                    .ok()
                    .map(|issue| {
                        json!({
                            "number": issue.number,
                            "state": issue.state,
                            "title": issue.title,
                            "url": issue.url,
                            "labels": issue.labels.iter().map(|label| label.name.clone()).collect::<Vec<_>>(),
                        })
                    })
            }
            _ => None,
        };

        let pr_tracking = load_pr_tracking_pg(pool, &card_id).await;

        Ok(json!({
            "card": card,
            "dispatches": dispatches,
            "connected_commits": commits,
            "merged_to_main": merged_to_main,
            "worktree": worktree,
            "pr_tracking": pr_tracking,
            "github_issue": github_issue,
        }))
    })
    .await
}

async fn resolve_auto_queue_run_pg(
    pool: &sqlx::PgPool,
    run_id: Option<&str>,
    repo: Option<&str>,
    agent_id: Option<&str>,
) -> Result<String, String> {
    if let Some(run_id) = run_id.map(str::trim).filter(|value| !value.is_empty()) {
        let exists = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM auto_queue_runs WHERE id = $1)",
        )
        .bind(run_id)
        .fetch_one(pool)
        .await
        .map_err(|e| format!("query postgres auto-queue run '{run_id}': {e}"))?;
        if exists {
            return Ok(run_id.to_string());
        }
        return Err(format!("auto-queue run '{run_id}' not found"));
    }

    let repo = repo.map(str::trim).filter(|value| !value.is_empty());
    let agent_id = agent_id.map(str::trim).filter(|value| !value.is_empty());
    sqlx::query_scalar::<_, String>(
        "SELECT id
         FROM auto_queue_runs
         WHERE status IN ('active', 'pending', 'generated', 'paused')
           AND ($1::TEXT IS NULL OR repo = $1 OR repo IS NULL OR repo = '')
           AND ($2::TEXT IS NULL OR agent_id = $2 OR agent_id IS NULL OR agent_id = '')
         ORDER BY created_at DESC
         LIMIT 1",
    )
    .bind(repo)
    .bind(agent_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| format!("query postgres live auto-queue run: {e}"))?
    .ok_or_else(|| "no matching live auto-queue run found".to_string())
}

pub(crate) async fn cmd_auto_queue_add(
    card_ref: &str,
    run_id: Option<&str>,
    priority: Option<i64>,
    phase: Option<i64>,
    thread_group: Option<i64>,
    agent_id: Option<&str>,
) -> Result<(), String> {
    if let Some(thread_group) = thread_group {
        if thread_group < 0 {
            return Err("--thread-group must be >= 0".to_string());
        }
    }

    run_command(false, true, |state| async move {
        let pool = state
            .pg_pool_ref()
            .ok_or_else(|| "postgres pool unavailable for auto-queue add".to_string())?;
        let card_id = resolve_card_id_pg(pool, card_ref, None).await?;

        let (repo_id, card_agent_id, card_status, issue_number): (
            Option<String>,
            Option<String>,
            String,
            Option<i64>,
        ) = {
            let row = sqlx::query(
                "SELECT repo_id, assigned_agent_id, status, github_issue_number
                 FROM kanban_cards
                 WHERE id = $1",
            )
            .bind(&card_id)
            .fetch_optional(pool)
            .await
            .map_err(|e| format!("load postgres card '{card_id}': {e}"))?
            .ok_or_else(|| format!("card '{card_id}' not found"))?;
            (
                row.try_get("repo_id").ok().flatten(),
                row.try_get("assigned_agent_id").ok().flatten(),
                row.try_get("status")
                    .map_err(|e| format!("decode postgres card status: {e}"))?,
                row.try_get("github_issue_number").ok().flatten(),
            )
        };

        let effective_agent = agent_id
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .or(card_agent_id)
            .unwrap_or_default();
        let existing_run_id = match resolve_auto_queue_run_pg(
            pool,
            run_id,
            repo_id.as_deref(),
            (!effective_agent.is_empty()).then_some(effective_agent.as_str()),
        )
        .await
        {
            Ok(existing_run_id) => Some(existing_run_id),
            Err(err) if run_id.is_some() => return Err(err),
            Err(_) => None,
        };

        if let Some(existing_run_id) = existing_run_id {
            let run_status = sqlx::query_scalar::<_, String>(
                "SELECT status FROM auto_queue_runs WHERE id = $1",
            )
            .bind(&existing_run_id)
            .fetch_optional(pool)
            .await
            .map_err(|e| format!("load postgres auto-queue run status: {e}"))?;
            if run_id.is_some() || matches!(run_status.as_deref(), Some("active")) {
                let issue_number = issue_number.ok_or_else(|| {
                    format!("card '{card_id}' has no linked GitHub issue number")
                })?;
                let (status, Json(mut value)) = crate::server::routes::auto_queue::add_run_entry(
                    State(state.clone()),
                    Path(existing_run_id.clone()),
                    Json(crate::server::routes::auto_queue::AddRunEntryBody {
                        issue_number,
                        batch_phase: Some(phase.unwrap_or(0)),
                        thread_group,
                    }),
                )
                .await;
                if !status.is_success() {
                    return Err(extract_error_message(&value)
                        .unwrap_or_else(|| format!("auto-queue add failed ({})", status.as_u16())));
                }
                if let Some(priority_rank) = priority {
                    if let Some(entry_id) = value
                        .get("entry")
                        .and_then(|entry| entry.get("id"))
                        .and_then(Value::as_str)
                    {
                        let (update_status, Json(update_value)) =
                            crate::server::routes::auto_queue::update_entry(
                                State(state),
                                Path(entry_id.to_string()),
                                Json(crate::server::routes::auto_queue::UpdateEntryBody {
                                    thread_group: None,
                                    priority_rank: Some(priority_rank),
                                    batch_phase: None,
                                    status: None,
                                }),
                            )
                            .await;
                        if update_status.is_success() {
                            value = update_value;
                        } else {
                            return Err(extract_error_message(&update_value).unwrap_or_else(|| {
                                format!("auto-queue priority update failed ({})", update_status.as_u16())
                            }));
                        }
                    }
                }
                return Ok(value);
            }
        }

        let run_id = {
            let created_run_id = uuid::Uuid::new_v4().to_string();
            sqlx::query(
                "INSERT INTO auto_queue_runs (
                     id, repo, agent_id, status, ai_model, ai_rationale, max_concurrent_threads, thread_group_count
                 )
                 VALUES ($1, $2, $3, 'pending', 'manual-cli', 'agentdesk auto-queue add', 1, 1)",
            )
            .bind(&created_run_id)
            .bind(&repo_id)
            .bind(&effective_agent)
            .execute(pool)
            .await
            .map_err(|e| format!("create postgres auto-queue run: {e}"))?;
            created_run_id
        };

        let already_queued = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(
                 SELECT 1 FROM auto_queue_entries
                 WHERE run_id = $1 AND kanban_card_id = $2 AND status IN ('pending', 'dispatched')
             )",
        )
        .bind(&run_id)
        .bind(&card_id)
        .fetch_one(pool)
        .await
        .unwrap_or(false);
        if already_queued {
            return Ok(json!({
                "ok": true,
                "already_queued": true,
                "run_id": run_id,
                "card_id": card_id,
            }));
        }

        let has_active_dispatch = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(
                 SELECT 1 FROM task_dispatches
                 WHERE kanban_card_id = $1 AND status IN ('pending', 'dispatched')
             )",
        )
        .bind(&card_id)
        .fetch_one(pool)
        .await
        .unwrap_or(false);
        if has_active_dispatch {
            return Err(format!(
                "card {card_id} already has an active dispatch and cannot be queued again"
            ));
        }

        let effective_thread_group = thread_group.unwrap_or(0);
        let next_rank = if let Some(priority) = priority {
            priority
        } else {
            sqlx::query_scalar::<_, i64>(
                "SELECT (COALESCE(MAX(priority_rank), -1) + 1)::BIGINT
                 FROM auto_queue_entries
                 WHERE run_id = $1
                   AND COALESCE(thread_group, 0) = $2",
            )
            .bind(&run_id)
            .bind(effective_thread_group)
            .fetch_one(pool)
            .await
            .unwrap_or(0)
        };
        let entry_id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO auto_queue_entries (
                 id, run_id, kanban_card_id, agent_id, priority_rank, thread_group, batch_phase, reason
             )
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(&entry_id)
        .bind(&run_id)
        .bind(&card_id)
        .bind(&effective_agent)
        .bind(next_rank)
        .bind(effective_thread_group)
        .bind(phase.unwrap_or(0))
        .bind(format!("manual CLI add from status {card_status}"))
        .execute(pool)
        .await
        .map_err(|e| format!("insert postgres auto-queue entry: {e}"))?;

        Ok(json!({
            "ok": true,
            "run_id": run_id,
            "entry_id": entry_id,
            "card_id": card_id,
            "priority_rank": next_rank,
            "thread_group": effective_thread_group,
            "batch_phase": phase.unwrap_or(0),
        }))
    })
    .await
}

pub(crate) async fn cmd_auto_queue_config(
    run_id: Option<&str>,
    repo: Option<&str>,
    agent_id: Option<&str>,
    max_concurrent_threads: i64,
) -> Result<(), String> {
    if max_concurrent_threads < 1 {
        return Err("--max-concurrent must be >= 1".to_string());
    }

    run_command(false, true, |state| async move {
        let pool = state
            .pg_pool_ref()
            .ok_or_else(|| "postgres pool unavailable for auto-queue config".to_string())?;
        let pool = pool.clone();
        let run_id = resolve_auto_queue_run_pg(&pool, run_id, repo, agent_id).await?;
        let (status, Json(value)) = crate::server::routes::auto_queue::update_run(
            State(state.clone()),
            Path(run_id.clone()),
            Json(crate::server::routes::auto_queue::UpdateRunBody {
                status: None,
                unified_thread: None,
                deploy_phases: None,
                max_concurrent_threads: Some(max_concurrent_threads),
            }),
        )
        .await;
        if !status.is_success() {
            return Err(extract_error_message(&value)
                .unwrap_or_else(|| format!("auto-queue config failed ({})", status.as_u16())));
        }

        let row = sqlx::query(
            "SELECT id, repo, agent_id, status, max_concurrent_threads, thread_group_count
             FROM auto_queue_runs WHERE id = $1",
        )
        .bind(&run_id)
        .fetch_one(&pool)
        .await
        .map_err(|e| format!("reload postgres auto-queue run: {e}"))?;
        let run = json!({
            "id": row.try_get::<String, _>("id").unwrap_or_default(),
            "repo": row.try_get::<Option<String>, _>("repo").ok().flatten(),
            "agent_id": row.try_get::<Option<String>, _>("agent_id").ok().flatten(),
            "status": row.try_get::<String, _>("status").unwrap_or_default(),
            "max_concurrent_threads": row.try_get::<i64, _>("max_concurrent_threads").unwrap_or(1),
            "thread_group_count": row.try_get::<i64, _>("thread_group_count").unwrap_or(1),
        });

        Ok(json!({
            "ok": true,
            "run": run,
        }))
    })
    .await
}

#[derive(Clone, Debug, Default)]
struct WorktreeEntry {
    path: String,
    branch: Option<String>,
}

fn parse_worktree_list(text: &str) -> Vec<WorktreeEntry> {
    let mut entries = Vec::new();
    let mut current = WorktreeEntry::default();
    for line in text.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            current.path = path.to_string();
        } else if let Some(branch) = line.strip_prefix("branch ") {
            current.branch = Some(
                branch
                    .strip_prefix("refs/heads/")
                    .unwrap_or(branch)
                    .to_string(),
            );
        } else if line.trim().is_empty() {
            if !current.path.is_empty() {
                entries.push(current.clone());
            }
            current = WorktreeEntry::default();
        }
    }
    if !current.path.is_empty() {
        entries.push(current);
    }
    entries
}

fn git_output(dir: &str, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .map_err(|e| format!("git {}: {e}", args.join(" ")))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn git_status_output(dir: &str, args: &[&str]) -> Result<bool, String> {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .map_err(|e| format!("git {}: {e}", args.join(" ")))?;
    Ok(status.success())
}

fn extract_issue_numbers(text: &str) -> Vec<String> {
    static ISSUE_RE: OnceLock<Regex> = OnceLock::new();
    let regex = ISSUE_RE.get_or_init(|| Regex::new(r"#(\d+)").expect("valid issue regex"));
    let mut issues = BTreeSet::new();
    for capture in regex.captures_iter(text) {
        if let Some(issue) = capture.get(1) {
            issues.insert(issue.as_str().to_string());
        }
    }
    issues.into_iter().collect()
}

fn maybe_restore_stash(main_worktree: &str, stash_created: bool) -> Result<Option<String>, String> {
    if !stash_created {
        return Ok(None);
    }
    let output = Command::new("git")
        .args(["stash", "pop"])
        .current_dir(main_worktree)
        .output()
        .map_err(|e| format!("git stash pop: {e}"))?;
    if output.status.success() {
        return Ok(Some("stash restored".to_string()));
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if stderr.is_empty() {
        Ok(Some(
            "stash created but restore needs manual check".to_string(),
        ))
    } else {
        Ok(Some(format!(
            "stash created but restore reported conflicts: {stderr}"
        )))
    }
}

pub(crate) fn cmd_cherry_merge(branch: &str, close_issue: bool) -> Result<(), String> {
    let repo_dir = crate::services::platform::resolve_repo_dir()
        .ok_or_else(|| "could not resolve AgentDesk repo dir".to_string())?;
    let worktree_output = git_output(&repo_dir, &["worktree", "list", "--porcelain"])?;
    let worktrees = parse_worktree_list(&worktree_output);

    let source = worktrees
        .iter()
        .find(|entry| entry.branch.as_deref() == Some(branch))
        .cloned();
    let main_worktree = worktrees
        .iter()
        .find(|entry| matches!(entry.branch.as_deref(), Some("main") | Some("master")))
        .cloned()
        .or_else(|| worktrees.first().cloned())
        .ok_or_else(|| "could not locate main worktree".to_string())?;

    let main_branch = main_worktree
        .branch
        .clone()
        .unwrap_or_else(|| "main".to_string());
    let branch_range = format!("{main_branch}..{branch}");
    let commits_output = git_output(
        &main_worktree.path,
        &["rev-list", "--reverse", &branch_range],
    )?;
    let commits: Vec<String> = commits_output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect();
    if commits.is_empty() {
        return Err(format!("no commits to cherry-pick from branch '{branch}'"));
    }

    let has_local_changes =
        !git_output(&main_worktree.path, &["status", "--porcelain"])?.is_empty();
    let stash_created = if has_local_changes {
        git_output(
            &main_worktree.path,
            &[
                "stash",
                "push",
                "-u",
                "-m",
                &format!("agentdesk cherry-merge {branch}"),
            ],
        )?;
        true
    } else {
        false
    };

    let cherry_pick_result = git_output(
        &main_worktree.path,
        &std::iter::once("cherry-pick")
            .chain(commits.iter().map(String::as_str))
            .collect::<Vec<_>>(),
    );
    if let Err(error) = cherry_pick_result {
        let _ = git_output(&main_worktree.path, &["cherry-pick", "--abort"]);
        let stash_note = maybe_restore_stash(&main_worktree.path, stash_created)?;
        let mut response = json!({
            "ok": false,
            "branch": branch,
            "main_branch": main_branch,
            "error": error,
        });
        if let Some(stash_note) = stash_note {
            response["stash"] = json!(stash_note);
        }
        print_json(&response);
        return Err("cherry-pick failed; main worktree rollback attempted".to_string());
    }

    git_output(&main_worktree.path, &["push", "origin", &main_branch])?;

    let close_issue_result = if close_issue {
        let log = git_output(&main_worktree.path, &["log", "--format=%s", &branch_range])?;
        let issues = extract_issue_numbers(&log);
        if issues.len() == 1 {
            let repo_id = maybe_infer_repo_from_git()
                .ok_or_else(|| "could not infer repo for gh issue close".to_string())?;
            let issue_number = issues[0]
                .parse::<i64>()
                .map_err(|e| format!("invalid issue number '{}': {e}", issues[0]))?;
            crate::github::close_issue(&repo_id, issue_number)?;
            Some(json!({
                "repo": repo_id,
                "issue_number": issue_number,
                "closed": true,
            }))
        } else {
            Some(json!({
                "closed": false,
                "reason": "issue number could not be inferred uniquely from branch commits",
                "candidates": issues,
            }))
        }
    } else {
        None
    };

    let worktree_removed = if let Some(source) = source.as_ref() {
        if source.path != main_worktree.path {
            git_status_output(&repo_dir, &["worktree", "remove", "--force", &source.path])?
        } else {
            false
        }
    } else {
        false
    };

    let stash_note = maybe_restore_stash(&main_worktree.path, stash_created)?;
    let value = json!({
        "ok": true,
        "branch": branch,
        "main_branch": main_branch,
        "commits": commits,
        "worktree_removed": worktree_removed,
        "closed_issue": close_issue_result,
        "stash": stash_note,
    });
    print_json(&value);
    Ok(())
}

/// Retry dispatch for a card, bypassing HTTP — calls the route handler directly.
pub(crate) async fn cmd_dispatch_retry(card_id: &str) -> Result<(), String> {
    run_command(false, true, |state| async move {
        let body = crate::server::routes::kanban::RetryCardBody {
            assignee_agent_id: None,
            request_now: None,
        };
        let (status, body) = crate::server::routes::kanban::retry_card(
            State(state),
            Path(card_id.to_string()),
            Json(body),
        )
        .await;
        route_json(status, body)
    })
    .await
}

/// Redispatch a card, bypassing HTTP — calls the route handler directly.
pub(crate) async fn cmd_dispatch_redispatch(card_id: &str) -> Result<(), String> {
    run_command(false, true, |state| async move {
        let body = crate::server::routes::kanban::RedispatchCardBody { reason: None };
        let (status, body) = crate::server::routes::kanban::redispatch_card(
            State(state),
            Path(card_id.to_string()),
            Json(body),
        )
        .await;
        route_json(status, body)
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::{extract_issue_numbers, parse_worktree_list, review_decision_mode};

    #[test]
    fn parse_worktree_list_reads_porcelain_blocks() {
        let parsed = parse_worktree_list(
            "worktree /tmp/main\nHEAD abc\nbranch refs/heads/main\n\nworktree /tmp/wt-1\nHEAD def\nbranch refs/heads/wt/439\n",
        );
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].path, "/tmp/main");
        assert_eq!(parsed[0].branch.as_deref(), Some("main"));
        assert_eq!(parsed[1].branch.as_deref(), Some("wt/439"));
    }

    #[test]
    fn extract_issue_numbers_deduplicates_multiple_mentions() {
        let issues = extract_issue_numbers("fix (#12)\nfollow-up #12\nalso #55");
        assert_eq!(issues, vec!["12".to_string(), "55".to_string()]);
    }

    #[test]
    fn review_decision_mode_maps_pm_aliases() {
        match review_decision_mode("approve", None).unwrap() {
            super::ReviewDecisionMode::Pm { requested, applied } => {
                assert_eq!(requested, "approve");
                assert_eq!(applied, "dismiss");
            }
            _ => panic!("expected PM review decision mode"),
        }
    }

    #[test]
    fn review_decision_mode_uses_agent_path_for_dispatch_scoped_dismiss() {
        match review_decision_mode("dismiss", Some("dispatch-1")).unwrap() {
            super::ReviewDecisionMode::Agent { decision, .. } => {
                assert_eq!(decision, "dismiss");
            }
            _ => panic!("expected agent review decision mode"),
        }
    }

    /// Integration test: exercises `build_app_state(false)` and runs `cmd_docs`,
    /// catching initialization regressions in the direct CLI module.
    #[tokio::test]
    async fn build_app_state_and_cmd_docs_smoke() {
        // build_app_state(false) skips health registry — lightweight init
        let state = super::build_app_state(false).await;
        if let Err(e) = &state {
            panic!("build_app_state(false) failed: {e}");
        }

        // Run cmd_docs (read-only, no db writes) to verify the full init path
        // produces a usable AppState. cmd_docs prints to stdout and returns Ok
        // on success; we just verify it doesn't panic or error.
        let result = super::cmd_docs(None, false).await;
        if let Err(e) = &result {
            panic!("cmd_docs failed after build_app_state: {e}");
        }
    }
}
