use axum::{Json, extract::State, http::StatusCode};
use serde::Deserialize;
use serde_json::json;
use sqlx::Row as SqlxRow;

use crate::error::{AppError, AppResult, ErrorCode};
use crate::server::routes::AppState;

const DISCORD_API_BASE: &str = "https://discord.com/api/v10";
const DISCORD_MESSAGE_CHAR_LIMIT: usize = 2000;
const ESCALATION_ISSUE_SUMMARY_LINE_LIMIT: usize = 3;
const ESCALATION_SECTION_CHAR_LIMIT: usize = 320;
const ESCALATION_REASON_CHAR_LIMIT: usize = 240;
const ESCALATION_RECENT_RESULT_LIMIT: usize = 2;

fn pg_unavailable() -> AppError {
    AppError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        ErrorCode::Config,
        "postgres pool not configured",
    )
}

#[derive(Debug, Deserialize)]
pub struct EmitEscalationBody {
    pub card_id: String,
    #[serde(default)]
    pub reasons: Vec<String>,
}

#[derive(Debug)]
struct CardEscalationSummary {
    title: String,
    issue_number: Option<i64>,
    description: Option<String>,
    status: String,
    review_status: Option<String>,
    blocked_reason: Option<String>,
    dispatch_count: i64,
    last_agent_id: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct CardContext {
    issue_summary: Option<String>,
    progress_summary: Option<String>,
    recent_results: Vec<String>,
    blocked_reason: Option<String>,
}

async fn load_card_summary_pg_async(
    pool: &sqlx::PgPool,
    card_id: &str,
) -> Result<CardEscalationSummary, String> {
    let row = sqlx::query(
        "SELECT
             kc.title,
             kc.github_issue_number,
             kc.description,
             kc.status,
             kc.review_status,
             kc.blocked_reason,
             (
                 SELECT COUNT(*)
                 FROM task_dispatches td_count
                 WHERE td_count.kanban_card_id = $1
             ) AS dispatch_count,
             (
                 SELECT td_last.to_agent_id
                 FROM task_dispatches td_last
                 WHERE td_last.kanban_card_id = $1
                   AND td_last.to_agent_id IS NOT NULL
                   AND BTRIM(td_last.to_agent_id) != ''
                 ORDER BY COALESCE(td_last.completed_at, td_last.updated_at, td_last.created_at) DESC,
                          td_last.id DESC
                 LIMIT 1
             ) AS last_agent_id
         FROM kanban_cards kc
         WHERE kc.id = $1",
    )
    .bind(card_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load postgres card summary {card_id}: {error}"))?;

    let Some(row) = row else {
        return Err(format!("card not found: {card_id}"));
    };

    Ok(CardEscalationSummary {
        title: row
            .try_get("title")
            .map_err(|error| format!("decode postgres card summary title {card_id}: {error}"))?,
        issue_number: row.try_get("github_issue_number").map_err(|error| {
            format!("decode postgres card summary github_issue_number {card_id}: {error}")
        })?,
        description: row.try_get("description").map_err(|error| {
            format!("decode postgres card summary description {card_id}: {error}")
        })?,
        status: row
            .try_get("status")
            .map_err(|error| format!("decode postgres card summary status {card_id}: {error}"))?,
        review_status: row.try_get("review_status").map_err(|error| {
            format!("decode postgres card summary review_status {card_id}: {error}")
        })?,
        blocked_reason: row.try_get("blocked_reason").map_err(|error| {
            format!("decode postgres card summary blocked_reason {card_id}: {error}")
        })?,
        dispatch_count: row.try_get("dispatch_count").map_err(|error| {
            format!("decode postgres card summary dispatch_count {card_id}: {error}")
        })?,
        last_agent_id: row.try_get("last_agent_id").map_err(|error| {
            format!("decode postgres card summary last_agent_id {card_id}: {error}")
        })?,
    })
}

fn format_card_label(summary: &CardEscalationSummary) -> String {
    if let Some(issue_number) = summary.issue_number {
        format!("#{issue_number} {}", summary.title)
    } else {
        summary.title.clone()
    }
}

fn format_reason_lines(reasons: &[String]) -> String {
    reasons
        .iter()
        .map(|reason| format!("- {reason}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn normalize_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    let trimmed = value.trim();
    if trimmed.chars().count() <= max_chars {
        return trimmed.to_string();
    }
    if max_chars <= 3 {
        return trimmed.chars().take(max_chars).collect();
    }
    let mut shortened = trimmed.chars().take(max_chars - 3).collect::<String>();
    shortened.push_str("...");
    shortened
}

fn summarize_issue_description(description: Option<&str>) -> Option<String> {
    let lines = description
        .into_iter()
        .flat_map(|raw| raw.lines())
        .map(normalize_whitespace)
        .filter(|line| !line.is_empty())
        .take(ESCALATION_ISSUE_SUMMARY_LINE_LIMIT)
        .collect::<Vec<_>>();
    if lines.is_empty() {
        None
    } else {
        Some(truncate_chars(
            &lines.join(" "),
            ESCALATION_SECTION_CHAR_LIMIT,
        ))
    }
}

fn format_phase(status: &str, review_status: Option<&str>) -> Option<String> {
    let status = normalize_whitespace(status);
    if status.is_empty() {
        return None;
    }
    let review_status = review_status
        .map(normalize_whitespace)
        .filter(|value| !value.is_empty());
    Some(match review_status {
        Some(review_status) => format!("{status}/{review_status}"),
        None => status,
    })
}

fn format_progress_summary(summary: &CardEscalationSummary) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(phase) = format_phase(&summary.status, summary.review_status.as_deref()) {
        parts.push(phase);
    }
    if summary.dispatch_count > 0 {
        parts.push(format!("{}회 디스패치", summary.dispatch_count));
    }
    if let Some(agent_id) = summary
        .last_agent_id
        .as_deref()
        .map(normalize_whitespace)
        .filter(|value| !value.is_empty())
    {
        parts.push(format!("마지막 에이전트: {agent_id}"));
    }
    if parts.is_empty() {
        None
    } else {
        Some(truncate_chars(
            &parts.join(" · "),
            ESCALATION_SECTION_CHAR_LIMIT,
        ))
    }
}

fn extract_dispatch_result_summary(value: &serde_json::Value) -> Option<String> {
    const PREFERRED_KEYS: &[&str] = &[
        "summary",
        "work_summary",
        "result_summary",
        "task_summary",
        "completion_summary",
        "outcome",
        "message",
        "final_message",
        "notes",
        "content",
    ];

    match value {
        serde_json::Value::String(text) => {
            let normalized = normalize_whitespace(text);
            if normalized.is_empty() {
                None
            } else {
                Some(truncate_chars(&normalized, ESCALATION_SECTION_CHAR_LIMIT))
            }
        }
        serde_json::Value::Object(map) => {
            for key in PREFERRED_KEYS {
                if let Some(summary) = map.get(*key).and_then(extract_dispatch_result_summary) {
                    return Some(summary);
                }
            }
            map.values().find_map(extract_dispatch_result_summary)
        }
        serde_json::Value::Array(items) => items.iter().find_map(extract_dispatch_result_summary),
        _ => None,
    }
}

fn summarize_dispatch_result_text(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    serde_json::from_str::<serde_json::Value>(trimmed)
        .ok()
        .and_then(|value| extract_dispatch_result_summary(&value))
        .or_else(|| {
            let normalized = normalize_whitespace(trimmed);
            if normalized.is_empty() {
                None
            } else {
                Some(truncate_chars(&normalized, ESCALATION_SECTION_CHAR_LIMIT))
            }
        })
}

async fn load_recent_dispatch_results_pg_async(
    pool: &sqlx::PgPool,
    card_id: &str,
) -> Result<Vec<String>, String> {
    let rows = sqlx::query(
        "SELECT dispatch_type, result
         FROM task_dispatches
         WHERE kanban_card_id = $1
           AND status IN ('completed', 'failed')
         ORDER BY COALESCE(completed_at, updated_at, created_at) DESC, id DESC
         LIMIT $2",
    )
    .bind(card_id)
    .bind(ESCALATION_RECENT_RESULT_LIMIT as i64)
    .fetch_all(pool)
    .await
    .map_err(|error| format!("load postgres recent dispatch results {card_id}: {error}"))?;

    let mut results = Vec::new();
    for row in rows {
        let dispatch_type: Option<String> = row.try_get("dispatch_type").map_err(|error| {
            format!("decode postgres recent dispatch dispatch_type {card_id}: {error}")
        })?;
        let result: Option<String> = row.try_get("result").map_err(|error| {
            format!("decode postgres recent dispatch result {card_id}: {error}")
        })?;
        let Some(summary) = result
            .as_deref()
            .and_then(summarize_dispatch_result_text)
            .filter(|value| !value.is_empty())
        else {
            continue;
        };
        let label = dispatch_type
            .as_deref()
            .map(normalize_whitespace)
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "dispatch".to_string());
        results.push(format!("{label}: {summary}"));
    }
    Ok(results)
}

async fn load_card_context_pg_async(
    pool: &sqlx::PgPool,
    card_id: &str,
    summary: &CardEscalationSummary,
) -> Result<Option<CardContext>, String> {
    let issue_summary = summarize_issue_description(summary.description.as_deref());
    let progress_summary = format_progress_summary(summary);
    let recent_results = load_recent_dispatch_results_pg_async(pool, card_id).await?;
    let blocked_reason = summary
        .blocked_reason
        .as_deref()
        .map(normalize_whitespace)
        .filter(|value| !value.is_empty())
        .map(|value| truncate_chars(&value, ESCALATION_REASON_CHAR_LIMIT));

    let context = CardContext {
        issue_summary,
        progress_summary,
        recent_results,
        blocked_reason,
    };

    if context.issue_summary.is_none()
        && context.progress_summary.is_none()
        && context.recent_results.is_empty()
        && context.blocked_reason.is_none()
    {
        Ok(None)
    } else {
        Ok(Some(context))
    }
}

fn load_escalation_emit_inputs_pg(
    pool: &sqlx::PgPool,
    card_id: &str,
) -> Result<(CardEscalationSummary, Option<CardContext>), String> {
    let card_id = card_id.to_string();
    crate::utils::async_bridge::block_on_pg_result(
        pool,
        move |bridge_pool| async move {
            let summary = load_card_summary_pg_async(&bridge_pool, &card_id).await?;
            let context = load_card_context_pg_async(&bridge_pool, &card_id, &summary).await?;
            Ok((summary, context))
        },
        |error| error,
    )
}

fn render_context_sections(context: Option<&CardContext>) -> Vec<String> {
    let Some(context) = context else {
        return Vec::new();
    };

    let mut sections = Vec::new();
    if let Some(issue_summary) = context.issue_summary.as_deref() {
        sections.push(format!("📋 이슈: {issue_summary}"));
    }
    if let Some(progress_summary) = context.progress_summary.as_deref() {
        sections.push(format!("📊 진행: {progress_summary}"));
    }
    if !context.recent_results.is_empty() {
        sections.push(format!(
            "📝 최근 결과:\n{}",
            context
                .recent_results
                .iter()
                .map(|line| format!("- {line}"))
                .collect::<Vec<_>>()
                .join("\n")
        ));
    }
    if let Some(blocked_reason) = context.blocked_reason.as_deref() {
        sections.push(format!("⛔ 기존 차단 사유: {blocked_reason}"));
    }
    sections
}

fn compose_escalation_message(
    prefix_lines: Vec<String>,
    context: Option<&CardContext>,
    suffix_lines: Vec<String>,
) -> String {
    let mut context_sections = render_context_sections(context);

    loop {
        let mut lines = prefix_lines.clone();
        lines.extend(context_sections.iter().cloned());
        lines.append(&mut suffix_lines.clone());
        let message = lines.join("\n");
        if message.chars().count() <= DISCORD_MESSAGE_CHAR_LIMIT {
            return message;
        }
        if context_sections.is_empty() {
            return truncate_chars(&message, DISCORD_MESSAGE_CHAR_LIMIT);
        }
        context_sections.pop();
    }
}

fn build_owner_message(
    summary: &CardEscalationSummary,
    context: Option<&CardContext>,
    reasons: &[String],
) -> String {
    compose_escalation_message(
        vec![
            format!("⚠️ [에스컬레이션] {}", format_card_label(summary)),
            "수동 판단이 필요합니다.".to_string(),
        ],
        context,
        vec![
            "사유:".to_string(),
            format_reason_lines(reasons),
            "선택지: `resume`, `rework`, `dismiss`, `requeue`".to_string(),
            "결정 API: `POST /api/pm-decision`".to_string(),
        ],
    )
}

fn summary_manual_decision_is_superseded(summary: &CardEscalationSummary) -> bool {
    matches!(summary.status.trim(), "backlog" | "done")
}

async fn discord_post_json(
    client: &reqwest::Client,
    base_url: &str,
    token: &str,
    path: &str,
    body: &serde_json::Value,
) -> Result<reqwest::Response, String> {
    client
        .post(format!("{}{}", base_url.trim_end_matches('/'), path))
        .header("Authorization", format!("Bot {}", token))
        .json(body)
        .send()
        .await
        .map_err(|err| err.to_string())
}

async fn open_dm_channel(
    client: &reqwest::Client,
    base_url: &str,
    token: &str,
    user_id: u64,
) -> Result<String, String> {
    let response = discord_post_json(
        client,
        base_url,
        token,
        "/users/@me/channels",
        &json!({ "recipient_id": user_id.to_string() }),
    )
    .await?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(format!("discord dm open failed: {status} {body}"));
    }
    let body: serde_json::Value = response.json().await.map_err(|err| err.to_string())?;
    body.get("id")
        .and_then(|value| value.as_str())
        .map(|value| value.to_string())
        .ok_or_else(|| "discord dm open missing id".to_string())
}

async fn send_channel_message(
    client: &reqwest::Client,
    base_url: &str,
    token: &str,
    channel_id: &str,
    content: &str,
) -> Result<(), String> {
    let response = discord_post_json(
        client,
        base_url,
        token,
        &format!("/channels/{channel_id}/messages"),
        &json!({ "content": content }),
    )
    .await?;
    if response.status().is_success() {
        Ok(())
    } else {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        Err(format!("discord message failed: {status} {body}"))
    }
}

async fn emit_escalation_with_base_url(
    state: &AppState,
    body: EmitEscalationBody,
    base_url: &str,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let card_id = body.card_id.trim().to_string();
    let reasons = body
        .reasons
        .into_iter()
        .map(|reason| reason.trim().to_string())
        .filter(|reason| !reason.is_empty())
        .collect::<Vec<_>>();
    if card_id.is_empty() {
        return Err(AppError::bad_request("card_id is required"));
    }
    if reasons.is_empty() {
        return Err(AppError::bad_request("reasons must not be empty"));
    }

    let Some(pool) = state.pg_pool_ref() else {
        return Err(pg_unavailable());
    };
    let (summary, context) = match load_escalation_emit_inputs_pg(pool, &card_id) {
        Ok(loaded) => loaded,
        Err(error) => {
            tracing::warn!(%error, "[escalation] postgres load failed for {card_id}");
            let status = if error.starts_with("card not found:") {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            return Err(AppError::new(status, ErrorCode::Internal, error));
        }
    };

    if summary_manual_decision_is_superseded(&summary) {
        let card_status = summary.status.clone();
        let review_status = summary.review_status.clone();
        return Ok((
            StatusCode::CONFLICT,
            Json(json!({
                "ok": false,
                "state": "superseded",
                "card_id": card_id,
                "card_status": card_status,
                "review_status": review_status,
                "error": "manual decision no longer required for this card",
            })),
        ));
    }

    // An error keeps the policy's pending entry, so a missing owner is retried
    // instead of silently dropping the alert.
    let Some(owner_user_id) = state.config.discord.owner_id else {
        return Err(AppError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::Config,
            "discord.owner_id is not configured",
        ));
    };

    let token = match crate::credential::read_bot_token(
        crate::services::discord::bot_role::UtilityBotRole::Notify.alias(),
    )
    .or_else(|| {
        crate::credential::read_bot_token(
            crate::services::discord::bot_role::UtilityBotRole::Announce.alias(),
        )
    }) {
        Some(token) => token,
        None => {
            return Err(AppError::internal("bot token not found").with_code(ErrorCode::Discord));
        }
    };

    let client = reqwest::Client::new();
    let message = build_owner_message(&summary, context.as_ref(), &reasons);
    let delivered = match open_dm_channel(&client, base_url, &token, owner_user_id).await {
        Ok(dm_channel_id) => {
            send_channel_message(&client, base_url, &token, &dm_channel_id, &message)
                .await
                .map(|()| dm_channel_id)
        }
        Err(error) => Err(error),
    };
    match delivered {
        Ok(dm_channel_id) => Ok((
            StatusCode::OK,
            Json(json!({
                "ok": true,
                "delivery": "owner_dm",
                "owner_user_id": owner_user_id,
                "dm_channel_id": dm_channel_id,
            })),
        )),
        Err(error) => Err(AppError::new(
            StatusCode::BAD_GATEWAY,
            ErrorCode::Discord,
            format!("owner dm delivery failed: {error}"),
        )),
    }
}

/// POST /api/internal/escalation/emit
pub async fn emit_escalation(
    State(state): State<AppState>,
    Json(body): Json<EmitEscalationBody>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    emit_escalation_with_base_url(&state, body, DISCORD_API_BASE).await
}

#[cfg(test)]
mod manual_decision_gate_tests {
    use super::*;
    use axum::{
        Json, Router,
        extract::{Path, State},
        response::IntoResponse,
        routing::post,
    };
    use std::{
        ffi::OsString,
        path::Path as FsPath,
        sync::{Arc, Mutex},
    };

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<OsString>,
    }

    impl EnvVarGuard {
        fn set_path(key: &'static str, value: &FsPath) -> Self {
            let previous = std::env::var_os(key);
            unsafe { std::env::set_var(key, value) };
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => unsafe { std::env::set_var(self.key, value) },
                None => unsafe { std::env::remove_var(self.key) },
            }
        }
    }

    fn write_test_bot_tokens(root: &FsPath) {
        let credential_dir = crate::runtime_layout::credential_dir(root);
        std::fs::create_dir_all(&credential_dir).unwrap();
        std::fs::write(
            crate::runtime_layout::credential_token_path(root, "announce"),
            "announce-token\n",
        )
        .unwrap();
        std::fs::write(
            crate::runtime_layout::credential_token_path(root, "notify"),
            "notify-token\n",
        )
        .unwrap();
    }

    fn test_engine_with_pg(pg_pool: sqlx::PgPool) -> crate::engine::PolicyEngine {
        let mut config = crate::config::Config::default();
        config.policies.dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("policies");
        config.policies.hot_reload = false;
        crate::engine::PolicyEngine::new_with_pg(&config, Some(pg_pool)).unwrap()
    }

    struct EscalationPgDatabase {
        _lifecycle: crate::db::postgres::PostgresTestLifecycleGuard,
        admin_url: String,
        database_name: String,
        database_url: String,
    }

    impl EscalationPgDatabase {
        async fn create() -> Self {
            let lifecycle = crate::db::postgres::lock_test_lifecycle();
            let base = crate::db::postgres::postgres_test_database_url_base()
                .expect("POSTGRES_TEST_DATABASE_URL_BASE required for escalation handler PG tests"); // agentdesk-audit: allow-unwrap — test-only fixture constructor requires an explicitly configured shared base
            let admin_url = if let Ok(url) = std::env::var("POSTGRES_TEST_ADMIN_URL") {
                let trimmed = url.trim();
                if trimmed.is_empty() {
                    format!("{base}/postgres")
                } else {
                    trimmed.to_string()
                }
            } else {
                format!("{base}/postgres")
            };
            let database_name = format!("agentdesk_escalation_{}", uuid::Uuid::new_v4().simple());
            let database_url = format!("{base}/{database_name}");
            crate::db::postgres::create_test_database(
                &admin_url,
                &database_name,
                "escalation handler pg",
            )
            .await
            .expect("create escalation postgres test db");

            Self {
                _lifecycle: lifecycle,
                admin_url,
                database_name,
                database_url,
            }
        }

        async fn migrate(&self) -> sqlx::PgPool {
            crate::db::postgres::connect_test_pool_and_migrate(
                &self.database_url,
                "escalation handler pg",
            )
            .await
            .expect("connect + migrate escalation postgres test db")
        }

        async fn drop(self) {
            crate::db::postgres::drop_test_database(
                &self.admin_url,
                &self.database_name,
                "escalation handler pg",
            )
            .await
            .expect("drop escalation postgres test db");
        }
    }

    #[tokio::test]
    async fn emit_escalation_pg_rejects_superseded_manual_decision_cards_without_discord_send() {
        let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let runtime_root = tempfile::tempdir().unwrap();
        let _env = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", runtime_root.path());
        write_test_bot_tokens(runtime_root.path());

        #[derive(Clone, Default)]
        struct MockDiscord {
            sent_messages: Arc<Mutex<Vec<String>>>,
        }

        async fn send_message(
            State(mock): State<MockDiscord>,
            Path(channel_id): Path<String>,
            Json(body): Json<serde_json::Value>,
        ) -> impl IntoResponse {
            mock.sent_messages.lock().unwrap().push(format!(
                "{channel_id}:{}",
                body["content"].as_str().unwrap_or("")
            ));
            (
                StatusCode::OK,
                Json(json!({"channel_id": channel_id, "content": body["content"]})),
            )
        }

        let mock = MockDiscord::default();
        let app = Router::new()
            .route("/channels/{channel_id}/messages", post(send_message))
            .with_state(mock.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let pg_db = EscalationPgDatabase::create().await;
        let pool = pg_db.migrate().await;

        for (card_id, status, review_status, blocked_reason) in [
            (
                "card-done-stale",
                "done",
                Some("dilemma_pending"),
                Some("stale manual marker"),
            ),
            (
                "card-backlog-stale",
                "backlog",
                Some("dilemma_pending"),
                Some("stale manual marker"),
            ),
        ] {
            sqlx::query(
                "INSERT INTO kanban_cards (
                    id, title, status, priority, review_status, blocked_reason, created_at, updated_at
                 )
                 VALUES ($1, $2, $3, 'medium', $4, $5, NOW(), NOW())",
            )
            .bind(card_id)
            .bind(format!("Superseded {card_id}"))
            .bind(status)
            .bind(review_status)
            .bind(blocked_reason)
            .execute(&pool)
            .await
            .unwrap();
        }

        let mut config = crate::config::Config::default();
        config.discord.owner_id = Some(111);
        let tx = crate::server::ws::new_broadcast();
        let buf = crate::server::ws::spawn_batch_flusher(tx.clone());
        let mut state = AppState {
            pg_pool: Some(pool.clone()),
            engine: test_engine_with_pg(pool.clone()),
            config: std::sync::Arc::new(crate::config::Config::default()),
            broadcast_tx: tx,
            batch_buffer: buf,
            health_registry: None,
            cluster_instance_id: None,
        };
        state.config = std::sync::Arc::new(config);

        for card_id in ["card-done-stale", "card-backlog-stale"] {
            let (status, Json(body)) = emit_escalation_with_base_url(
                &state,
                EmitEscalationBody {
                    card_id: card_id.to_string(),
                    reasons: vec!["stale escalation retry".to_string()],
                },
                &format!("http://{addr}"),
            )
            .await
            .expect("superseded escalation remains a flat response");
            assert_eq!(status, StatusCode::CONFLICT, "{card_id}");
            assert_eq!(body["state"], json!("superseded"), "{card_id}");
        }

        assert!(
            mock.sent_messages.lock().unwrap().is_empty(),
            "superseded escalation attempts must not send Discord messages"
        );

        server.abort();
        pool.close().await;
        pg_db.drop().await;
    }
    #[derive(Clone, Default)]
    struct MockDiscordDm {
        dm_recipients: Arc<Mutex<Vec<String>>>,
        sent_messages: Arc<Mutex<Vec<String>>>,
    }

    async fn open_dm(
        State(mock): State<MockDiscordDm>,
        Json(body): Json<serde_json::Value>,
    ) -> impl IntoResponse {
        let recipient = body["recipient_id"].as_str().unwrap_or("").to_string();
        mock.dm_recipients.lock().unwrap().push(recipient.clone());
        (
            StatusCode::OK,
            Json(json!({"id": format!("dm-{recipient}"), "type": 1})),
        )
    }

    async fn send_dm_message(
        State(mock): State<MockDiscordDm>,
        Path(channel_id): Path<String>,
        Json(body): Json<serde_json::Value>,
    ) -> impl IntoResponse {
        mock.sent_messages.lock().unwrap().push(format!(
            "{channel_id}:{}",
            body["content"].as_str().unwrap_or("")
        ));
        (StatusCode::OK, Json(json!({"channel_id": channel_id})))
    }

    fn test_state(pool: &sqlx::PgPool, owner_id: Option<u64>) -> AppState {
        let mut config = crate::config::Config::default();
        config.discord.owner_id = owner_id;
        let tx = crate::server::ws::new_broadcast();
        let buf = crate::server::ws::spawn_batch_flusher(tx.clone());
        AppState {
            pg_pool: Some(pool.clone()),
            engine: test_engine_with_pg(pool.clone()),
            config: std::sync::Arc::new(config),
            broadcast_tx: tx,
            batch_buffer: buf,
            health_registry: None,
            cluster_instance_id: None,
        }
    }

    /// Emit goes only to the `discord.owner_id` DM; a leftover routing
    /// override row is ignored, and a missing owner is an error with no send.
    #[tokio::test]
    async fn emit_escalation_pg_sends_only_to_configured_owner_dm() {
        let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let runtime_root = tempfile::tempdir().unwrap();
        let _env = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", runtime_root.path());
        write_test_bot_tokens(runtime_root.path());

        let mock = MockDiscordDm::default();
        let app = Router::new()
            .route("/users/@me/channels", post(open_dm))
            .route("/channels/{channel_id}/messages", post(send_dm_message))
            .with_state(mock.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let pg_db = EscalationPgDatabase::create().await;
        let pool = pg_db.migrate().await;
        sqlx::query(
            "INSERT INTO kanban_cards (
                id, title, status, priority, review_status, created_at, updated_at
             )
             VALUES ('card-active', 'Active card', 'review', 'medium', 'dilemma_pending', NOW(), NOW())",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO kv_meta (key, value) VALUES ($1, $2)")
            .bind(concat!("escalation-settings", "-override"))
            .bind(r#"{"mode":"pm","owner_user_id":999,"pm_channel_id":"222"}"#)
            .execute(&pool)
            .await
            .unwrap();

        let body = || EmitEscalationBody {
            card_id: "card-active".to_string(),
            reasons: vec!["needs a person".to_string()],
        };
        let base_url = format!("http://{addr}");
        let (status, Json(response)) =
            emit_escalation_with_base_url(&test_state(&pool, Some(111)), body(), &base_url)
                .await
                .expect("owner DM delivery succeeds");
        assert_eq!(status, StatusCode::OK);
        assert_eq!(response["delivery"], json!("owner_dm"));
        assert_eq!(response["owner_user_id"], json!(111));
        assert_eq!(*mock.dm_recipients.lock().unwrap(), vec!["111".to_string()]);
        let sent = mock.sent_messages.lock().unwrap().clone();
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert!(sent[0].starts_with("dm-111:"), "{sent:?}");
        assert!(sent[0].contains("Active card") && sent[0].contains("needs a person"));

        let error = emit_escalation_with_base_url(&test_state(&pool, None), body(), &base_url)
            .await
            .expect_err("missing owner is reported, not dropped");
        assert_eq!(error.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(error.message().contains("discord.owner_id"));
        assert_eq!(mock.dm_recipients.lock().unwrap().len(), 1);
        assert_eq!(mock.sent_messages.lock().unwrap().len(), 1);

        server.abort();
        pool.close().await;
        pg_db.drop().await;
    }
}

#[cfg(test)]
mod retired_settings_route_tests {
    use crate::server::routes::AppState;
    use axum::{
        body::Body,
        extract::ConnectInfo,
        http::{Request, StatusCode},
    };
    use tower::ServiceExt;

    fn loopback_request(method: &str, path: &str, body: &str) -> Request<Body> {
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        request.extensions_mut().insert(ConnectInfo(
            "127.0.0.1:1234".parse::<std::net::SocketAddr>().unwrap(),
        ));
        request
    }

    /// The routing-settings API is gone; the emit route stays mounted.
    #[tokio::test]
    async fn escalation_settings_route_is_gone_and_emit_stays_mounted() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = crate::config::Config::default();
        config.policies.dir = dir.path().join("policies");
        config.policies.hot_reload = false;
        config.data.dir = dir.path().join("data");
        std::fs::create_dir_all(&config.policies.dir).unwrap();
        let state = AppState {
            engine: crate::engine::PolicyEngine::new_with_pg(&config, None).unwrap(),
            config: std::sync::Arc::new(config),
            pg_pool: None,
            broadcast_tx: crate::eventbus::new_broadcast(),
            batch_buffer: Default::default(),
            health_registry: None,
            cluster_instance_id: None,
        };
        let app = super::super::compose_api_router(state.clone()).with_state(state);
        for (method, body) in [("GET", ""), ("PUT", r#"{"mode":"user"}"#)] {
            let response = app
                .clone()
                .oneshot(loopback_request(method, "/settings/escalation", body))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{method}");
        }
        let emit = app
            .oneshot(loopback_request(
                "POST",
                "/internal/escalation/emit",
                r#"{"card_id":"card-1","reasons":["needs a person"]}"#,
            ))
            .await
            .unwrap();
        assert_eq!(emit.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}
