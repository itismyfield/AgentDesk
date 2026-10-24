use super::*;

fn stream_event(task_id: &str, summary: &str) -> TaskCardEvent {
    let context = TaskNotificationContext::from_stream_json(
        &serde_json::json!({
            "type": "system",
            "subtype": "task_notification",
            "task_notification_kind": "background",
            "task_id": task_id,
            "status": "completed",
            "summary": summary,
        }),
        &StreamLineState::new(),
    )
    .expect("the real stream parser must recognize a body-free completion context");
    assert!(context.footer_only_marker_event_key().is_some());
    let event = context.to_event(CHANNEL, "claude", "inline-contract-session");
    assert!(event.supports_footer_deferral());
    let TaskCardPayload::Task(note) = &event.payload else {
        panic!("stream completion context must produce a task payload");
    };
    assert!(note.result.is_none());
    event
}

async fn promote(pool: &PgPool, transport: &Transport, event: &TaskCardEvent) -> CardEnsureOutcome {
    ensure_card(
        Some(pool),
        &clients("claude"),
        transport,
        event,
        EnsureIntent::Promotion,
    )
    .await
    .expect("the actual promotion entrypoint must recover its recorded completion body")
}

#[tokio::test]
async fn footer_only_background_stream_promotion_keeps_full_notify_body_pg() {
    let (database, pool) =
        database("footer-only Background result survives stream promotion").await;
    let summary = "Background command \"footer worker\" completed (exit code 0)";
    let full = event(
        "footer-background",
        None,
        Some("completed"),
        summary,
        "full background first\nfull background second\nfull background third",
    );
    assert!(full.supports_footer_deferral());
    record_footer_only(Some(&pool), &full)
        .await
        .expect("record the full real Background payload before stream context loses its result");
    let promoted = stream_event("footer-background", summary);
    assert_eq!(promoted.scope.event_key, full.scope.event_key);
    assert_eq!(promoted.scope.session_key, full.scope.session_key);
    let transport = Transport::default();
    let first = promote(&pool, &transport, &promoted).await;
    assert_eq!(first.bot_key, "notify");
    assert_eq!(transport.counts(), (1, 0));
    let call = transport.latest();
    assert_notify_body_card(
        &call,
        &[
            "full background first",
            "full background second",
            "full background third",
        ],
    );
    assert!(call.content.contains("> full background second"));
    let before = snapshot(&pool, &promoted).await;
    assert_eq!(before.content, call.content);
    let replay = promote(&pool, &transport, &promoted).await;
    assert_eq!(replay.message_id, first.message_id);
    assert_eq!(replay.bot_key, "notify");
    assert_eq!(transport.counts(), (1, 0));
    assert_eq!(snapshot(&pool, &promoted).await, before);
    database.drop().await;
}

#[tokio::test]
async fn legacy_footer_only_stream_promotion_normalizes_header_and_preserves_body_pg() {
    let (database, pool) = database("legacy footer-only card body survives stream promotion").await;
    let event = stream_event("abc", "Agent finished");
    record_footer_only(Some(&pool), &event)
        .await
        .expect("create the real FooterOnly authority before replacing its historical bytes");
    let legacy =
        "✅ Task completed\n**Agent finished**\n\n> legacy result\n\n-# task abc · 51 tools";
    let changed = sqlx::query(
        "UPDATE task_notification_card_state SET rendered_content = $5, content_hash = $6
         WHERE channel_id = $1 AND provider = $2 AND session_key = $3 AND event_key = $4
           AND surface_owner = 'footer_only' AND delivery_state = 'footer_only'",
    )
    .bind(i64::try_from(event.scope.channel_id).unwrap())
    .bind(&event.scope.provider)
    .bind(&event.scope.session_key)
    .bind(&event.scope.event_key)
    .bind(legacy)
    .bind(content_hash(legacy))
    .execute(&pool)
    .await
    .expect("seed only the lane-owned legacy FooterOnly renderer snapshot")
    .rows_affected();
    assert_eq!(changed, 1);
    let transport = Transport::default();
    let first = promote(&pool, &transport, &event).await;
    assert_eq!(first.bot_key, "notify");
    assert_eq!(transport.counts(), (1, 0));
    let call = transport.latest();
    assert_notify_body_card(&call, &["Agent finished", "> legacy result"]);
    assert!(!call.content.contains("Task completed"), "{call:?}");
    assert!(!call.content.contains("task abc"), "{call:?}");
    assert!(!call.content.contains("-# task "), "{call:?}");
    let before = snapshot(&pool, &event).await;
    assert_eq!(before.content, call.content);
    let replay = promote(&pool, &transport, &event).await;
    assert_eq!(replay.message_id, first.message_id);
    assert_eq!(replay.bot_key, "notify");
    assert_eq!(transport.counts(), (1, 0));
    assert_eq!(snapshot(&pool, &event).await, before);
    database.drop().await;
}
