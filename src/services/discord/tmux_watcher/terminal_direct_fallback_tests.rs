use super::*;

#[test]
fn committed_edit_failure_cleanup_has_controller_legacy_parity_4508() {
    let controller = include_str!("terminal_send.rs");
    let legacy = include_str!("terminal_direct_fallback.rs");
    for (label, source) in [("controller", controller), ("legacy", legacy)] {
        assert!(
            source.contains("reconcile_already_committed_after_edit_failure("),
            "{label}: committed branch must use shared guarded reconciliation"
        );
    }
    let cleanup = include_str!("committed_placeholder_cleanup.rs");
    assert!(cleanup.contains("delete_terminal_placeholder_unless_delivered("));
    assert!(cleanup.contains("true,"));
    assert!(cleanup.contains("reconcile_committed_placeholder_cleanup("));
}

#[test]
fn committed_cleanup_preserves_tracking_until_delete_commits_4508() {
    use std::cell::Cell;

    use crate::services::discord::placeholder_cleanup::PlaceholderCleanupOutcome;
    use super::terminal_send::committed_placeholder_cleanup::reconcile_committed_placeholder_cleanup;

    fn run(
        outcome: Option<&PlaceholderCleanupOutcome>,
    ) -> (bool, Option<MessageId>, bool, String, bool) {
        let mut placeholder_msg_id = Some(MessageId::new(45_080_900));
        let mut restored = true;
        let mut last_edit_text = "streamed body".to_string();
        let orphan_dropped = Cell::new(false);
        let committed = reconcile_committed_placeholder_cleanup(
            outcome,
            &mut placeholder_msg_id,
            &mut restored,
            &mut last_edit_text,
            || orphan_dropped.set(true),
        );
        (
            committed,
            placeholder_msg_id,
            restored,
            last_edit_text,
            orphan_dropped.get(),
        )
    }

    let preserved = run(Some(&PlaceholderCleanupOutcome::failed(
        "transient delete failure",
    )));
    assert_eq!(
        preserved,
        (
            false,
            Some(MessageId::new(45_080_900)),
            true,
            "streamed body".to_string(),
            false,
        ),
        "uncommitted cleanup must retain local and durable orphan tracking"
    );

    for committed_outcome in [
        PlaceholderCleanupOutcome::Succeeded,
        PlaceholderCleanupOutcome::AlreadyGone,
    ] {
        assert_eq!(
            run(Some(&committed_outcome)),
            (true, None, false, String::new(), true),
            "committed delete is the sole authority to clear lifecycle tracking"
        );
    }
    assert!(!run(None).0);
}

#[test]
fn recovered_task_response_identity_is_stable_without_inflight_or_context() {
    let provider = ProviderKind::Claude;
    let channel_id = ChannelId::new(4_055_902);
    let session_name = "AgentDesk-claude-4055-durable-fallback-gate";
    let turn_key = crate::services::discord::task_notification_delivery::fallback_response_turn_key(
        channel_id.get(),
        provider.as_str(),
        session_name,
        20,
        "done",
    );
    let same = crate::services::discord::task_notification_delivery::fallback_response_turn_key(
        channel_id.get(),
        provider.as_str(),
        session_name,
        20,
        "done",
    );
    let next = crate::services::discord::task_notification_delivery::fallback_response_turn_key(
        channel_id.get(),
        provider.as_str(),
        session_name,
        30,
        "done",
    );
    assert_eq!(turn_key, same);
    assert_ne!(turn_key, next);
    let event = crate::services::discord::task_notification_delivery::TaskCardEvent::from_recovered_terminal(
        channel_id.get(),
        provider.as_str(),
        session_name,
        TaskNotificationKind::Background,
        &turn_key,
    );
    assert_eq!(event.event_key(), format!("turn:{turn_key}"));
}
