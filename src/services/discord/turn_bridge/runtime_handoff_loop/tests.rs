use super::*;
use crate::services::discord::inflight::{
    GuardedSaveOutcome, load_inflight_state, save_inflight_state,
};

fn runtime_seed(provider: ProviderKind, channel_id: u64) -> InflightTurnState {
    InflightTurnState::new(
        provider,
        channel_id,
        Some("adk-4259-r2".to_string()),
        343_742_347_365_974_026,
        77_010,
        18,
        "runtime handoff".to_string(),
        Some("provider-session-before-handoff".to_string()),
        None,
        Some("/seeded/runtime-output.jsonl".to_string()),
        None,
        512,
    )
}

async fn dispatch_process_handoff(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    state: &mut InflightTurnState,
    message: RuntimeHandoffLoopMessage,
    state_dirty: &mut bool,
) -> RuntimeHandoffLoopOutcome {
    let channel_id = ChannelId::new(state.channel_id);
    let mut terminal_control_ready_observed = false;
    let mut tmux_last_offset = None;
    let mut watcher_owner_channel_id = channel_id;
    let mut standby_relay_owns_output = false;
    let mut watcher_relay_available_for_turn = false;
    let mut watcher_handoff_claim_outcome = WatcherHandoffClaimOutcome::None;
    let mut tmux_handed_off = false;
    let mut watcher_owns_assistant_relay = false;
    let mut terminal_control_drain_until = None;
    let mut last_activity_heartbeat_at = None;

    handle_runtime_handoff_loop_message(
        message,
        RuntimeHandoffLoopContext {
            shared_owned: shared,
            provider,
            channel_id,
            done: false,
            adk_session_name: &None,
        },
        RuntimeHandoffLoopState {
            terminal_control_ready_observed: &mut terminal_control_ready_observed,
            tmux_last_offset: &mut tmux_last_offset,
            inflight_state: state,
            watcher_owner_channel_id: &mut watcher_owner_channel_id,
            standby_relay_owns_output: &mut standby_relay_owns_output,
            watcher_relay_available_for_turn: &mut watcher_relay_available_for_turn,
            watcher_handoff_claim_outcome: &mut watcher_handoff_claim_outcome,
            tmux_handed_off: &mut tmux_handed_off,
            watcher_owns_assistant_relay: &mut watcher_owns_assistant_relay,
            state_dirty,
            terminal_control_drain_until: &mut terminal_control_drain_until,
            last_activity_heartbeat_at: &mut last_activity_heartbeat_at,
        },
    )
    .await
}

#[tokio::test]
async fn process_runtime_ready_first_population_is_saved() {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let root = tempfile::tempdir().expect("runtime root");
    let _env_reset = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        root.path(),
    );
    let provider = ProviderKind::Codex;
    let mut state = runtime_seed(provider.clone(), 42_592_601);
    save_inflight_state(&state).expect("seed owner row");
    let shared = crate::services::discord::make_shared_data_for_tests();
    let mut state_dirty = false;

    let outcome = dispatch_process_handoff(
        &shared,
        &provider,
        &mut state,
        RuntimeHandoffLoopMessage::RuntimeReady {
            handoff: RuntimeHandoff::ProcessBackend {
                output_path: "/runtime/process-ready.jsonl".to_string(),
                session_name: "process-session-r2".to_string(),
                last_offset: 4096,
            },
        },
        &mut state_dirty,
    )
    .await;

    assert_eq!(outcome, Some(GuardedSaveOutcome::Saved));
    assert!(
        state_dirty,
        "the existing dirty flag from watcher-owner normalization remains queued"
    );
    let persisted = load_inflight_state(&provider, state.channel_id).expect("persisted row");
    assert_eq!(
        persisted.runtime_kind,
        Some(RuntimeHandoffKind::ProcessBackend)
    );
    assert_eq!(
        persisted.tmux_session_name.as_deref(),
        Some("process-session-r2")
    );
    assert_eq!(persisted.last_offset, 4096);
}

#[tokio::test]
async fn process_ready_skips_reowned_row_and_does_not_queue_stale_flush() {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let root = tempfile::tempdir().expect("runtime root");
    let _env_reset = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        root.path(),
    );
    let provider = ProviderKind::Claude;
    let mut state = runtime_seed(provider.clone(), 42_592_602);
    let mut newer = state.clone();
    newer.user_msg_id = 99_999;
    newer.output_path = Some("/runtime/newer-turn.jsonl".to_string());
    save_inflight_state(&newer).expect("seed re-owned row");
    let shared = crate::services::discord::make_shared_data_for_tests();
    let mut state_dirty = false;

    let outcome = dispatch_process_handoff(
        &shared,
        &provider,
        &mut state,
        RuntimeHandoffLoopMessage::ProcessReady {
            output_path: "/runtime/stale-process.jsonl".to_string(),
            session_name: "stale-process-session".to_string(),
            last_offset: 8192,
        },
        &mut state_dirty,
    )
    .await;

    assert_eq!(outcome, Some(GuardedSaveOutcome::IdentityMismatch));
    assert!(
        !state_dirty,
        "a stale handoff must not queue a later whole-row flush"
    );
    let persisted = load_inflight_state(&provider, state.channel_id).expect("preserved row");
    assert_eq!(persisted.user_msg_id, 99_999);
    assert_eq!(
        persisted.output_path.as_deref(),
        Some("/runtime/newer-turn.jsonl")
    );
}
