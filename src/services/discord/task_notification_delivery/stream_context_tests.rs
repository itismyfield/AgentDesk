use crate::services::agent_protocol::TaskNotificationKind;
use crate::services::discord::task_notification_delivery::{
    TaskNotificationContext,
    inline_contract_tests::{
        CHANNEL, Transport, assert_inline, clients, database, deliver, snapshot,
    },
};
use crate::services::session_backend::StreamLineState;

#[tokio::test]
async fn monitor_auto_turn_stream_context_uses_provider_inline_once_pg() {
    let (database, pool) = database("MonitorAutoTurn actual stream context entrypoint").await;
    let context = |summary| {
        TaskNotificationContext::from_stream_json(
            &serde_json::json!({
                "type": "system",
                "subtype": "task_notification",
                "task_notification_kind": "monitor_auto_turn",
                "task_id": "monitor-auto-turn",
                "summary": summary,
            }),
            &StreamLineState::new(),
        )
        .expect("the actual stream entrypoint must recognize a status-free MonitorAutoTurn")
    };
    let prior_context = context("before stream promotion");
    let context = context("heartbeat event");
    assert_eq!(
        context.routing_kind(),
        TaskNotificationKind::MonitorAutoTurn
    );
    let prior_event = prior_context.to_event(CHANNEL, "claude", "inline-contract-session");
    let event = context.to_event(CHANNEL, "claude", "inline-contract-session");
    let transport = Transport::default();
    let first = deliver(&pool, &transport, &prior_event).await;
    let before = snapshot(&pool, &event).await;
    assert_eq!(before.count, 1);
    assert_inline(
        &transport.latest(),
        "claude",
        "📡",
        "before stream promotion",
    );

    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let promoted = crate::services::discord::session_relay_sink::task_notification_context::confirm_task_context_card(
        Some(&shared),
        &clients("claude"),
        &transport,
        &shared.ui.placeholder_live_events,
        CHANNEL,
        "claude",
        "inline-contract-session",
        Some(&context),
    )
    .await
    .expect("the actual stream gate must confirm the current Monitor card")
    .expect("the Monitor context must resolve a card");
    assert_eq!(promoted.message_id, first.message_id);
    assert_eq!(promoted.bot_key, "provider:claude");
    assert_eq!(
        transport.counts(),
        (1, 0),
        "stream Promotion must not count or publish an unobserved Monitor event"
    );
    assert_eq!(
        snapshot(&pool, &event).await,
        before,
        "stream Promotion must preserve the last observed Monitor payload and count"
    );

    let observed = deliver(&pool, &transport, &event).await;
    assert_eq!(observed.message_id, first.message_id);
    assert_eq!(transport.counts(), (1, 1));
    assert_inline(&transport.latest(), "claude", "📡", "heartbeat event");
    let after = snapshot(&pool, &event).await;
    assert_eq!(
        after.count, 2,
        "only the subsequent TUI Observation increments the Monitor count"
    );
    assert!(after.content.ends_with(" · 2회"));
    let replay = deliver(&pool, &transport, &event).await;
    assert_eq!(replay.message_id, first.message_id);
    assert_eq!(transport.counts(), (1, 1));
    assert_eq!(snapshot(&pool, &event).await, after);
    drop(shared);
    database.drop().await;
}
