use super::*;
use crate::services::{discord::inflight::InflightTurnState, provider::ProviderKind};
use std::sync::{Arc, atomic::AtomicUsize};

pub(super) fn payload() -> Value {
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
        drain_with(&root, |value, _| async { (value, Ok(false)) })
            .await
            .unwrap(),
        0
    );
    assert_eq!(fs::read(&path).unwrap(), before);
    // A restarted caller knows only the directory. The original body and
    // captured identity must be reconstructed from the durable payload.
    let seen = original.clone();
    assert_eq!(
        drain_with(&root, move |value, _| {
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
        drain_with(temp.path(), |_, _| async {
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
        drain_with(temp.path(), move |value, _| {
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
        drain_with(temp.path(), move |value, _| {
            first_calls.fetch_add(1, Ordering::SeqCst);
            async {
                tokio::task::yield_now().await;
                (value, Ok(true))
            }
        }),
        drain_with(temp.path(), move |value, _| {
            second_calls.fetch_add(1, Ordering::SeqCst);
            async { (value, Ok(true)) }
        }),
    );
    assert_eq!(first.unwrap() + second.unwrap(), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
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
        drain_with(temp.path(), |mut value, _| async move {
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
        drain_with(temp.path(), |mut value, _| async move {
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
        drain_with(temp.path(), |value, _| async move {
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

#[tokio::test]
async fn foreign_terminal_custody_checkpoint_survives_interrupted_callback() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().to_path_buf();
    persist_at(&root, "episode-A", &payload()).await.unwrap();
    let attempt_root = root.clone();
    let attempt = tokio::spawn(async move {
        drain_with(&attempt_root, |mut value, checkpoint| async move {
            value["delivery_receipts"] = serde_json::json!([91, 92]);
            checkpoint.persist(&value).unwrap();
            // Simulate interruption before the adapter can return progress.
            panic!("next chunk aborted after earlier ACKs");
        })
        .await
    });
    assert!(attempt.await.unwrap_err().is_panic());
    let retained: Record =
        serde_json::from_slice(&fs::read(record_path(&root, "episode-A")).unwrap()).unwrap();
    assert_eq!(
        retained.payload["delivery_receipts"],
        serde_json::json!([91, 92])
    );
    assert_eq!(
        drain_with(&root, |value, checkpoint| async move {
            assert_eq!(value["delivery_receipts"], serde_json::json!([91, 92]));
            checkpoint.persist(&value).unwrap();
            (value, Ok(true))
        })
        .await
        .unwrap(),
        1
    );
}

#[tokio::test]
async fn foreign_terminal_custody_checkpoint_failure_does_not_settle_or_overwrite() {
    let temp = tempfile::tempdir().unwrap();
    persist_at(temp.path(), "episode-A", &payload())
        .await
        .unwrap();
    let path = record_path(temp.path(), "episode-A");
    let before = fs::read_to_string(&path).unwrap();
    let changed_path = path.clone();
    assert!(
        drain_with(temp.path(), move |mut value, checkpoint| {
            // A file changed outside the lock protocol cannot be overwritten or
            // falsely acknowledged by either an intermediate checkpoint or finish.
            fs::write(&changed_path, "foreign retained bytes").unwrap();
            async move {
                value["delivery_receipts"] = serde_json::json!([91]);
                let result = checkpoint.persist(&value);
                assert!(result.is_err());
                (value, result.map(|()| true))
            }
        })
        .await
        .is_err()
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), "foreign retained bytes");
    // Restore the same pending record and verify a closed handle cannot write
    // after a completed callback relinquished the existing flock.
    fs::write(&path, before).unwrap();
    let handle = Arc::new(Mutex::new(None));
    let escaped = handle.clone();
    assert_eq!(
        drain_with(temp.path(), move |value, checkpoint| {
            *escaped.lock().unwrap() = Some(checkpoint);
            async { (value, Ok(false)) }
        })
        .await
        .unwrap(),
        0
    );
    assert!(
        handle
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .persist(&payload())
            .is_err()
    );
    assert!(path.exists());
}

#[cfg(unix)]
#[tokio::test]
async fn foreign_terminal_custody_checkpoint_write_failure_is_explicit() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    persist_at(temp.path(), "episode-A", &payload())
        .await
        .unwrap();
    let path = record_path(temp.path(), "episode-A");
    let before = fs::read(&path).unwrap();
    let root = temp.path().to_path_buf();
    let checked_path = path.clone();
    assert!(
        drain_with(temp.path(), move |mut value, checkpoint| {
            let root = root.clone();
            let checked_path = checked_path.clone();
            let before = before.clone();
            async move {
                value["delivery_receipts"] = serde_json::json!([91]);
                let permissions = fs::metadata(&root).unwrap().permissions();
                fs::set_permissions(&root, fs::Permissions::from_mode(0o500)).unwrap();
                let result = checkpoint.persist(&value);
                fs::set_permissions(&root, permissions).unwrap();
                assert!(result.is_err(), "failed checkpoint must stop publication");
                assert_eq!(fs::read(&checked_path).unwrap(), before);
                (value, result.map(|()| true))
            }
        })
        .await
        .is_err()
    );
    // The final attempt can retain the in-memory ACK after writes recover,
    // but it must still report the adapter failure and keep custody pending.
    let retained: Record = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(
        retained.payload["delivery_receipts"],
        serde_json::json!([91])
    );
}

#[tokio::test]
async fn foreign_terminal_custody_valid_json_corruption_is_not_acknowledged() {
    for corrupt_receipt in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let original = payload();
        persist_at(temp.path(), "episode-A", &original)
            .await
            .unwrap();
        let path = record_path(temp.path(), "episode-A");
        let mut record: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        if corrupt_receipt {
            record["payload"]["delivery_receipts"] = serde_json::json!([991]);
        } else {
            record["payload"]["inflight"]["full_response"] = "corrupted response".into();
        }
        // Keep both digest fields and valid JSON intact while changing the
        // body or receipt. Neither reseeding nor drain may acknowledge it.
        let corrupted = serde_json::to_string(&record).unwrap();
        fs::write(&path, &corrupted).unwrap();
        assert!(
            persist_at(temp.path(), "episode-A", &original)
                .await
                .is_err()
        );
        assert!(
            drain_with(temp.path(), |_, _| async {
                panic!("corrupt payload must never reach the terminal adapter")
            })
            .await
            .is_err()
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), corrupted);
    }
}

#[tokio::test]
async fn foreign_terminal_custody_aborted_attempt_deactivates_escaped_handle() {
    use std::time::Duration;
    for panic_callback in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        persist_at(&root, "episode-A", &payload()).await.unwrap();
        let (entered, ready) = tokio::sync::oneshot::channel();
        let mut entered = Some(entered);
        let attempt_root = root.clone();
        let attempt = tokio::spawn(async move {
            drain_with(&attempt_root, move |mut value, checkpoint| {
                let entered = entered.take().unwrap();
                async move {
                    value["delivery_receipts"] = serde_json::json!([91]);
                    checkpoint.persist(&value).unwrap();
                    assert!(entered.send(checkpoint.clone()).is_ok());
                    if panic_callback {
                        panic!("callback panic after checkpoint escaped");
                    }
                    std::future::pending::<()>().await;
                    unreachable!()
                }
            })
            .await
        });
        let escaped = tokio::time::timeout(Duration::from_secs(2), ready)
            .await
            .unwrap()
            .unwrap();
        if !panic_callback {
            attempt.abort();
        }
        let error = attempt.await.unwrap_err();
        assert_eq!(error.is_panic(), panic_callback);
        assert_eq!(error.is_cancelled(), !panic_callback);
        assert!(escaped.persist(&payload()).is_err());
        // Keep the escaped Arc alive while a fresh drain acquires the same
        // actual file lock, proving that cancellation did not strand it.
        let retried = tokio::time::timeout(
            Duration::from_secs(2),
            drain_with(&root, |value, _| async {
                assert_eq!(value["delivery_receipts"], serde_json::json!([91]));
                (value, Ok(true))
            }),
        )
        .await
        .expect("aborted attempt retained the file lock")
        .unwrap();
        assert_eq!(retried, 1);
        assert!(escaped.persist(&payload()).is_err());
    }
}
