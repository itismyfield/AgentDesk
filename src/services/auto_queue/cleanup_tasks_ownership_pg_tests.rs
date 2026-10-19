use super::{ReleasedSlot, slot_taken_by_foreign_run_pg};
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::auto_queue::runtime::clear_slot_threads_for_runs_pg;

#[tokio::test]
async fn slot_cleanup_preserves_successor_session_after_ownership_probe_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    sqlx::query(
        "INSERT INTO agents (id, name, provider)
         VALUES ('cleanup-owner', 'Cleanup owner', 'claude')",
    )
    .execute(&pool)
    .await
    .expect("seed agent"); // agentdesk-audit: allow-unwrap — test fixture
    sqlx::query(
        "INSERT INTO auto_queue_runs (id, agent_id, status)
         VALUES ('cleanup-old', 'cleanup-owner', 'cancelled'),
                ('cleanup-sibling', 'cleanup-owner', 'cancelled'),
                ('cleanup-new', 'cleanup-owner', 'active')",
    )
    .execute(&pool)
    .await
    .expect("seed cleanup and successor runs"); // agentdesk-audit: allow-unwrap — test fixture
    sqlx::query(
        "INSERT INTO auto_queue_slots (agent_id, slot_index, assigned_run_id, thread_id_map)
         VALUES ('cleanup-owner', 7, 'cleanup-old', '{\"0\":\"7522201\"}'::jsonb),
                ('cleanup-owner', 8, 'cleanup-old', '{}'::jsonb)",
    )
    .execute(&pool)
    .await
    .expect("seed owned slots"); // agentdesk-audit: allow-unwrap — test fixture
    sqlx::query(
        "INSERT INTO sessions (session_key, provider, status, thread_channel_id, claude_session_id)
         VALUES ('test-host:AgentDesk-claude-cleanup-owner', 'claude', 'idle',
                 '7522201', 'old-session')",
    )
    .execute(&pool)
    .await
    .expect("seed slot session"); // agentdesk-audit: allow-unwrap — test fixture

    let run_ids = vec!["cleanup-old".to_string(), "cleanup-sibling".to_string()];
    let slot = ReleasedSlot {
        agent_id: "cleanup-owner".to_string(),
        slot_index: 7,
    };
    assert!(
        !slot_taken_by_foreign_run_pg(&pool, &run_ids, &slot)
            .await
            .expect("ownership probe passes") // agentdesk-audit: allow-unwrap — test assertion
    );
    // Reassign only after the real probe has returned, before the real clear.
    sqlx::query(
        "UPDATE auto_queue_slots SET assigned_run_id = 'cleanup-new'
         WHERE agent_id = 'cleanup-owner' AND slot_index = 7",
    )
    .execute(&pool)
    .await
    .expect("reassign after probe"); // agentdesk-audit: allow-unwrap — test fixture
    sqlx::query(
        "UPDATE sessions SET claude_session_id = 'successor-session'
         WHERE thread_channel_id = '7522201'",
    )
    .execute(&pool)
    .await
    .expect("successor establishes its session"); // agentdesk-audit: allow-unwrap — test fixture
    let cleared = clear_slot_threads_for_runs_pg(None, &pool, &slot.agent_id, 7, &run_ids)
        .await
        .expect("foreign ownership is a successful skip"); // agentdesk-audit: allow-unwrap — test assertion
    let session: Option<String> = sqlx::query_scalar(
        "SELECT claude_session_id FROM sessions WHERE thread_channel_id = '7522201'",
    )
    .fetch_one(&pool)
    .await
    .expect("read successor session"); // agentdesk-audit: allow-unwrap — test assertion
    assert_eq!(session.as_deref(), Some("successor-session"));
    assert_eq!(cleared, 0);

    for owner in [None, Some("cleanup-old"), Some("cleanup-sibling")] {
        sqlx::query(
            "UPDATE auto_queue_slots SET assigned_run_id = $1
             WHERE agent_id = 'cleanup-owner' AND slot_index = 7",
        )
        .bind(owner)
        .execute(&pool)
        .await
        .expect("restore eligible slot ownership"); // agentdesk-audit: allow-unwrap — test fixture
        assert_eq!(
            clear_slot_threads_for_runs_pg(None, &pool, &slot.agent_id, 7, &run_ids)
                .await
                .expect("clear released or task-owned slot"), // agentdesk-audit: allow-unwrap — test assertion
            1,
            "owner {owner:?} must remain eligible"
        );
    }

    pool.close().await;
    pg_db.drop().await;
}
