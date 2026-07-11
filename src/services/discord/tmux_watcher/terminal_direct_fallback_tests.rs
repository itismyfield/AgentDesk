use super::*;

#[tokio::test]
async fn pending_pg_task_card_survives_process_local_fence_loss() {
    let Some(pg_db) = crate::dispatch::test_support::DispatchPostgresTestDb::try_create(
        "agentdesk_task_fallback_fence_4055",
        "watcher task fallback durable fence",
    )
    .await
    else {
        return;
    };
    let pool = pg_db.connect_and_migrate().await;
    let provider = ProviderKind::Claude;
    let channel_id = ChannelId::new(4_055_902);
    let session_name = "AgentDesk-claude-4055-durable-fallback-gate";
    let event =
        crate::services::discord::task_notification_delivery::TaskCardEvent::from_task_prompt(
            channel_id.get(),
            provider.as_str(),
            session_name,
            "<task-notification><task-id>durable-fence</task-id><status>completed</status><summary>done</summary></task-notification>",
        );
    crate::services::discord::task_notification_delivery::record_footer_only(Some(&pool), &event)
        .await
        .expect("seed pending durable task-card state");

    assert!(
        task_response_fallback_must_wait_for_sink(
            true,
            Some(TaskNotificationKind::Background),
            Some(event.event_key()),
            None,
            Some(&pool),
            &provider,
            session_name,
            channel_id,
        )
        .await,
        "PostgreSQL pending state must remain authoritative after process-local state is lost",
    );
}

#[tokio::test]
async fn task_notification_response_fallback_fails_closed_without_durable_witness() {
    let provider = ProviderKind::Claude;
    let channel_id = ChannelId::new(4_055_901);
    let session_name = "AgentDesk-claude-4055-fallback-gate";

    assert!(
        task_response_fallback_must_wait_for_sink(
            true,
            Some(TaskNotificationKind::Background),
            None,
            None,
            None,
            &provider,
            session_name,
            channel_id,
        )
        .await
    );
    assert!(
        !task_response_fallback_must_wait_for_sink(
            false,
            Some(TaskNotificationKind::Background),
            None,
            None,
            None,
            &provider,
            session_name,
            channel_id,
        )
        .await
    );
    assert!(
        !task_response_fallback_must_wait_for_sink(
            true,
            None,
            None,
            None,
            None,
            &provider,
            session_name,
            channel_id,
        )
        .await
    );
}
