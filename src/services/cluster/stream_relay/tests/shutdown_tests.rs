use super::*;

#[tokio::test]
async fn shutdown_preserves_join_failures_and_no_task() {
    for panic in [false, true] {
        let mut relay = spawn_stream_relay(
            matched_for("join-result"),
            Arc::new(CapturingSink::default()),
        );
        let original = relay.task.take().unwrap();
        original.abort();
        assert!(original.await.unwrap_err().is_cancelled());
        let task = tokio::spawn(async move {
            if panic {
                panic!("shutdown join probe");
            }
            std::future::pending::<()>().await;
        });
        if !panic {
            task.abort();
        }
        relay.task = Some(task);
        let error = relay.shutdown_with_result().await.unwrap_err();
        assert_eq!(error.is_panic(), panic);
        assert_eq!(error.is_cancelled(), !panic);
    }
    let mut relay = spawn_stream_relay(matched_for("no-task"), Arc::new(CapturingSink::default()));
    let task = relay.task.take().unwrap();
    assert_eq!(
        relay.shutdown_with_result().await.unwrap(),
        ShutdownOutcome::NoTask
    );
    task.await.unwrap();
}

#[tokio::test]
async fn shutdown_waits_for_sink_then_joins() {
    let sink = Arc::new(BlockingSequenceSink {
        first_started: tokio::sync::Notify::new(),
        unblock: tokio::sync::Notify::new(),
        block_first: AtomicBool::new(true),
    });
    let relay = spawn_stream_relay(matched_for("shutdown-barrier"), sink.clone());
    assert!(relay.try_send_frame("first".into()));
    sink.first_started.notified().await;
    let shutdown = relay.shutdown_with_result();
    tokio::pin!(shutdown);
    assert!(futures::poll!(&mut shutdown).is_pending());
    sink.unblock.notify_one();
    assert_eq!(shutdown.await.unwrap(), ShutdownOutcome::Joined);
}
