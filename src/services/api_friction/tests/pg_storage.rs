use super::super::*;
use super::helpers::{TestPostgresDb, restore_env};
use crate::services::discord::settings::{MemoryBackendKind, ResolvedMemorySettings};
use std::fs;

#[tokio::test]
async fn list_api_friction_patterns_counts_repeated_rows() {
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;
    sqlx::query(
        "INSERT INTO api_friction_events (
            id, fingerprint, endpoint, friction_type, summary, keywords_json, payload_json,
            channel_id, provider, repo_id, memory_backend, memory_status, created_at
         ) VALUES (
            $1, $2, $3, $4, $5,
            '[]'::jsonb, '{}'::jsonb, $6, $7, $8, $9, $10, NOW() - INTERVAL '2 minutes'
         )",
    )
    .bind("event-1")
    .bind("api-docs-kanban::docs-bypass")
    .bind("/api/docs/kanban")
    .bind("docs-bypass")
    .bind("first")
    .bind("1")
    .bind("codex")
    .bind("itismyfield/AgentDesk")
    .bind("memento")
    .bind("stored")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO api_friction_events (
            id, fingerprint, endpoint, friction_type, summary, keywords_json, payload_json,
            channel_id, provider, repo_id, memory_backend, memory_status, created_at
         ) VALUES (
            $1, $2, $3, $4, $5,
            '[]'::jsonb, '{}'::jsonb, $6, $7, $8, $9, $10, NOW() - INTERVAL '1 minute'
         )",
    )
    .bind("event-2")
    .bind("api-docs-kanban::docs-bypass")
    .bind("/api/docs/kanban")
    .bind("docs-bypass")
    .bind("second")
    .bind("1")
    .bind("codex")
    .bind("itismyfield/AgentDesk")
    .bind("memento")
    .bind("stored")
    .execute(&pg_pool)
    .await
    .unwrap();

    let patterns = load_pattern_candidates_pg(
        &pg_pool,
        API_FRICTION_MIN_REPEAT_COUNT,
        DEFAULT_PATTERN_LIMIT,
    )
    .await
    .unwrap();
    assert_eq!(patterns.len(), 1);
    assert_eq!(patterns[0].event_count, 2);
    assert_eq!(patterns[0].summary, "second");
    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn record_api_friction_reports_uses_pg_only_when_sqlite_handle_present() {
    let _guard = crate::services::discord::runtime_store::lock_test_env();
    let temp = tempfile::tempdir().unwrap();
    let previous_root = std::env::var_os("AGENTDESK_ROOT_DIR");
    let config_dir = temp.path().join("config");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("agentdesk.yaml"),
        "server:\n  port: 8791\nmemory:\n  backend: file\n",
    )
    .unwrap();
    unsafe {
        std::env::set_var("AGENTDESK_ROOT_DIR", temp.path());
    }

    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;
    let sqlite_db = crate::db::test_db();

    let result = record_api_friction_reports(
        Some(&sqlite_db),
        Some(&pg_pool),
        &ResolvedMemorySettings {
            backend: MemoryBackendKind::File,
            ..ResolvedMemorySettings::default()
        },
        ApiFrictionRecordContext {
            channel_id: 1,
            session_key: Some("host:session"),
            dispatch_id: None,
            provider: "codex",
        },
        &[ApiFrictionReport {
            endpoint: "/api/docs/kanban".to_string(),
            friction_type: "docs-bypass".to_string(),
            summary: "category guessing".to_string(),
            workaround: Some("sqlite3".to_string()),
            suggested_fix: Some("document a single endpoint".to_string()),
            docs_category: Some("kanban".to_string()),
            keywords: vec!["/api/docs/kanban".to_string(), "sqlite3".to_string()],
        }],
    )
    .await
    .unwrap();

    assert_eq!(result.stored_event_count, 1);
    assert_eq!(result.memory_stored_count, 0);
    assert!(result.memory_errors.is_empty(), "{result:?}");

    let sqlite_count: i64 = sqlite_db
        .lock()
        .unwrap()
        .query_row("SELECT COUNT(*) FROM api_friction_events", [], |row| {
            row.get(0)
        })
        .unwrap();
    let pg_row = sqlx::query_as::<_, (String, String)>(
        "SELECT id, memory_status
         FROM api_friction_events
         LIMIT 1",
    )
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    assert_eq!(sqlite_count, 0);
    assert_eq!(pg_row.1, "skipped_backend");

    restore_env("AGENTDESK_ROOT_DIR", previous_root);
    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn load_dispatch_source_context_pg_reads_bigint_issue_numbers() {
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;

    sqlx::query(
        "INSERT INTO kanban_cards (id, repo_id, title, status, github_issue_number, created_at, updated_at)
         VALUES ($1, $2, $3, $4, $5, NOW(), NOW())",
    )
    .bind("card-api-friction-bigint")
    .bind("itismyfield/AgentDesk")
    .bind("API friction card")
    .bind("in_progress")
    .bind(3_000_000_123_i64)
    .execute(&pg_pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at
         ) VALUES ($1, $2, $3, $4, $5, $6, NOW(), NOW())",
    )
    .bind("dispatch-api-friction-bigint")
    .bind("card-api-friction-bigint")
    .bind("agent-1")
    .bind("implementation")
    .bind("running")
    .bind("Bigint dispatch")
    .execute(&pg_pool)
    .await
    .unwrap();

    let context = load_dispatch_source_context_pg(&pg_pool, "dispatch-api-friction-bigint")
        .await
        .unwrap()
        .expect("postgres source context");

    assert_eq!(context.card_id.as_deref(), Some("card-api-friction-bigint"));
    assert_eq!(context.repo_id.as_deref(), Some("itismyfield/AgentDesk"));
    assert_eq!(context.issue_number, Some(3_000_000_123));
    assert_eq!(context.task_summary.as_deref(), Some("API friction card"));
    assert_eq!(context.agent_id.as_deref(), Some("agent-1"));

    pg_pool.close().await;
    pg_db.drop().await;
}
