use super::super::*;
use super::helpers::{TestPostgresDb, install_mock_gh_issue_create};

#[tokio::test]
#[cfg_attr(
    windows,
    ignore = "batch file argument sanitization rejects gh mock args on Windows"
)]
async fn process_api_friction_patterns_creates_issue_once() {
    let lock = crate::services::discord::runtime_store::lock_test_env();
    let _mock_gh =
        install_mock_gh_issue_create("https://github.com/itismyfield/AgentDesk/issues/999");

    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;
    sqlx::query(
        "INSERT INTO api_friction_events (
            id, fingerprint, endpoint, friction_type, summary, keywords_json, payload_json,
            channel_id, provider, repo_id, memory_backend, memory_status, created_at
         ) VALUES (
            $1, $2, $3, $4, $5, '[]'::jsonb, '{}'::jsonb,
            $6, $7, $8, $9, $10, NOW() - INTERVAL '2 minutes'
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
            $1, $2, $3, $4, $5, '[]'::jsonb, '{}'::jsonb,
            $6, $7, $8, $9, $10, NOW() - INTERVAL '1 minute'
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

    let summary = process_api_friction_patterns(Some(&pg_pool), None, None)
        .await
        .unwrap();
    drop(lock);

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
