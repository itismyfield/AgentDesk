use serde::Serialize;
use serde_json::{Value, json};
use sqlx::{PgPool, Row};
use std::future::Future;

use crate::db::Db;
use crate::services::discord::settings::{
    MemoryBackendKind, ResolvedMemorySettings, resolve_memory_settings,
};
use crate::services::memory::{
    MementoBackend, MementoRememberRequest, sanitize_memento_workspace_segment,
};

const MAX_SUMMARY_CHARS: usize = 220;
const MAX_FEEDBACK_CHARS: usize = 240;
const MAX_CONTENT_CHARS: usize = 900;

#[derive(Clone, Debug, Serialize)]
struct RetrospectiveMemoryPayload {
    content: String,
    topic: String,
    #[serde(rename = "type")]
    kind: String,
    keywords: Vec<String>,
    source: String,
    workspace: String,
    #[serde(rename = "agentId")]
    agent_id: String,
    #[serde(rename = "caseId")]
    case_id: String,
    goal: String,
    outcome: String,
    phase: String,
    #[serde(rename = "resolutionStatus")]
    resolution_status: String,
    #[serde(rename = "assertionStatus")]
    assertion_status: String,
    #[serde(rename = "contextSummary")]
    context_summary: String,
}

#[derive(Clone, Debug)]
struct RetrospectiveDraft {
    dispatch_id: String,
    terminal_status: String,
    issue_number: Option<i64>,
    repo_id: Option<String>,
    title: String,
    review_round: i64,
    review_notes: Option<String>,
    duration_seconds: Option<i64>,
    success: bool,
    result_json: String,
    memory_payload: RetrospectiveMemoryPayload,
}

pub(crate) fn record_card_retrospective_json(
    db: Option<&Db>,
    pg_pool: Option<&PgPool>,
    card_id: &str,
    terminal_status: &str,
) -> String {
    match record_card_retrospective(db, pg_pool, card_id, terminal_status) {
        Ok(value) => value.to_string(),
        Err(error) => json!({
            "ok": false,
            "error": error,
        })
        .to_string(),
    }
}

fn record_card_retrospective(
    _db: Option<&Db>,
    pg_pool: Option<&PgPool>,
    card_id: &str,
    terminal_status: &str,
) -> Result<Value, String> {
    // PG card_retrospectives rows are authoritative once a pool is attached.
    let card_id = card_id.trim().to_string();
    let terminal_status = terminal_status.trim().to_string();

    if card_id.is_empty() {
        return Err("card_id is required".to_string());
    }
    if terminal_status.is_empty() {
        return Err("terminal_status is required".to_string());
    }

    if let Some(pg_pool) = pg_pool.cloned() {
        return run_async_bridge_pg(&pg_pool, move |pool| async move {
            record_card_retrospective_pg(&pool, &card_id, &terminal_status).await
        });
    }

    Err("postgres backend is unavailable".to_string())
}

async fn record_card_retrospective_pg(
    pg_pool: &PgPool,
    card_id: &str,
    terminal_status: &str,
) -> Result<Value, String> {
    let sync_settings = resolve_memory_settings(None, None);
    let has_runtime = tokio::runtime::Handle::try_current().is_ok();
    let sync_backend = Some(sync_settings.backend.as_str());
    let sync_status = match sync_settings.backend {
        MemoryBackendKind::Memento if has_runtime => "queued",
        MemoryBackendKind::Memento => "skipped_no_runtime",
        _ => "skipped_backend",
    };

    let Some(draft) = build_retrospective_draft_pg(pg_pool, card_id, terminal_status).await? else {
        return Ok(json!({
            "ok": true,
            "skipped": true,
            "reason": "no_completed_dispatch_result",
        }));
    };

    let retrospective_id = uuid::Uuid::new_v4().to_string();
    let memory_payload_json = serde_json::to_value(&draft.memory_payload)
        .map_err(|err| format!("serialize memory payload: {err}"))?;
    let result_json_value =
        parse_result_json_value(&draft.result_json, "retrospective draft result_json");

    let inserted = sqlx::query(
        "INSERT INTO card_retrospectives (
            id, card_id, dispatch_id, terminal_status, repo_id, issue_number, title, topic,
            content, review_round, review_notes, duration_seconds, success, result_json,
            memory_payload, sync_backend, sync_status, created_at, updated_at
         ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, $8,
            $9, $10, $11, $12, $13, $14,
            $15, $16, $17, NOW(), NOW()
         )
         ON CONFLICT (card_id, dispatch_id, terminal_status) DO NOTHING",
    )
    .bind(&retrospective_id)
    .bind(card_id)
    .bind(&draft.dispatch_id)
    .bind(&draft.terminal_status)
    .bind(&draft.repo_id)
    .bind(draft.issue_number)
    .bind(&draft.title)
    .bind(&draft.memory_payload.topic)
    .bind(&draft.memory_payload.content)
    .bind(draft.review_round)
    .bind(&draft.review_notes)
    .bind(draft.duration_seconds)
    .bind(draft.success)
    .bind(result_json_value)
    .bind(memory_payload_json)
    .bind(sync_backend)
    .bind(sync_status)
    .execute(pg_pool)
    .await
    .map_err(|err| format!("insert card_retrospectives: {err}"))?
    .rows_affected();

    if inserted == 0 {
        return Ok(json!({
            "ok": true,
            "skipped": true,
            "reason": "duplicate",
        }));
    }

    if sync_settings.backend == MemoryBackendKind::Memento && has_runtime {
        let pg_pool_clone = pg_pool.clone();
        let retrospective_id_clone = retrospective_id.clone();
        let remember_request = MementoRememberRequest {
            content: draft.memory_payload.content.clone(),
            topic: draft.memory_payload.topic.clone(),
            kind: draft.memory_payload.kind.clone(),
            importance: None,
            keywords: draft.memory_payload.keywords.clone(),
            source: Some(draft.memory_payload.source.clone()),
            workspace: Some(draft.memory_payload.workspace.clone()),
            global: false,
            channel_id: None,
            channel_name: None,
            agent_id: Some(draft.memory_payload.agent_id.clone()),
            case_id: Some(draft.memory_payload.case_id.clone()),
            goal: Some(draft.memory_payload.goal.clone()),
            outcome: Some(draft.memory_payload.outcome.clone()),
            phase: Some(draft.memory_payload.phase.clone()),
            resolution_status: Some(draft.memory_payload.resolution_status.clone()),
            assertion_status: Some(draft.memory_payload.assertion_status.clone()),
            context_summary: Some(draft.memory_payload.context_summary.clone()),
        };

        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                sync_retrospective_to_memento(
                    pg_pool_clone,
                    retrospective_id_clone,
                    sync_settings,
                    remember_request,
                )
                .await;
            });
        }
    }

    Ok(json!({
        "ok": true,
        "skipped": false,
        "retrospective_id": retrospective_id,
        "dispatch_id": draft.dispatch_id,
        "sync_backend": sync_backend,
        "sync_status": sync_status,
    }))
}

async fn sync_retrospective_to_memento(
    pg_pool: PgPool,
    retrospective_id: String,
    settings: ResolvedMemorySettings,
    request: MementoRememberRequest,
) {
    let backend = MementoBackend::new(settings);
    let result = backend.remember(request).await;

    let query = match result {
        Ok(_) => sqlx::query(
            "UPDATE card_retrospectives
             SET sync_status = 'stored', sync_error = NULL, updated_at = NOW()
             WHERE id = $1",
        )
        .bind(&retrospective_id),
        Err(error) => sqlx::query(
            "UPDATE card_retrospectives
             SET sync_status = 'failed', sync_error = $1, updated_at = NOW()
             WHERE id = $2",
        )
        .bind(error)
        .bind(&retrospective_id),
    };
    if let Err(error) = query.execute(&pg_pool).await {
        tracing::warn!("failed to update PG retrospective sync status: {error}");
    }
}

fn run_async_bridge_pg<F, T>(
    pool: &PgPool,
    future_factory: impl FnOnce(PgPool) -> F + Send + 'static,
) -> Result<T, String>
where
    F: Future<Output = Result<T, String>> + Send + 'static,
    T: Send + 'static,
{
    crate::utils::async_bridge::block_on_pg_result(pool, future_factory, |error| error)
}

async fn build_retrospective_draft_pg(
    pg_pool: &PgPool,
    card_id: &str,
    terminal_status: &str,
) -> Result<Option<RetrospectiveDraft>, String> {
    let card = sqlx::query(
        "SELECT kc.title,
                kc.github_issue_number::BIGINT AS github_issue_number,
                kc.repo_id,
                GREATEST(COALESCE(crs.review_round, 0), COALESCE(kc.review_round, 0))::BIGINT AS review_round,
                kc.review_notes
         FROM kanban_cards kc
         LEFT JOIN card_review_state crs ON crs.card_id = kc.id
         WHERE kc.id = $1",
    )
    .bind(card_id)
    .fetch_one(pg_pool)
    .await
    .map_err(|err| format!("load card retrospective source: {err}"))?;

    let latest_dispatch = sqlx::query(
        "SELECT id,
                dispatch_type,
                result,
                ROUND(EXTRACT(EPOCH FROM (COALESCE(completed_at, updated_at, created_at) - created_at)))::BIGINT AS duration_seconds
         FROM task_dispatches
         WHERE kanban_card_id = $1
           AND status = 'completed'
           AND result IS NOT NULL
           AND BTRIM(result) <> ''
         ORDER BY COALESCE(completed_at, updated_at, created_at) DESC, id DESC
         LIMIT 1",
    )
    .bind(card_id)
    .fetch_optional(pg_pool)
    .await
    .map_err(|err| format!("load latest dispatch result: {err}"))?;

    let Some(latest_dispatch) = latest_dispatch else {
        return Ok(None);
    };

    let title_raw = card
        .try_get::<String, _>("title")
        .map_err(|err| format!("decode retrospective title: {err}"))?;
    let issue_number = card
        .try_get::<Option<i64>, _>("github_issue_number")
        .map_err(|err| format!("decode retrospective issue number: {err}"))?;
    let repo_id = card
        .try_get::<Option<String>, _>("repo_id")
        .map_err(|err| format!("decode retrospective repo id: {err}"))?;
    let review_round = card
        .try_get::<i64, _>("review_round")
        .map_err(|err| format!("decode retrospective review round: {err}"))?;
    let review_notes_raw = card
        .try_get::<Option<String>, _>("review_notes")
        .map_err(|err| format!("decode retrospective review notes: {err}"))?;

    let dispatch_id = latest_dispatch
        .try_get::<String, _>("id")
        .map_err(|err| format!("decode retrospective dispatch id: {err}"))?;
    let dispatch_type = latest_dispatch
        .try_get::<Option<String>, _>("dispatch_type")
        .map_err(|err| format!("decode retrospective dispatch type: {err}"))?;
    let result_json = latest_dispatch
        .try_get::<String, _>("result")
        .map_err(|err| format!("decode retrospective dispatch result: {err}"))?;
    let duration_seconds = latest_dispatch
        .try_get::<Option<i64>, _>("duration_seconds")
        .map_err(|err| format!("decode retrospective dispatch duration: {err}"))?;

    let result_value =
        parse_result_json_value(&result_json, "postgres dispatch retrospective result");
    let title = normalize_whitespace(&title_raw);
    let review_notes = review_notes_raw
        .as_deref()
        .map(normalize_whitespace)
        .filter(|value| !value.is_empty());
    let summary = extract_summary_from_result(&result_value)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| title.clone());
    let success = terminal_status.trim().eq_ignore_ascii_case("done");
    let issue_label = issue_number
        .map(|value| format!("#{value}"))
        .unwrap_or_else(|| format!("card {card_id}"));
    let status_label = if success { "성공" } else { "실패" };
    let content = build_retrospective_content(
        &issue_label,
        &title,
        &summary,
        review_round,
        review_notes.as_deref(),
        duration_seconds,
        status_label,
    );
    let topic = issue_number
        .map(|value| format!("issue-{value}"))
        .unwrap_or_else(|| default_topic_from_title(&title));
    let workspace = repo_id
        .as_deref()
        .map(sanitize_memento_workspace_segment)
        .unwrap_or_else(|| "agentdesk".to_string());
    let dispatch_label = dispatch_type
        .as_deref()
        .map(normalize_whitespace)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "implementation".to_string());
    let keywords = build_keywords(issue_number, &title, &dispatch_label, success);

    Ok(Some(RetrospectiveDraft {
        dispatch_id: dispatch_id.clone(),
        terminal_status: terminal_status.trim().to_string(),
        issue_number,
        repo_id,
        title: title.clone(),
        review_round,
        review_notes,
        duration_seconds: duration_seconds.filter(|value| *value > 0),
        success,
        result_json,
        memory_payload: RetrospectiveMemoryPayload {
            content,
            topic,
            kind: "episode".to_string(),
            keywords,
            source: format!("card:{card_id}/dispatch:{dispatch_id}"),
            workspace,
            agent_id: "default".to_string(),
            case_id: issue_number
                .map(|value| format!("issue-{value}"))
                .unwrap_or_else(|| card_id.to_string()),
            goal: title.clone(),
            outcome: format!("{status_label} 종료"),
            phase: "retrospective".to_string(),
            resolution_status: if success {
                "resolved".to_string()
            } else {
                "abandoned".to_string()
            },
            assertion_status: "verified".to_string(),
            context_summary: format!(
                "Kanban terminal retrospective for {issue_label} after a completed {dispatch_label} dispatch."
            ),
        },
    }))
}

fn build_retrospective_content(
    issue_label: &str,
    title: &str,
    summary: &str,
    review_round: i64,
    review_notes: Option<&str>,
    duration_seconds: Option<i64>,
    status_label: &str,
) -> String {
    let mut segments = vec![
        format!("AgentDesk 이슈 {issue_label} ({title}) 작업은 {status_label}으로 종료되었다."),
        format!("핵심 작업 요약: {}.", trim_trailing_punctuation(summary)),
    ];

    let mut trailing = Vec::new();
    if review_round > 0 {
        trailing.push(format!("review {}라운드", review_round));
    }
    if let Some(duration) = format_duration(duration_seconds) {
        trailing.push(format!("소요 시간 {}", duration));
    }
    if let Some(notes) = review_notes
        .map(|value| truncate_chars(value, MAX_FEEDBACK_CHARS))
        .filter(|value| !value.is_empty())
    {
        trailing.push(format!(
            "주요 review feedback {}",
            trim_trailing_punctuation(&notes)
        ));
    }
    if !trailing.is_empty() {
        segments.push(format!("{}.", trailing.join(", ")));
    }

    truncate_chars(&segments.join(" "), MAX_CONTENT_CHARS)
}

fn build_keywords(
    issue_number: Option<i64>,
    title: &str,
    dispatch_type: &str,
    success: bool,
) -> Vec<String> {
    let mut keywords = Vec::new();
    if let Some(issue_number) = issue_number {
        keywords.push(format!("issue-{issue_number}"));
    }
    keywords.push(dispatch_type.to_ascii_lowercase());
    keywords.push(if success { "success" } else { "failure" }.to_string());
    for token in title
        .split(|ch: char| !ch.is_alphanumeric())
        .map(str::trim)
        .filter(|part| part.len() >= 3)
    {
        let lowered = token.to_ascii_lowercase();
        if !keywords.iter().any(|existing| existing == &lowered) {
            keywords.push(lowered);
        }
        if keywords.len() >= 6 {
            break;
        }
    }
    keywords
}

fn extract_summary_from_result(value: &Value) -> Option<String> {
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
        Value::String(text) => {
            let normalized = normalize_whitespace(text);
            if normalized.is_empty() {
                None
            } else {
                Some(truncate_chars(&normalized, MAX_SUMMARY_CHARS))
            }
        }
        Value::Object(map) => {
            for key in PREFERRED_KEYS {
                if let Some(summary) = map.get(*key).and_then(extract_summary_from_result) {
                    return Some(summary);
                }
            }
            map.values().find_map(extract_summary_from_result)
        }
        Value::Array(items) => items.iter().find_map(extract_summary_from_result),
        _ => None,
    }
}

fn default_topic_from_title(title: &str) -> String {
    let joined = title
        .split_whitespace()
        .take(6)
        .collect::<Vec<_>>()
        .join("-");
    if joined.trim().is_empty() {
        "card-retrospective".to_string()
    } else {
        sanitize_memento_workspace_segment(&joined)
    }
}

fn normalize_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn parse_result_json_value(raw: &str, context: &'static str) -> Value {
    match serde_json::from_str::<Value>(raw) {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(
                "[retrospectives] malformed JSON in {context}; falling back to string value: {error}"
            );
            Value::String(normalize_whitespace(raw))
        }
    }
}

fn trim_trailing_punctuation(value: &str) -> String {
    value
        .trim()
        .trim_end_matches(|ch: char| matches!(ch, '.' | '!' | '?'))
        .to_string()
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    if max_chars <= 3 {
        return value.chars().take(max_chars).collect();
    }
    let mut shortened = value.chars().take(max_chars - 3).collect::<String>();
    shortened.push_str("...");
    shortened
}

fn format_duration(duration_seconds: Option<i64>) -> Option<String> {
    let total_seconds = duration_seconds?;
    if total_seconds <= 0 {
        return None;
    }
    let total_minutes = (total_seconds + 59) / 60;
    if total_minutes < 60 {
        return Some(format!("{total_minutes}분"));
    }
    let hours = total_minutes / 60;
    let minutes = total_minutes % 60;
    if minutes == 0 {
        Some(format!("{hours}시간"))
    } else {
        Some(format!("{hours}시간 {minutes}분"))
    }
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::*;
    use std::io::{self, Write};
    use std::sync::{Arc, Mutex};

    #[derive(Clone)]
    struct TestLogWriter {
        buffer: Arc<Mutex<Vec<u8>>>,
    }

    impl Write for TestLogWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.buffer.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn capture_logs<T>(run: impl FnOnce() -> T) -> (T, String) {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let log_buffer = buffer.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::WARN)
            .with_ansi(false)
            .without_time()
            .with_writer(move || TestLogWriter {
                buffer: log_buffer.clone(),
            })
            .finish();

        let result = tracing::subscriber::with_default(subscriber, run);
        let captured = buffer.lock().unwrap().clone();
        (result, String::from_utf8_lossy(&captured).to_string())
    }

    struct TestPostgresDb {
        admin_url: String,
        database_name: String,
        database_url: String,
    }

    impl TestPostgresDb {
        async fn create() -> Self {
            let admin_url = postgres_admin_database_url();
            let database_name =
                format!("agentdesk_retrospectives_{}", uuid::Uuid::new_v4().simple());
            let database_url = format!("{}/{}", postgres_base_database_url(), database_name);
            let admin_pool = sqlx::PgPool::connect(&admin_url).await.unwrap();
            sqlx::query(&format!("CREATE DATABASE \"{database_name}\""))
                .execute(&admin_pool)
                .await
                .unwrap();
            admin_pool.close().await;
            Self {
                admin_url,
                database_name,
                database_url,
            }
        }

        async fn migrate(&self) -> sqlx::PgPool {
            let pool = sqlx::PgPool::connect(&self.database_url).await.unwrap();
            crate::db::postgres::migrate(&pool).await.unwrap();
            pool
        }

        async fn drop(self) {
            let admin_pool = sqlx::PgPool::connect(&self.admin_url).await.unwrap();
            sqlx::query(
                "SELECT pg_terminate_backend(pid)
                 FROM pg_stat_activity
                 WHERE datname = $1
                   AND pid <> pg_backend_pid()",
            )
            .bind(&self.database_name)
            .execute(&admin_pool)
            .await
            .unwrap();
            sqlx::query(&format!(
                "DROP DATABASE IF EXISTS \"{}\"",
                self.database_name
            ))
            .execute(&admin_pool)
            .await
            .unwrap();
            admin_pool.close().await;
        }
    }

    fn postgres_base_database_url() -> String {
        if let Ok(base) = std::env::var("POSTGRES_TEST_DATABASE_URL_BASE") {
            let trimmed = base.trim();
            if !trimmed.is_empty() {
                return trimmed.trim_end_matches('/').to_string();
            }
        }

        let user = std::env::var("PGUSER")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| {
                std::env::var("USER")
                    .ok()
                    .filter(|value| !value.trim().is_empty())
            })
            .unwrap_or_else(|| "postgres".to_string());
        let password = std::env::var("PGPASSWORD")
            .ok()
            .filter(|value| !value.trim().is_empty());
        let host = std::env::var("PGHOST")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "localhost".to_string());
        let port = std::env::var("PGPORT")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "5432".to_string());

        match password {
            Some(password) => format!("postgresql://{user}:{password}@{host}:{port}"),
            None => format!("postgresql://{user}@{host}:{port}"),
        }
    }

    fn postgres_admin_database_url() -> String {
        let admin_db = std::env::var("POSTGRES_TEST_ADMIN_DB")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "postgres".to_string());
        format!("{}/{}", postgres_base_database_url(), admin_db)
    }

    #[test]
    fn parse_result_json_value_logs_warn_and_preserves_string_fallback() {
        let (value, logs) = capture_logs(|| {
            parse_result_json_value("{bad json", "retrospective draft result_json")
        });

        assert_eq!(value, Value::String("{bad json".to_string()));
        assert!(
            logs.contains("[retrospectives] malformed JSON in retrospective draft result_json")
        );
    }

    #[test]
    #[ignore = "SQLite retrospective service path removed for #868; persistence/skipped behavior now belongs to Postgres coverage and route-level integration fixtures."]
    fn sqlite_retrospective_unit_coverage_obsolete_after_pg_migration() {}

    #[tokio::test]
    async fn record_card_retrospective_persists_pg_episode() {
        let pg_db = TestPostgresDb::create().await;
        let pg_pool = pg_db.migrate().await;

        sqlx::query(
            "INSERT INTO agents (id, name, provider, status)
             VALUES ($1, $2, $3, $4)",
        )
        .bind("agent-1")
        .bind("Agent")
        .bind("codex")
        .bind("idle")
        .execute(&pg_pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO kanban_cards (
                id, title, status, assigned_agent_id, github_issue_number, review_round, review_notes,
                created_at, updated_at
             ) VALUES (
                $1, $2, $3, $4, $5, $6, $7, NOW(), NOW()
             )",
        )
        .bind("card-retro-pg")
        .bind("PG retrospective")
        .bind("review")
        .bind("agent-1")
        .bind(478_i64)
        .bind(1_i64)
        .bind("policy broker 확대")
        .execute(&pg_pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, result,
                created_at, updated_at, completed_at
             ) VALUES (
                $1, $2, $3, $4, $5, $6, $7,
                NOW() - INTERVAL '15 minutes', NOW(), NOW()
             )",
        )
        .bind("dispatch-retro-pg")
        .bind("card-retro-pg")
        .bind("agent-1")
        .bind("implementation")
        .bind("completed")
        .bind("Done")
        .bind(
            json!({
                "summary": "PG retrospective path inserted through sqlx"
            })
            .to_string(),
        )
        .execute(&pg_pool)
        .await
        .unwrap();

        let payload = serde_json::from_str::<Value>(&record_card_retrospective_json(
            None,
            Some(&pg_pool),
            "card-retro-pg",
            "done",
        ))
        .unwrap();
        eprintln!("retrospective_payload={payload}");
        assert_eq!(payload["ok"], Value::Bool(true), "payload={payload}");
        assert_eq!(payload["skipped"], Value::Bool(false), "payload={payload}");

        let stored = sqlx::query_as::<_, (String, String, bool)>(
            "SELECT topic, content, success
             FROM card_retrospectives
             WHERE card_id = $1",
        )
        .bind("card-retro-pg")
        .fetch_one(&pg_pool)
        .await
        .unwrap();
        assert_eq!(stored.0, "issue-478");
        assert!(stored.1.contains("AgentDesk 이슈 #478"));
        assert!(stored.1.contains("sqlx"));
        assert!(stored.2);

        pg_pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn record_card_retrospective_skips_pg_without_completed_dispatch_result() {
        let pg_db = TestPostgresDb::create().await;
        let pg_pool = pg_db.migrate().await;

        sqlx::query(
            "INSERT INTO agents (id, name, provider, status)
             VALUES ($1, $2, $3, $4)",
        )
        .bind("agent-1")
        .bind("Agent")
        .bind("codex")
        .bind("idle")
        .execute(&pg_pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO kanban_cards (
                id, title, status, assigned_agent_id, created_at, updated_at
             ) VALUES (
                $1, $2, $3, $4, NOW(), NOW()
             )",
        )
        .bind("card-retro-skip-pg")
        .bind("Manual done")
        .bind("review")
        .bind("agent-1")
        .execute(&pg_pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title,
                created_at, updated_at, completed_at
             ) VALUES (
                $1, $2, $3, $4, $5, $6,
                NOW() - INTERVAL '5 minutes', NOW(), NOW()
             )",
        )
        .bind("dispatch-retro-skip-pg")
        .bind("card-retro-skip-pg")
        .bind("agent-1")
        .bind("implementation")
        .bind("completed")
        .bind("Done")
        .execute(&pg_pool)
        .await
        .unwrap();

        let payload = serde_json::from_str::<Value>(&record_card_retrospective_json(
            None,
            Some(&pg_pool),
            "card-retro-skip-pg",
            "done",
        ))
        .unwrap();
        assert_eq!(payload["skipped"], Value::Bool(true));
        assert_eq!(
            payload["reason"],
            Value::String("no_completed_dispatch_result".to_string())
        );

        let count =
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*)::BIGINT FROM card_retrospectives")
                .fetch_one(&pg_pool)
                .await
                .unwrap();
        assert_eq!(count, 0);

        pg_pool.close().await;
        pg_db.drop().await;
    }
}
