use super::super::{
    restored_watcher_turn_from_inflight, terminal_relay_decision, watcher_stream_seed,
};
use crate::services::agent_protocol::TaskNotificationKind;
use crate::services::discord::inflight::{
    InflightTurnState, load_inflight_state, save_inflight_state,
};
use crate::services::provider::ProviderKind;

#[test]
fn task_notification_kind_restart_roundtrip_4253() {
    let runtime_root = tempfile::tempdir().expect("isolated runtime root");
    let _root_guard =
        crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", runtime_root.path());

    let provider = ProviderKind::Claude;
    let channel_id = 1_509_350_493_350_459_253;
    let tmux_session_name = "AgentDesk-claude-adk-4253-restart-roundtrip";
    let mut state = InflightTurnState::new(
        provider.clone(),
        channel_id,
        Some("claude-pipe".to_string()),
        343_742_347_365_974_026,
        4_253_001,
        4_253_002,
        "background task notification".to_string(),
        Some("session-4253".to_string()),
        Some(tmux_session_name.to_string()),
        Some("/tmp/agentdesk-4253-restart-roundtrip.jsonl".to_string()),
        None,
        425_300,
    );
    state.full_response = "background task completed".to_string();
    state.task_notification_kind = Some(TaskNotificationKind::Background);

    save_inflight_state(&state).expect("persist background inflight row");
    drop(state);

    let reloaded = load_inflight_state(&provider, channel_id).expect("reload inflight from disk");
    assert_eq!(
        reloaded.task_notification_kind,
        Some(TaskNotificationKind::Background),
        "disk reload must retain the background task-notification kind"
    );

    let restored = restored_watcher_turn_from_inflight(&reloaded, tmux_session_name, true)
        .expect("matching tmux session and nonzero message id must restore the watcher turn");
    assert_eq!(
        restored.task_notification_kind,
        Some(TaskNotificationKind::Background),
        "restart restoration must carry the persisted kind"
    );

    let seed = watcher_stream_seed(Some(restored));
    assert_eq!(
        seed.task_notification_kind,
        Some(TaskNotificationKind::Background),
        "the restarted watcher stream seed must carry the restored kind"
    );

    // This is the nearest pure decision called by the restored watcher terminal
    // path. With no assistant text observed, Background must suppress the
    // notification; losing the kind to None would incorrectly direct-send it.
    let terminal = terminal_relay_decision(true, seed.task_notification_kind, false);
    assert!(
        terminal.suppressed,
        "the restart terminal decision must consume Background as a task notification"
    );
    assert!(
        !terminal.should_direct_send,
        "Background without observed assistant text must not direct-send after restart"
    );

    let visible_terminal = terminal_relay_decision(true, seed.task_notification_kind, true);
    assert!(
        visible_terminal.should_direct_send && !visible_terminal.suppressed,
        "Background with observed assistant text remains user-visible after restart"
    );
    assert!(
        !visible_terminal.should_tag_monitor_origin,
        "Background must remain distinct from the monitor-auto-turn kind"
    );
}
