use super::*;
use serde_json::json;

#[tokio::test(start_paused = true)]
async fn absorbed_remove_without_command_posts_once_across_restart() {
    let _turns = TURNS.lock().await;
    let runtime = crate::config::TestRuntimeRootGuard::new();
    let path = runtime.path().join("native.jsonl");
    std::fs::write(&path, b"").unwrap();
    let (store, _binding) = begin(runtime.path(), &path, "remove-only-notice-pane");
    let port = Arc::new(Port::default());
    let _confirmed = TestConfirmation::new(CHANNEL);
    let mut actor = host(&store, port.clone(), Alarms::default(), u64::MAX);
    start_migration(&mut actor);
    resume(&mut actor);
    assert!(crate::services::tui_o::writer::input_notices::writer_posts(
        CHANNEL,
        ShadowProvider::Claude
    ));

    let prompt = "계속 진행해 줘";
    append_value(
        &path,
        json!({"type":"queue-operation", "operation":"enqueue", "content":prompt}),
    );
    capture_and_deliver(&mut actor).await;
    assert!(port.posts.lock().unwrap().is_empty());

    for (reason, id, content) in [
        ("withdrawn", "withdrawn", prompt),
        ("delivered_to_agent", "redirected", prompt),
        (
            "absorbed_mid_turn",
            "machine",
            "<task-notification>done</task-notification>",
        ),
        ("absorbed_mid_turn", "event", "<event>session event</event>"),
        (
            "absorbed_mid_turn",
            "peer",
            "<cross-session-message>peer message</cross-session-message>",
        ),
        ("absorbed_mid_turn", "pending-input", prompt),
    ] {
        append_value(
            &path,
            json!({"type":"queue-operation", "operation":"remove", "reason":reason,
                "commandUuid":id, "deliveryId":format!("delivery-{id}"), "content":content}),
        );
    }
    capture_and_deliver(&mut actor).await;
    assert_eq!(*port.posts.lock().unwrap(), [notice(prompt)]);
    assert_eq!(notice_evidence(&mut actor, &input_key("pending-input")), 1);
    drop(actor);

    let mut restarted = host(&store, port.clone(), Alarms::default(), u64::MAX);
    resume(&mut restarted);
    capture_and_deliver(&mut restarted).await;
    assert_eq!(*port.posts.lock().unwrap(), [notice(prompt)]);

    append_value(
        &path,
        queued_command("command-row", "pending-input", prompt, "prompt", "human"),
    );
    append(&path, &row("pending-input", prompt));
    capture_and_deliver(&mut restarted).await;
    assert_eq!(*port.posts.lock().unwrap(), [notice(prompt)]);
    let ledger = restarted.writer_mut_for_test().store().ledger();
    let (_, saved) = ledger
        .latest_piece(&input_key("pending-input"), 0)
        .expect("notice was prepared");
    assert!(matches!(
        saved.outcome,
        Some(crate::services::tui_o::store::ledger::PieceOutcome::Posted(
            _
        ))
    ));
    assert!(ledger.violation().is_none());
}
