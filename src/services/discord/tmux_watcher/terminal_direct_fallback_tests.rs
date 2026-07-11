use super::*;

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

#[test]
fn watcher_task_response_wiring_prepares_reference_before_send_and_marks_after_frontier() {
    let helper = include_str!("task_response_authority.rs");
    let prepare = helper
        .find("prepare_watcher_task_response")
        .expect("watcher task fallback must prepare a durable card/reference");
    let send = helper[prepare..]
        .find("send_long_message_raw_with_required_reference_rollback")
        .map(|offset| prepare + offset)
        .expect("watcher task fallback must use a referenced response send");
    assert!(prepare < send, "card/bind preparation must precede response send");

    let fallback = include_str!("terminal_direct_fallback.rs");
    assert!(
        fallback.contains("task_response_authority::apply_watcher_task_response("),
        "the production fallback must delegate task responses to the durable authority"
    );

    let parent = include_str!("../tmux_watcher.rs");
    let apply = parent
        .find("apply_watcher_direct_fallback_send(")
        .expect("production watcher must invoke the task-aware fallback helper");
    let frontier = parent[apply..]
        .find("advance_watcher_confirmed_end(")
        .map(|offset| apply + offset)
        .expect("watcher delivery must commit its frontier");
    let mark = parent[apply..]
        .find("commit_watcher_task_response_fence(")
        .map(|offset| apply + offset)
        .expect("watcher must mark the exact response cycle after commit");
    assert!(frontier < mark, "frontier commit must precede response-fence mark");
    let commit = helper
        .find("commit_watcher_task_response_fence(")
        .expect("watcher response commit helper");
    assert!(
        helper[commit..].contains("mark_task_response_delivered("),
        "the production commit helper must token-CAS the exact response claim"
    );
}
