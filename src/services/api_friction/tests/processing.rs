use super::super::*;
use super::helpers::{
    TestPostgresDb, install_mock_gh_issue_create, install_mock_gh_issue_create_failure,
};
use sqlx::PgPool;

async fn insert_pattern_event(
    pg_pool: &PgPool,
    id: &str,
    fingerprint: &str,
    summary: &str,
    minutes_ago: i32,
) {
    sqlx::query(
        "INSERT INTO api_friction_events (
            id, fingerprint, endpoint, friction_type, summary, keywords_json, payload_json,
            channel_id, provider, repo_id, memory_backend, memory_status, created_at
         ) VALUES (
            $1, $2, $3, $4, $5, '[]'::jsonb, '{}'::jsonb,
            $6, $7, $8, $9, $10, NOW() - ($11::INT * INTERVAL '1 minute')
         )",
    )
    .bind(id)
    .bind(fingerprint)
    .bind("/api/docs/kanban")
    .bind("docs-bypass")
    .bind(summary)
    .bind("1")
    .bind("codex")
    .bind("itismyfield/AgentDesk")
    .bind("memento")
    .bind("stored")
    .bind(minutes_ago)
    .execute(pg_pool)
    .await
    .unwrap();
}

async fn insert_repeated_pattern(pg_pool: &PgPool, fingerprint: &str) {
    insert_pattern_event(pg_pool, "event-1", fingerprint, "first", 2).await;
    insert_pattern_event(pg_pool, "event-2", fingerprint, "second", 1).await;
}

#[tokio::test]
#[cfg_attr(
    windows,
    ignore = "batch file argument sanitization rejects gh mock args on Windows"
)]
async fn process_api_friction_patterns_creates_issue_once() {
    let _env_lock = crate::services::discord::runtime_store::lock_test_env();
    let _mock_gh =
        install_mock_gh_issue_create("https://github.com/itismyfield/AgentDesk/issues/999");

    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;
    insert_repeated_pattern(&pg_pool, "api-docs-kanban::docs-bypass").await;

    let summary = process_api_friction_patterns(Some(&pg_pool), None, None)
        .await
        .unwrap();

    assert_eq!(summary.created_issues.len(), 1, "{summary:?}");
    assert!(summary.failed_patterns.is_empty(), "{summary:?}");
    let issue_number: i64 = sqlx::query_scalar(
        "SELECT issue_number::BIGINT
         FROM api_friction_issues
         WHERE fingerprint = 'api-docs-kanban::docs-bypass'",
    )
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    assert_eq!(issue_number, 999);
    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
#[cfg_attr(
    windows,
    ignore = "batch file argument sanitization rejects gh mock args on Windows"
)]
async fn process_api_friction_patterns_skips_existing_issue_urls() {
    let _env_lock = crate::services::discord::runtime_store::lock_test_env();
    let _mock_gh = install_mock_gh_issue_create_failure("gh should not be called for skips");

    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;
    insert_repeated_pattern(&pg_pool, "api-docs-kanban::docs-bypass").await;
    sqlx::query(
        "INSERT INTO api_friction_issues (
            fingerprint, repo_id, endpoint, friction_type, title, body, issue_number,
            issue_url, event_count, first_event_at, last_event_at, created_at, updated_at
         ) VALUES (
            $1, $2, $3, $4, $5, $6, $7,
            $8, $9, NOW() - INTERVAL '2 minutes', NOW() - INTERVAL '1 minute', NOW(), NOW()
         )",
    )
    .bind("api-docs-kanban::docs-bypass")
    .bind("itismyfield/AgentDesk")
    .bind("/api/docs/kanban")
    .bind("docs-bypass")
    .bind("existing title")
    .bind("existing body")
    .bind(123_i32)
    .bind("https://github.com/itismyfield/AgentDesk/issues/123")
    .bind(2_i32)
    .execute(&pg_pool)
    .await
    .unwrap();

    let summary = process_api_friction_patterns(Some(&pg_pool), None, None)
        .await
        .unwrap();

    assert_eq!(summary.processed_patterns, 1, "{summary:?}");
    assert!(summary.created_issues.is_empty(), "{summary:?}");
    assert!(summary.failed_patterns.is_empty(), "{summary:?}");
    assert_eq!(summary.skipped_patterns.len(), 1, "{summary:?}");
    assert_eq!(
        summary.skipped_patterns[0].issue_url.as_deref(),
        Some("https://github.com/itismyfield/AgentDesk/issues/123")
    );
    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
#[cfg_attr(
    windows,
    ignore = "batch file argument sanitization rejects gh mock args on Windows"
)]
async fn process_api_friction_patterns_persists_github_failures() {
    let _env_lock = crate::services::discord::runtime_store::lock_test_env();
    let _mock_gh = install_mock_gh_issue_create_failure("mock gh failure");

    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;
    insert_repeated_pattern(&pg_pool, "api-docs-kanban::docs-bypass").await;

    let summary = process_api_friction_patterns(Some(&pg_pool), None, None)
        .await
        .unwrap();

    assert_eq!(summary.processed_patterns, 1, "{summary:?}");
    assert!(summary.created_issues.is_empty(), "{summary:?}");
    assert_eq!(summary.failed_patterns.len(), 1, "{summary:?}");
    assert!(summary.failed_patterns[0].error.contains("mock gh failure"));

    let (issue_number, issue_url, last_error): (Option<i64>, Option<String>, Option<String>) =
        sqlx::query_as(
            "SELECT issue_number::BIGINT, issue_url, last_error
             FROM api_friction_issues
             WHERE fingerprint = 'api-docs-kanban::docs-bypass'",
        )
        .fetch_one(&pg_pool)
        .await
        .unwrap();

    assert_eq!(issue_number, None);
    assert_eq!(issue_url, None);
    assert!(
        last_error
            .as_deref()
            .is_some_and(|error| error.contains("mock gh failure")),
        "{last_error:?}"
    );
    pg_pool.close().await;
    pg_db.drop().await;
}
