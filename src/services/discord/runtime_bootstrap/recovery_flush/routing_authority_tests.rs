use super::*;

#[test]
fn genuine_unowned_cleanup_removes_exact_token_artifacts() {
    let root = tempfile::tempdir().expect("temp AgentDesk root");
    let _env = crate::config::set_agentdesk_root_for_test(root.path());
    let provider = ProviderKind::Codex;
    let token_hash = "discord_r11_test";
    let channel_id = serenity::ChannelId::new(15_046_124_559_162_459);
    let dir = root
        .path()
        .join("runtime")
        .join("discord_pending_queue")
        .join(provider.as_str())
        .join(token_hash);
    std::fs::create_dir_all(&dir).expect("create pending queue dir");
    let queue = dir.join(format!("{}.json", channel_id.get()));
    let dispatch = dir.join(format!("{}.dispatch", channel_id.get()));
    std::fs::write(&queue, "[]").expect("seed queue artifact");
    std::fs::write(&dispatch, "{}").expect("seed dispatch artifact");

    clear_unowned_pending_queue_artifact(&provider, token_hash, channel_id)
        .expect("clear queue artifact");
    clear_unowned_pending_dispatch_artifact(&provider, token_hash, channel_id)
        .expect("clear dispatch artifact");
    assert!(!queue.exists());
    assert!(!dispatch.exists());
}

#[test]
fn unowned_cleanup_accounting_distinguishes_success_from_failure() {
    let mut cleared_unowned = 0usize;
    let mut cleanup_failed_unowned = 0usize;
    account_unowned_cleanup_result(
        &Ok(()),
        3,
        &mut cleared_unowned,
        &mut cleanup_failed_unowned,
    );
    account_unowned_cleanup_result(
        &Err("disk unavailable".to_string()),
        2,
        &mut cleared_unowned,
        &mut cleanup_failed_unowned,
    );
    assert_eq!(cleared_unowned, 3);
    assert_eq!(cleanup_failed_unowned, 2);
}

#[test]
fn recovery_flush_uses_bot_aware_cached_live_routing_on_all_durable_surfaces() {
    let source = include_str!("../recovery_flush.rs");
    let production = source
        .split("#[cfg(test)]")
        .next()
        .expect("production recovery-flush source");
    assert_eq!(
        production
            .matches("cached_live_bot_routing_status(")
            .count(),
        6,
        "one helper definition plus override, marker override, queue, dispatch, and placeholder gates"
    );
    assert_eq!(
        production
            .matches("RuntimeChannelBindingStatus::Unknown => continue")
            .count(),
        2,
        "override artifacts must be preserved for unresolved/sibling ownership"
    );
    assert_eq!(
        production
            .matches("RuntimeChannelBindingStatus::Unowned => {")
            .count(),
        5
    );
    assert_eq!(
        production
            .matches("clear_unowned_pending_queue_artifact(")
            .count(),
        3,
        "one helper definition plus both queue-backed restore surfaces"
    );
    assert_eq!(
        production
            .matches("clear_unowned_pending_dispatch_artifact(")
            .count(),
        3,
        "one helper definition plus both dispatch-marker restore surfaces"
    );
    assert_eq!(
        production
            .matches("account_unowned_cleanup_result(")
            .count(),
        3,
        "one helper definition plus queue and dispatch cleanup accounting"
    );
    assert_eq!(
        production
            .matches("cleanup_failed_unowned={cleanup_failed_unowned}")
            .count(),
        2,
        "queue and dispatch summaries must distinguish cleanup failures from successful clears"
    );
    assert!(!production.contains("resolve_runtime_channel_binding_status("));
    assert!(production.contains("let mut live_routing_status_cache"));
    let placeholder_gate = production
        .split("for (key @ (channel_id, user_msg_id), placeholder_msg_id) in")
        .nth(1)
        .expect("placeholder tri-state gate");
    let unknown = placeholder_gate
        .split("RuntimeChannelBindingStatus::Unknown => {")
        .nth(1)
        .and_then(|tail| {
            tail.split("RuntimeChannelBindingStatus::Unowned => {")
                .next()
        })
        .expect("placeholder Unknown arm");
    assert!(!unknown.contains(".insert(key, placeholder_msg_id);"));
    assert!(unknown.contains("Leave it disk-only"));
}
