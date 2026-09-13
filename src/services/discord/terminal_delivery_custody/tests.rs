use super::*;
use crate::services::{discord::inflight::InflightTurnState, provider::ProviderKind};
use std::sync::{Arc, atomic::AtomicUsize};

fn payload() -> Value {
    let mut state = InflightTurnState::new(
        ProviderKind::Claude,
        5521,
        None,
        1,
        42,
        99,
        "original input".into(),
        None,
        None,
        Some("AgentDesk-custody-A".into()),
        None,
        7,
    );
    state.turn_nonce = Some("episode-A".into());
    state.turn_start_offset = Some(10);
    state.last_offset = 64;
    state.full_response = "A retained answer 한글".into();
    serde_json::json!({"inflight": state, "cancelled": false, "dispatch_id": "dispatch-A"})
}

#[tokio::test]
async fn foreign_terminal_custody_restart_retries_original_snapshot() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join(DIRECTORY);
    let original = payload();
    persist_at(&root, "episode-A", &original).await.unwrap();
    let path = record_path(&root, "episode-A");
    let before = fs::read(&path).unwrap();
    assert_eq!(
        drain_with(&root, |value| async { (value, Ok(false)) })
            .await
            .unwrap(),
        0
    );
    assert_eq!(fs::read(&path).unwrap(), before);
    // A restarted caller knows only the directory. The original body and
    // captured identity must be reconstructed from the durable payload.
    let seen = original.clone();
    assert_eq!(
        drain_with(&root, move |value| {
            assert_eq!(value, seen);
            async { (value, Ok(true)) }
        })
        .await
        .unwrap(),
        1
    );
    assert!(!path.exists());
}

#[tokio::test]
async fn foreign_terminal_custody_conflict_and_write_failure_never_acknowledge() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join(DIRECTORY);
    let original = payload();
    persist_at(&root, "episode-A", &original).await.unwrap();
    persist_at(&root, "episode-A", &original).await.unwrap();
    let mut successor = original.clone();
    successor["inflight"]["full_response"] = "successor body".into();
    assert!(persist_at(&root, "episode-A", &successor).await.is_err());
    let retained: Record =
        serde_json::from_slice(&fs::read(record_path(&root, "episode-A")).unwrap()).unwrap();
    assert_eq!(retained.payload, original);
    let blocked = temp.path().join("blocked");
    fs::write(&blocked, "not a directory").unwrap();
    assert!(persist_at(&blocked, "episode-A", &original).await.is_err());
}

#[tokio::test]
async fn foreign_terminal_custody_unknown_schema_and_changed_snapshot_survive() {
    let temp = tempfile::tempdir().unwrap();
    let path = record_path(temp.path(), "episode-A");
    fs::write(&path, r#"{"version":2,"key":"episode-A","payload":{}}"#).unwrap();
    assert!(
        drain_with(temp.path(), |_| async {
            panic!("unknown schema must not publish")
        })
        .await
        .is_err()
    );
    assert!(path.exists());
    fs::remove_file(&path).unwrap();
    persist_at(temp.path(), "episode-A", &payload())
        .await
        .unwrap();
    let changed_path = path.clone();
    assert!(
        drain_with(temp.path(), move |value| {
            fs::write(&changed_path, "changed outside the custody protocol").unwrap();
            async { (value, Ok(true)) }
        })
        .await
        .is_err()
    );
    assert_eq!(
        fs::read_to_string(path).unwrap(),
        "changed outside the custody protocol"
    );
}

#[tokio::test]
async fn foreign_terminal_custody_concurrent_drains_publish_once() {
    let temp = tempfile::tempdir().unwrap();
    persist_at(temp.path(), "episode-A", &payload())
        .await
        .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let first_calls = calls.clone();
    let second_calls = calls.clone();
    let (first, second) = tokio::join!(
        drain_with(temp.path(), move |value| {
            first_calls.fetch_add(1, Ordering::SeqCst);
            async {
                tokio::task::yield_now().await;
                (value, Ok(true))
            }
        }),
        drain_with(temp.path(), move |value| {
            second_calls.fetch_add(1, Ordering::SeqCst);
            async { (value, Ok(true)) }
        }),
    );
    assert_eq!(first.unwrap() + second.unwrap(), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

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
    let enqueue = |value: Value| {
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

#[tokio::test]
async fn foreign_terminal_custody_receipt_progress_survives_cleanup_failure() {
    let temp = tempfile::tempdir().unwrap();
    let original = payload();
    persist_at(temp.path(), "episode-A", &original)
        .await
        .unwrap();
    let path = record_path(temp.path(), "episode-A");
    // The terminal adapter received an actual Discord message ID, but the
    // first lifecycle settlement fails. Preserve that receipt before retry.
    assert!(
        drain_with(temp.path(), |mut value| async move {
            value["delivery_receipts"] = serde_json::json!([91]);
            value["children_remaining"] = serde_json::json!(["child-A"]);
            (value, Err("lifecycle settlement unavailable".into()))
        })
        .await
        .is_err()
    );
    let retained: Record = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(
        retained.payload["delivery_receipts"],
        serde_json::json!([91])
    );
    // A retry by the original bridge cannot rewind the adapter's progress.
    persist_at(temp.path(), "episode-A", &original)
        .await
        .unwrap();
    assert_eq!(
        drain_with(temp.path(), |mut value| async move {
            assert_eq!(value["delivery_receipts"], serde_json::json!([91]));
            value["children_remaining"] = serde_json::json!([]);
            (value, Ok(false))
        })
        .await
        .unwrap(),
        0
    );
    persist_at(temp.path(), "episode-A", &original)
        .await
        .unwrap();
    assert_eq!(
        drain_with(temp.path(), |value| async move {
            assert_eq!(value["delivery_receipts"], serde_json::json!([91]));
            assert_eq!(value["children_remaining"], serde_json::json!([]));
            (value, Ok(true))
        })
        .await
        .unwrap(),
        1
    );
    assert!(!path.exists());
}
