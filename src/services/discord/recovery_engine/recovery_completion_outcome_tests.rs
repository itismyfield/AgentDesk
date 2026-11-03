use super::{RecoveryCompletionOutcome, recovery_status_panel};
use crate::services::agent_protocol::StatusEvent;
use crate::services::provider::ProviderKind;
use poise::serenity_prelude::{ChannelId, MessageId};
use std::sync::{Arc, Mutex};

struct RuntimeRootGuard {
    previous: Option<std::ffi::OsString>,
    _root: tempfile::TempDir,
}

impl RuntimeRootGuard {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("runtime root");
        let previous = std::env::var_os("AGENTDESK_ROOT_DIR");
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", root.path()) };
        Self {
            previous,
            _root: root,
        }
    }
}

impl Drop for RuntimeRootGuard {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
            None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
        }
    }
}

fn isolate_agentdesk_runtime_root() -> (
    crate::config::test_env_lock::SharedTestEnvLockGuard,
    RuntimeRootGuard,
) {
    let lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    (lock, RuntimeRootGuard::new())
}

fn state_for_recovery(user_msg_id: u64) -> super::inflight::InflightTurnState {
    serde_json::from_value(serde_json::json!({
        "version": 9,
        "provider": "claude",
        "channel_id": 4243,
        "channel_name": "adk-cc",
        "request_owner_user_id": 7,
        "user_msg_id": user_msg_id,
        "current_msg_id": user_msg_id + 1,
        "current_msg_len": 0,
        "user_text": "prompt",
        "source": "text",
        "session_id": "session",
        "tmux_session_name": "AgentDesk-claude-adk-cc",
        "output_path": "/tmp/claude-transcript.jsonl",
        "input_fifo_path": null,
        "last_offset": 0,
        "full_response": "",
        "response_sent_offset": 0,
        "started_at": "2026-01-01 00:00:00",
        "updated_at": "2026-01-01 00:00:00"
    }))
    .expect("recovery test inflight state")
}

#[test]
fn emitted_lets_callers_proceed_with_dispatch_finalize() {
    assert!(RecoveryCompletionOutcome::Emitted.should_proceed());
}

#[test]
fn completion_prefers_guarded_persisted_fallback_message_id() {
    let mut snapshot = state_for_recovery(9101);
    snapshot.status_message_id = Some(3003);
    let mut persisted = state_for_recovery(9101);
    persisted.status_message_id = Some(4004);

    let status_msg_id =
        recovery_status_panel::message_id_for_completion(&snapshot, Some(&persisted));

    assert_eq!(status_msg_id, Some(super::MessageId::new(4004)));
}

#[test]
fn completion_skips_zero_persisted_and_snapshot_status_ids_without_panicking() {
    let mut snapshot = state_for_recovery(9101);
    snapshot.status_message_id = Some(0);
    let mut persisted = state_for_recovery(9101);
    persisted.status_message_id = Some(0);

    let status_msg_id =
        recovery_status_panel::message_id_for_completion(&snapshot, Some(&persisted));

    assert_eq!(status_msg_id, None);
}

#[test]
fn completion_skips_zero_persisted_status_then_uses_nonzero_snapshot() {
    let mut snapshot = state_for_recovery(9101);
    snapshot.status_message_id = Some(3003);
    let mut persisted = state_for_recovery(9101);
    persisted.status_message_id = Some(0);

    let status_msg_id =
        recovery_status_panel::message_id_for_completion(&snapshot, Some(&persisted));

    assert_eq!(status_msg_id, Some(super::MessageId::new(3003)));
}

#[test]
fn completion_ignores_persisted_id_from_different_turn() {
    let mut snapshot = state_for_recovery(9101);
    snapshot.status_message_id = Some(3003);
    let mut persisted = state_for_recovery(9201);
    persisted.status_message_id = Some(4004);

    let status_msg_id =
        recovery_status_panel::message_id_for_completion(&snapshot, Some(&persisted));

    assert_eq!(status_msg_id, Some(super::MessageId::new(3003)));
}

#[test]
fn footer_mode_skips_recovery_status_panel_completion_for_stale_persisted_id() {
    let mut snapshot = state_for_recovery(9101);
    snapshot.status_message_id = Some(3003);
    let mut persisted = state_for_recovery(9101);
    persisted.status_message_id = Some(4004);

    let target =
        recovery_status_panel::completion_target_for_flags(true, &snapshot, Some(&persisted));

    assert_eq!(
        target,
        Some(Some(super::MessageId::new(4004))),
        "retirement completes the same-turn persisted panel in place without requesting SendFallback"
    );
}

#[test]
fn flag_off_recovery_none_target_still_requests_send_fallback() {
    let snapshot = state_for_recovery(9101);

    let target = recovery_status_panel::completion_target_for_flags(true, &snapshot, None);

    assert_eq!(
        target, None,
        "retired recovery without a persisted panel must not request SendFallback"
    );
}

#[tokio::test]
async fn recovery_status_panel_completion_emits_background_agent_pending_payload() {
    let (_env_lock, _runtime_root) = isolate_agentdesk_runtime_root();
    let mut shared = super::super::make_shared_data_for_tests();
    Arc::get_mut(&mut shared)
        .expect("fresh test shared data should be uniquely owned")
        .ui
        .status_panel_v2_enabled = true;
    let http = poise::serenity_prelude::Http::new("Bot test-token");
    let provider = ProviderKind::Claude;
    let state = state_for_recovery(9101);
    let channel_id = ChannelId::new(state.channel_id);
    let started_at_unix = 1_700_000_000;
    shared.ui.placeholder_live_events.push_status_event(
        channel_id,
        StatusEvent::TurnCompleted {
            background: false,
            background_agent_pending: true,
        },
    );
    let mut last_status_panel_text = shared.ui.placeholder_live_events.render_status_panel(
        channel_id,
        &provider,
        started_at_unix,
    );

    let committed = super::super::turn_bridge::complete_status_panel_v2_with_http(
        &shared,
        &http,
        channel_id,
        Some(MessageId::new(4_047_301)),
        &provider,
        started_at_unix,
        &mut last_status_panel_text,
        false,
        true,
        "test_recovery_background_agent_pending_payload",
        (Some(state.user_msg_id), Some(&state)),
    )
    .await;

    assert!(committed);
    let rendered = shared
        .ui
        .placeholder_live_events
        .render_completion_footer(channel_id, &provider, "⠸");
    let block = rendered.block.expect("background-agent pending footer");

    assert!(rendered.has_unfinished_entries);
    assert!(block.contains("Background agents"));
    assert!(block.contains("Waiting for background agents ⠸"));
}

#[tokio::test]
async fn recovery_status_panel_completion_producer_threads_sniffed_background_agent_pending() {
    let (_env_lock, _runtime_root) = isolate_agentdesk_runtime_root();
    for (pending, channel_raw) in [(true, 4_047_311), (false, 4_047_312)] {
        let shared = super::super::make_shared_data_for_tests();
        let provider = ProviderKind::Claude;
        let mut state = state_for_recovery(9101);
        state.channel_id = channel_raw;
        state.tmux_session_name = Some("AgentDesk-claude-recovery-background-test".to_string());
        let channel_id = ChannelId::new(state.channel_id);
        let observed_tmux_session = Arc::new(Mutex::new(Vec::new()));
        let sniffer_observed_tmux_session = observed_tmux_session.clone();
        let sink_shared = shared.clone();

        let outcome = super::complete_recovery_status_panel_with_sniffer_and_sink(
            &state,
            move |tmux_session_name| async move {
                sniffer_observed_tmux_session
                    .lock()
                    .expect("observed tmux session lock")
                    .push(tmux_session_name);
                pending
            },
            move |background_agent_pending| async move {
                sink_shared.ui.placeholder_live_events.push_status_event(
                    channel_id,
                    StatusEvent::TurnCompleted {
                        background: false,
                        background_agent_pending,
                    },
                );
                true
            },
        )
        .await;

        assert_eq!(outcome, RecoveryCompletionOutcome::Emitted);
        assert_eq!(
            observed_tmux_session
                .lock()
                .expect("observed tmux session lock")
                .as_slice(),
            &[Some(
                "AgentDesk-claude-recovery-background-test".to_string()
            )]
        );

        let rendered = shared
            .ui
            .placeholder_live_events
            .render_completion_footer(channel_id, &provider, "⠸");
        let block_has_background_agents = rendered
            .block
            .as_deref()
            .is_some_and(|block| block.contains("Background agents"));

        assert_eq!(rendered.has_unfinished_entries, pending);
        assert_eq!(block_has_background_agents, pending);
    }
}

#[test]
fn recovery_takeover_forgets_registered_completion_footer_target() {
    let channel_id = ChannelId::new(3_089_201);
    let shared = super::super::make_shared_data_for_tests();
    super::super::footer_view_reconciler::completion_footer_forget_registered_target(channel_id);
    let _ = super::super::footer_view_reconciler::register_completion_footer_target(
        channel_id,
        MessageId::new(3_089_301),
        &ProviderKind::Claude,
        1_800_000_000,
        "Final answer",
        None,
        true,
    );

    assert!(
        super::super::footer_view_reconciler::note_footer_suppressed_for_message_takeover(
            channel_id,
            MessageId::new(3_089_301),
        )
    );

    assert_eq!(
        super::super::footer_view_reconciler::completion_footer_edit_for_registered_target_at(
            shared.as_ref(),
            channel_id,
            "⠸",
            1_800_000_005,
        ),
        None
    );
}

#[test]
fn recovery_takeover_keeps_different_completion_footer_target() {
    let channel_id = ChannelId::new(3_089_211);
    let shared = super::super::make_shared_data_for_tests();
    super::super::footer_view_reconciler::completion_footer_forget_registered_target(channel_id);
    let _ = super::super::footer_view_reconciler::register_completion_footer_target(
        channel_id,
        MessageId::new(3_089_311),
        &ProviderKind::Claude,
        1_800_000_000,
        "Final answer",
        None,
        true,
    );

    assert!(
        !super::super::footer_view_reconciler::note_footer_suppressed_for_message_takeover(
            channel_id,
            MessageId::new(3_089_312),
        )
    );

    assert!(
        super::super::footer_view_reconciler::completion_footer_edit_for_registered_target_at(
            shared.as_ref(),
            channel_id,
            "⠸",
            1_800_000_005,
        )
        .is_some()
    );
    super::super::footer_view_reconciler::completion_footer_forget_registered_target(channel_id);
}

#[tokio::test(flavor = "current_thread")]
async fn recovered_notices_record_bodies_without_counting_neutral_markers() {
    use crate::services::discord::outbound::raw_output::tests as observation;
    use crate::services::tui_o::repost::io::tests::Mock;
    if observation::isolated(
        concat!(
            module_path!(),
            "::recovered_notices_record_bodies_without_counting_neutral_markers"
        )
        .trim_start_matches("agentdesk::"),
    ) {
        return;
    }
    let journal = observation::journal().await;
    let mut row = state_for_recovery(9904);
    row.runtime_kind = Some(crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui);
    let neutral = super::RECOVERY_NEUTRAL_NOTICE;
    let partial = format!("saved answer\n\n{neutral}");
    for (entry, text) in [
        ("notice", "saved provider answer"),
        ("notice", super::RECOVERED_WITHOUT_TEXT),
        ("notice", neutral),
        ("notice", partial.as_str()),
        ("terminal", neutral),
        ("worktree", "fixture worktree is unavailable"),
    ] {
        let mock = Mock::start(vec![observation::message(9904)]).await;
        let http = Arc::new(observation::client(&mock));
        let shared = crate::services::discord::make_shared_data_for_tests();
        let outcome = match entry {
            "worktree" => {
                super::completion_delivery::observe_worktree_recovery_refusal(
                    &row,
                    super::relay_recovered_terminal_text_to_placeholder(
                        &http,
                        &shared,
                        ChannelId::new(row.channel_id),
                        Some(MessageId::new(row.current_msg_id)),
                        &format!("❌ {text}\nmain workspace fallback blocked."),
                        None,
                    ),
                )
                .await
            }
            "terminal" => {
                super::relay_recovered_terminal_text_to_placeholder(
                    &http,
                    &shared,
                    ChannelId::new(row.channel_id),
                    Some(MessageId::new(row.current_msg_id)),
                    text,
                    None,
                )
                .await
            }
            _ => {
                super::relay_recovery_terminal_notice(
                    &http,
                    &shared,
                    &ProviderKind::Claude,
                    &row,
                    text,
                )
                .await
            }
        };
        assert_eq!(outcome, super::RecoveryRelayOutcome::Delivered, "{entry}");
        assert_eq!(mock.requests().len(), 1, "{entry}");
    }
    observation::flushed().await;
    let recorded: Vec<serde_json::Value> = std::fs::read_to_string(journal)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .filter(|row: &serde_json::Value| row["kind"] == "target")
        .collect();
    assert_eq!(
        recorded.len(),
        4,
        "two real bodies, including a partial before a notice; registered notices are excluded"
    );
    for event in recorded {
        assert_eq!(event["census"]["site"], "W30");
        assert_eq!(event["census"]["reason"], "NO_CLAIM");
        assert_eq!(event["census"]["binding_mode"], "TUI");
    }
}
