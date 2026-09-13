use super::{tests::payload, *};
use crate::services::discord::inflight::InflightTurnState;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn foreign_terminal_custody_pg_failure_restart_and_exact_dedupe() {
    use crate::services::message_outbox as outbox;
    let database = crate::dispatch::test_support::DispatchPostgresTestDb::create(
        "agentdesk_terminal_custody",
        "foreign terminal custody retry",
    )
    .await;
    let pool = database.connect_and_migrate().await;
    sqlx::query("INSERT INTO sessions (session_key, provider, status, thread_channel_id, active_turn_delivery_outbox_id) VALUES ('foreign-successor-B', 'claude', 'turn_active', '5521', 991)")
        .execute(&pool).await.unwrap();
    let temp = tempfile::tempdir().unwrap();
    let original = payload();
    persist_at(temp.path(), "episode-A", &original)
        .await
        .unwrap();
    // Exercise a real enqueue failure, not a synthetic successful callback.
    sqlx::query("ALTER TABLE message_outbox RENAME TO custody_unavailable_outbox")
        .execute(&pool)
        .await
        .unwrap();
    let enqueue = |value: Value, _: Arc<CustodyCheckpoint>| {
        let pool = pool.clone();
        async move {
            let state: InflightTurnState =
                serde_json::from_value(value["inflight"].clone()).unwrap();
            let result = outbox::enqueue_outbox_pg_returning_outcome_with_exact_dedupe_and_cancel(
                &pool,
                outbox::OutboxMessage {
                    target: "channel:5521",
                    content: &state.full_response,
                    bot: "claude",
                    source: "headless_turn",
                    reason_code: Some("custody-A"),
                    session_key: None,
                },
                "foreign-custody-test:episode-A",
                None,
            )
            .await
            .map(|result| matches!(result, outbox::OutboxEnqueueOutcome::Enqueued { .. }))
            .map_err(|error| error.to_string());
            (value, result)
        }
    };
    assert!(drain_with(temp.path(), enqueue).await.is_err());
    assert!(record_path(temp.path(), "episode-A").exists());
    sqlx::query("ALTER TABLE custody_unavailable_outbox RENAME TO message_outbox")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(drain_with(temp.path(), enqueue).await.unwrap(), 1);
    persist_at(temp.path(), "episode-A", &original)
        .await
        .unwrap();
    assert_eq!(drain_with(temp.path(), enqueue).await.unwrap(), 1);
    let rows: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT content, session_key FROM message_outbox WHERE dedupe_key='foreign-custody-test:episode-A'")
        .fetch_all(&pool).await.unwrap();
    assert_eq!(rows, vec![("A retained answer 한글".into(), None)]);
    let successor: (String, Option<i64>) = sqlx::query_as("SELECT status, active_turn_delivery_outbox_id FROM sessions WHERE session_key='foreign-successor-B'")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(successor, ("turn_active".into(), Some(991)));
}
