use super::super::complete_bridge_terminal_footer_or_status_panel_with_sniffer;
use super::{
    ChannelId, InflightTurnState, MessageId, ProviderKind, StatusPanelCompletionAction,
    bridge_epilogue_identity_guards_inflight_clear, complete_status_panel_v2,
    migrate_separate_status_panel_to_footer, should_open_long_running_placeholder_controller,
    status_panel_completion_action, status_panel_completion_edit_aliases_newer_turn,
    status_panel_completion_ready_after_terminal_body, status_panel_message_id_for_turn,
    status_panel_wip_inflight_for_completion,
};
use crate::services::discord::formatting::ReplaceLongMessageOutcome;
use crate::services::discord::gateway::TurnGateway;
use crate::services::discord::inflight::{
    GuardedClearOutcome, InflightTurnIdentity, clear_inflight_state,
    clear_inflight_state_if_matches, clear_inflight_state_if_matches_zero_owned,
    load_inflight_state, save_inflight_state,
};
use crate::services::git::GitCommand;
use std::fs;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

type TestGatewayFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

struct StatusPanelFallbackGateway {
    sent_messages: Arc<Mutex<Vec<String>>>,
    edited_message_ids: Arc<Mutex<Vec<MessageId>>>,
    edit_error: Option<String>,
    send_id: MessageId,
    can_chain_locally: bool,
}

impl StatusPanelFallbackGateway {
    fn with_edit_error(error: &str) -> Self {
        Self {
            edit_error: Some(error.to_string()),
            ..Self::default()
        }
    }

    fn without_local_chain() -> Self {
        Self {
            can_chain_locally: false,
            ..Self::default()
        }
    }
}

impl Default for StatusPanelFallbackGateway {
    fn default() -> Self {
        Self {
            sent_messages: Arc::new(Mutex::new(Vec::new())),
            edited_message_ids: Arc::new(Mutex::new(Vec::new())),
            edit_error: None,
            send_id: MessageId::new(1_500_000_000_000_999),
            can_chain_locally: true,
        }
    }
}

impl TurnGateway for StatusPanelFallbackGateway {
    fn send_message<'a>(
        &'a self,
        _channel_id: ChannelId,
        content: &'a str,
    ) -> TestGatewayFuture<'a, Result<MessageId, String>> {
        let sent_messages = self.sent_messages.clone();
        let send_id = self.send_id;
        Box::pin(async move {
            sent_messages
                .lock()
                .expect("sent messages lock")
                .push(content.to_string());
            Ok(send_id)
        })
    }

    fn edit_message<'a>(
        &'a self,
        _channel_id: ChannelId,
        message_id: MessageId,
        _content: &'a str,
    ) -> TestGatewayFuture<'a, Result<(), String>> {
        let edited_message_ids = self.edited_message_ids.clone();
        let edit_error = self.edit_error.clone();
        Box::pin(async move {
            edited_message_ids
                .lock()
                .expect("edited ids lock")
                .push(message_id);
            match edit_error {
                Some(error) => Err(error),
                None => Ok(()),
            }
        })
    }

    fn replace_message_with_outcome<'a>(
        &'a self,
        _channel_id: ChannelId,
        _message_id: MessageId,
        _content: &'a str,
    ) -> TestGatewayFuture<'a, Result<ReplaceLongMessageOutcome, String>> {
        Box::pin(async { Ok(ReplaceLongMessageOutcome::EditedOriginal) })
    }

    fn schedule_retry_with_history<'a>(
        &'a self,
        _channel_id: ChannelId,
        _user_message_id: MessageId,
        _user_text: &'a str,
    ) -> TestGatewayFuture<'a, ()> {
        Box::pin(async {})
    }

    fn dispatch_queued_turn<'a>(
        &'a self,
        _channel_id: ChannelId,
        _intervention: &'a super::super::Intervention,
        _request_owner_name: &'a str,
        _has_more_queued_turns: bool,
    ) -> TestGatewayFuture<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }

    fn validate_live_routing<'a>(
        &'a self,
        _channel_id: ChannelId,
    ) -> TestGatewayFuture<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }

    fn requester_mention(&self) -> Option<String> {
        None
    }

    fn can_chain_locally(&self) -> bool {
        self.can_chain_locally
    }

    fn bot_owner_provider(&self) -> Option<ProviderKind> {
        Some(ProviderKind::Claude)
    }
}

#[tokio::test]
async fn status_panel_wip_warning_does_not_use_synthetic_gateway_when_http_path_required() {
    let Some(worktree) = init_git_repo_for_wip_warning() else {
        return;
    };
    fs::write(worktree.path().join("untracked.txt"), "untracked\n").expect("write untracked file");

    let shared = make_status_panel_v2_shared_for_tests();
    let gateway = StatusPanelFallbackGateway::without_local_chain();
    let state =
        wip_warning_inflight_state(&ProviderKind::Claude, 3_792_010, 3_792_011, worktree.path());

    let outcome = super::warn_turn_end_wip_before_status_panel_commit(
        shared.as_ref(),
        &gateway,
        ChannelId::new(3_792_010),
        Some(&state),
        "test_wip_warning_http_path",
    )
    .await;

    assert_eq!(
        outcome,
        crate::services::discord::turn_end_wip_warning::TurnEndWipWarningOutcome::SendFailed,
        "no local-chain gateway plus no HTTP handle should fail instead of being swallowed"
    );
    assert!(
        gateway
            .sent_messages
            .lock()
            .expect("sent messages lock")
            .is_empty(),
        "headless/synthetic gateway sends must not absorb WIP warnings"
    );
}

#[test]
fn status_panel_v2_disables_long_running_placeholder_controller() {
    assert!(!should_open_long_running_placeholder_controller(true));
    assert!(should_open_long_running_placeholder_controller(false));
}

fn make_status_panel_v2_shared_for_tests() -> Arc<crate::services::discord::SharedData> {
    let mut shared = super::super::make_shared_data_for_tests();
    Arc::get_mut(&mut shared)
        .expect("fresh test shared data should be uniquely owned")
        .ui
        .status_panel_v2_enabled = true;
    shared
}

fn test_inflight_state() -> InflightTurnState {
    serde_json::from_value(serde_json::json!({
        "version": 9,
        "provider": "codex",
        "channel_id": 1,
        "channel_name": "adk-cdx-test",
        "request_owner_user_id": 2,
        "user_msg_id": 3,
        "current_msg_id": 4,
        "current_msg_len": 0,
        "user_text": "test turn",
        "source": "text",
        "session_id": null,
        "tmux_session_name": null,
        "output_path": null,
        "input_fifo_path": null,
        "last_offset": 0,
        "full_response": "",
        "response_sent_offset": 0,
        "started_at": "2026-01-01 00:00:00",
        "updated_at": "2026-01-01 00:00:00"
    }))
    .expect("test inflight state")
}

fn git_available_for_wip_warning() -> bool {
    GitCommand::new().arg("--version").run_output().is_ok()
}

fn init_git_repo_for_wip_warning() -> Option<tempfile::TempDir> {
    if !git_available_for_wip_warning() {
        return None;
    }
    let temp = tempfile::tempdir().expect("tempdir");
    GitCommand::new()
        .repo(temp.path())
        .arg("init")
        .run_output()
        .expect("git init");
    Some(temp)
}

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

fn isolate_agentdesk_runtime_root() -> (std::sync::MutexGuard<'static, ()>, RuntimeRootGuard) {
    let lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let root = RuntimeRootGuard::new();
    (lock, root)
}

fn wip_warning_inflight_state(
    provider: &ProviderKind,
    channel_id: u64,
    user_msg_id: u64,
    worktree_path: &Path,
) -> InflightTurnState {
    let mut state: InflightTurnState = serde_json::from_value(serde_json::json!({
        "version": 9,
        "provider": provider.as_str(),
        "channel_id": channel_id,
        "channel_name": "wip-warning-integration-test",
        "request_owner_user_id": 42,
        "user_msg_id": user_msg_id,
        "current_msg_id": user_msg_id + 1,
        "current_msg_len": 0,
        "user_text": "turn",
        "source": "text",
        "session_id": null,
        "tmux_session_name": null,
        "output_path": null,
        "input_fifo_path": null,
        "last_offset": 0,
        "full_response": "",
        "response_sent_offset": 0,
        "started_at": "2026-01-01 00:00:00",
        "updated_at": "2026-01-01 00:00:00"
    }))
    .expect("wip warning inflight state");
    state.worktree_path = Some(worktree_path.display().to_string());
    state
}

#[test]
fn fresh_turn_discards_stale_status_panel_message_id() {
    let mut state = test_inflight_state();
    state.status_message_id = Some(99);

    let status_panel_msg_id = status_panel_message_id_for_turn(&mut state, false);

    assert_eq!(status_panel_msg_id, None);
    assert_eq!(state.status_message_id, None);
}

#[test]
fn resume_turn_preserves_status_panel_message_id() {
    let mut state = test_inflight_state();
    state.status_message_id = Some(99);

    let status_panel_msg_id = status_panel_message_id_for_turn(&mut state, true);

    assert_eq!(status_panel_msg_id, Some(MessageId::new(99)));
    assert_eq!(state.status_message_id, Some(99));
}

#[test]
fn resume_turn_discards_synthetic_status_panel_message_id() {
    let mut state = test_inflight_state();
    state.status_message_id = Some(9_100_000_000_000_000_123);

    let status_panel_msg_id = status_panel_message_id_for_turn(&mut state, true);

    assert_eq!(status_panel_msg_id, None);
    assert_eq!(state.status_message_id, None);
}

// #3560 codex review: default-OFF → default-ON migration guard. A turn that
// created a real separate status panel under default-OFF must have that panel
// finalized (edited to a migration notice) when it resumes under footer mode,
// instead of orphaning the Discord message.
#[tokio::test]
async fn footer_migration_edits_and_clears_existing_separate_panel() {
    let gateway = StatusPanelFallbackGateway::default();
    let mut state = test_inflight_state();
    state.status_message_id = Some(9_876_543_210);

    let migrated =
        migrate_separate_status_panel_to_footer(&gateway, ChannelId::new(4321), &mut state).await;

    assert!(
        migrated,
        "an existing separate panel should report a migration"
    );
    assert_eq!(
        state.status_message_id, None,
        "handle must be cleared after migration"
    );
    let edited = gateway.edited_message_ids.lock().expect("edited ids lock");
    assert_eq!(
        edited.as_slice(),
        &[MessageId::new(9_876_543_210)],
        "the old separate panel must be edited exactly once"
    );
}

// Synthetic-headless ids are not real Discord messages, so footer migration
// clears the handle without issuing an edit.
#[tokio::test]
async fn footer_migration_skips_synthetic_headless_panel() {
    let gateway = StatusPanelFallbackGateway::default();
    let mut state = test_inflight_state();
    state.status_message_id = Some(9_100_000_000_000_000_123);

    let migrated =
        migrate_separate_status_panel_to_footer(&gateway, ChannelId::new(4321), &mut state).await;

    assert!(migrated);
    assert_eq!(state.status_message_id, None);
    assert!(
        gateway
            .edited_message_ids
            .lock()
            .expect("edited ids lock")
            .is_empty(),
        "synthetic-headless panels must not be edited"
    );
}

// When no separate panel handle exists (footer mode from the start) the
// migration is a no-op and reports nothing was migrated.
#[tokio::test]
async fn footer_migration_noop_without_existing_panel() {
    let gateway = StatusPanelFallbackGateway::default();
    let mut state = test_inflight_state();
    state.status_message_id = None;

    let migrated =
        migrate_separate_status_panel_to_footer(&gateway, ChannelId::new(4321), &mut state).await;

    assert!(!migrated);
    assert_eq!(state.status_message_id, None);
    assert!(
        gateway
            .edited_message_ids
            .lock()
            .expect("edited ids lock")
            .is_empty()
    );
}

#[test]
fn completion_action_does_not_fallback_when_panel_text_already_committed() {
    let panel_text = "응답 완료";

    let action = status_panel_completion_action(None, panel_text, panel_text);

    assert_eq!(action, StatusPanelCompletionAction::AlreadyCommitted);
}

#[test]
fn completion_action_treats_synthetic_id_as_missing_target() {
    let action = status_panel_completion_action(
        Some(MessageId::new(9_100_000_000_000_000_123)),
        "",
        "응답 완료",
    );

    assert_eq!(action, StatusPanelCompletionAction::SendFallback);
}

#[test]
fn completion_action_edits_real_status_panel_message_id() {
    let message_id = MessageId::new(1510319194921504931);

    let action = status_panel_completion_action(Some(message_id), "", "응답 완료");

    assert_eq!(action, StatusPanelCompletionAction::Edit(message_id));
}

// #3161: the bridge-path status-panel turn-aliasing gate. A NEWER follow-up
// turn re-adopted THIS turn's captured panel between turn start and
// completion (the on-disk row now carries a different, real `user_msg_id`
// pointing at the SAME `status_message_id`), so the older bridge turn must
// NOT edit it — that would alias the newer turn's live panel.
#[test]
fn completion_edit_skips_when_newer_turn_owns_this_panel() {
    let panel = MessageId::new(1510319194921504931);

    assert!(
        status_panel_completion_edit_aliases_newer_turn(
            7_000_001,
            Some(panel),
            7_000_999,
            Some(panel.get()),
        ),
        "a different real on-disk turn owning THIS panel must suppress the edit"
    );
}

// The common, non-aliased case: the on-disk row is still THIS turn → edit
// proceeds. This is the GREEN companion to the aliasing case above.
#[test]
fn completion_edit_proceeds_when_same_turn_still_owns_panel() {
    let panel = MessageId::new(1510319194921504931);

    assert!(
        !status_panel_completion_edit_aliases_newer_turn(
            7_000_001,
            Some(panel),
            7_000_001,
            Some(panel.get()),
        ),
        "the SAME turn still owning the panel must complete normally"
    );
}

// Over-suppression guard (issue requirement): an in-range id==0
// bridge/watcher-direct turn (TUI-direct / external-input) must STILL
// complete its panel even though the on-disk id differs — a 0-id this-turn
// can never be proven stale this way, and the panel was never re-adopted.
#[test]
fn completion_edit_proceeds_for_in_range_id_zero_turn() {
    let panel = MessageId::new(1510319194921504931);

    assert!(
        !status_panel_completion_edit_aliases_newer_turn(
            0,
            Some(panel),
            7_000_999,
            Some(panel.get()),
        ),
        "an id==0 watcher-direct/bridge turn must not be suppressed"
    );
}

// A different on-disk turn that does NOT own this turn's panel (e.g. it
// adopted a different panel, or none) is not evidence of aliasing → edit
// proceeds. Guards against over-suppression from a stale unrelated row.
#[test]
fn completion_edit_proceeds_when_newer_turn_owns_different_panel() {
    let panel = MessageId::new(1510319194921504931);
    let other_panel = 1510319194921599999u64;

    assert!(
        !status_panel_completion_edit_aliases_newer_turn(
            7_000_001,
            Some(panel),
            7_000_999,
            Some(other_panel),
        ),
        "a newer turn owning a DIFFERENT panel does not alias this one"
    );
    assert!(
        !status_panel_completion_edit_aliases_newer_turn(7_000_001, Some(panel), 7_000_999, None),
        "a newer turn with no panel does not alias this one"
    );
}

// No captured panel id (or a synthetic-headless one) → nothing to alias →
// edit proceeds (routes to the fallback path as today).
#[test]
fn completion_edit_proceeds_when_no_real_panel_captured() {
    assert!(
        !status_panel_completion_edit_aliases_newer_turn(7_000_001, None, 7_000_999, Some(123)),
        "no captured panel id cannot alias"
    );
    assert!(
        !status_panel_completion_edit_aliases_newer_turn(
            7_000_001,
            Some(MessageId::new(9_100_000_000_000_000_123)),
            7_000_999,
            Some(9_100_000_000_000_000_123),
        ),
        "a synthetic-headless captured panel id cannot alias"
    );
}

// An absent on-disk identity (on_disk_user_msg_id == 0, the inflight row's
// default / cleared identity) is not a newer-owner proof → edit proceeds.
#[test]
fn completion_edit_proceeds_when_on_disk_identity_absent() {
    let panel = MessageId::new(1510319194921504931);

    assert!(
        !status_panel_completion_edit_aliases_newer_turn(
            7_000_001,
            Some(panel),
            0,
            Some(panel.get()),
        ),
        "an id==0 on-disk row is not proof of a newer owner"
    );
}

#[test]
fn status_panel_completion_waits_for_visible_terminal_body() {
    assert!(
        !status_panel_completion_ready_after_terminal_body(true, false, false),
        "terminal delivery accepted by an async body path is not enough to post completion"
    );
    assert!(
        status_panel_completion_ready_after_terminal_body(true, true, false),
        "completion may post once the terminal body is visibly committed"
    );
    assert!(
        !status_panel_completion_ready_after_terminal_body(true, true, true),
        "cleanup retry preservation must still suppress visible completion"
    );
}

#[tokio::test]
async fn status_panel_fallback_completion_is_blocked_until_body_visible() {
    let (_env_lock, _runtime_root) = isolate_agentdesk_runtime_root();
    let shared = make_status_panel_v2_shared_for_tests();
    let gateway = StatusPanelFallbackGateway::default();
    let provider = ProviderKind::Claude;
    let channel_id = ChannelId::new(1509350490461180105);
    let mut last_status_panel_text = String::new();

    if status_panel_completion_ready_after_terminal_body(true, false, false) {
        let _ = complete_status_panel_v2(
            shared.as_ref(),
            &gateway,
            channel_id,
            Some(MessageId::new(9_100_000_000_000_000_123)),
            &provider,
            1_700_000_000,
            &mut last_status_panel_text,
            false,
            false,
            "test_completion_before_body",
            1510319194921504929,
        )
        .await;
    }

    assert!(
        gateway
            .sent_messages
            .lock()
            .expect("sent messages lock")
            .is_empty(),
        "fallback completion must not send before the terminal body is visible"
    );

    if status_panel_completion_ready_after_terminal_body(true, true, false) {
        let committed = complete_status_panel_v2(
            shared.as_ref(),
            &gateway,
            channel_id,
            Some(MessageId::new(9_100_000_000_000_000_123)),
            &provider,
            1_700_000_000,
            &mut last_status_panel_text,
            false,
            false,
            "test_completion_after_body",
            1510319194921504929,
        )
        .await;
        assert!(committed);
    }

    let sent_messages = gateway
        .sent_messages
        .lock()
        .expect("sent messages lock")
        .clone();
    assert_eq!(sent_messages.len(), 1);
    assert!(sent_messages[0].contains("완료"));
}

#[tokio::test]
async fn bridge_status_panel_completion_emits_background_agent_pending_payload() {
    let (_env_lock, _runtime_root) = isolate_agentdesk_runtime_root();
    let shared = make_status_panel_v2_shared_for_tests();
    let gateway = StatusPanelFallbackGateway::default();
    let provider = ProviderKind::Claude;
    let channel_id = ChannelId::new(4_047_401);
    let mut last_status_panel_text = String::new();

    let committed = complete_status_panel_v2(
        shared.as_ref(),
        &gateway,
        channel_id,
        None,
        &provider,
        1_700_000_000,
        &mut last_status_panel_text,
        false,
        true,
        "test_bridge_background_agent_pending_payload",
        4_047_402,
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
async fn bridge_status_panel_completion_producer_threads_sniffed_background_agent_pending() {
    for (pending, channel_raw) in [(true, 4_047_411), (false, 4_047_412)] {
        let (_env_lock, _runtime_root) = isolate_agentdesk_runtime_root();
        let shared = make_status_panel_v2_shared_for_tests();
        let gateway = StatusPanelFallbackGateway::default();
        let provider = ProviderKind::Claude;
        let channel_id = ChannelId::new(channel_raw);
        let mut last_status_panel_text = String::new();
        let observed_tmux_session = Arc::new(Mutex::new(Vec::new()));
        let sniffer_observed_tmux_session = observed_tmux_session.clone();

        let committed = complete_bridge_terminal_footer_or_status_panel_with_sniffer(
            shared.as_ref(),
            &gateway,
            channel_id,
            MessageId::new(channel_raw + 1),
            Some(MessageId::new(channel_raw + 2)),
            None,
            &provider,
            1_700_000_000,
            &mut last_status_panel_text,
            false,
            false,
            Some("Final answer"),
            "⠸",
            0,
            Some("AgentDesk-claude-status-panel-background-test".to_string()),
            move |tmux_session_name| async move {
                sniffer_observed_tmux_session
                    .lock()
                    .expect("observed tmux session lock")
                    .push(tmux_session_name);
                pending
            },
        )
        .await;

        assert!(committed);
        assert_eq!(
            observed_tmux_session
                .lock()
                .expect("observed tmux session lock")
                .as_slice(),
            &[Some(
                "AgentDesk-claude-status-panel-background-test".to_string()
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

#[tokio::test]
async fn status_panel_completion_fallback_posts_when_message_id_is_synthetic() {
    let (_env_lock, _runtime_root) = isolate_agentdesk_runtime_root();
    let shared = make_status_panel_v2_shared_for_tests();
    let gateway = StatusPanelFallbackGateway::default();
    let provider = ProviderKind::Claude;
    let channel_id = ChannelId::new(1509350490461180105);
    let mut last_status_panel_text = String::new();

    let committed = complete_status_panel_v2(
        shared.as_ref(),
        &gateway,
        channel_id,
        Some(MessageId::new(9_100_000_000_000_000_123)),
        &provider,
        1_700_000_000,
        &mut last_status_panel_text,
        false,
        false,
        "test_synthetic_status_panel_id",
        1510319194921504929,
    )
    .await;

    assert!(committed);
    assert!(
        gateway
            .edited_message_ids
            .lock()
            .expect("edited ids lock")
            .is_empty(),
        "synthetic status-panel ids must not be edited through Discord"
    );
    let sent_messages = gateway
        .sent_messages
        .lock()
        .expect("sent messages lock")
        .clone();
    assert_eq!(sent_messages.len(), 1);
    assert!(sent_messages[0].contains("완료"));
    assert_eq!(last_status_panel_text, sent_messages[0]);

    let committed = complete_status_panel_v2(
        shared.as_ref(),
        &gateway,
        channel_id,
        Some(MessageId::new(9_100_000_000_000_000_123)),
        &provider,
        1_700_000_000,
        &mut last_status_panel_text,
        false,
        false,
        "test_synthetic_status_panel_id_retry",
        1510319194921504929,
    )
    .await;

    assert!(committed);
    assert_eq!(
        gateway
            .sent_messages
            .lock()
            .expect("sent messages lock")
            .len(),
        1,
        "same completed panel text must not send duplicate fallback panels"
    );
}

#[tokio::test]
async fn status_panel_completion_sends_wip_warning_before_completion_surface() {
    let Some(worktree) = init_git_repo_for_wip_warning() else {
        return;
    };
    fs::write(worktree.path().join("untracked.txt"), "untracked\n").expect("write untracked file");

    let (_env_lock, _runtime_root) = isolate_agentdesk_runtime_root();

    let shared = make_status_panel_v2_shared_for_tests();
    let gateway = StatusPanelFallbackGateway::default();
    let provider = ProviderKind::Claude;
    let channel_id = ChannelId::new(3_792_000);
    let user_msg_id = 3_792_001;
    save_inflight_state(&wip_warning_inflight_state(
        &provider,
        channel_id.get(),
        user_msg_id,
        worktree.path(),
    ))
    .expect("save inflight state");

    let mut last_status_panel_text = String::new();
    let committed = complete_status_panel_v2(
        shared.as_ref(),
        &gateway,
        channel_id,
        None,
        &provider,
        1_700_000_000,
        &mut last_status_panel_text,
        false,
        false,
        "test_wip_warning_order",
        user_msg_id,
    )
    .await;

    assert!(committed);
    let sent_messages = gateway
        .sent_messages
        .lock()
        .expect("sent messages lock")
        .clone();
    assert_eq!(sent_messages.len(), 2);
    assert!(sent_messages[0].contains("WIP uncommitted files detected"));
    assert!(sent_messages[0].contains(&format!("Workspace: `{}`", worktree.path().display())));
    assert!(sent_messages[0].contains("Counts: 0 staged, 0 unstaged, 1 untracked."));
    assert!(sent_messages[1].contains("완료"));

    let committed_retry = complete_status_panel_v2(
        shared.as_ref(),
        &gateway,
        channel_id,
        None,
        &provider,
        1_700_000_000,
        &mut last_status_panel_text,
        false,
        false,
        "test_wip_warning_order_retry",
        user_msg_id,
    )
    .await;

    assert!(committed_retry);
    assert_eq!(
        gateway
            .sent_messages
            .lock()
            .expect("sent messages lock")
            .len(),
        2,
        "already-committed completion retries must not duplicate WIP warnings"
    );
}

#[test]
fn status_panel_wip_completion_uses_preloaded_recovery_snapshot_after_cleanup() {
    let (_env_lock, _runtime_root) = isolate_agentdesk_runtime_root();
    let worktree = tempfile::tempdir().expect("worktree tempdir");
    let provider = ProviderKind::Claude;
    let channel_id = ChannelId::new(3_792_020);
    let user_msg_id = 3_792_021;
    let state =
        wip_warning_inflight_state(&provider, channel_id.get(), user_msg_id, worktree.path());
    save_inflight_state(&state).expect("save inflight state");
    assert!(clear_inflight_state(&provider, channel_id.get()));
    assert!(
        crate::services::discord::turn_end_wip_warning::load_matching_inflight_state(
            &provider,
            channel_id,
            Some(user_msg_id)
        )
        .is_none(),
        "cleanup should remove the disk row that the old HTTP path reloaded"
    );

    let selected = status_panel_wip_inflight_for_completion(
        Some(&state),
        &provider,
        channel_id,
        Some(user_msg_id),
    )
    .expect("preloaded recovery snapshot should remain eligible");

    assert_eq!(selected.as_inflight().user_msg_id, user_msg_id);
    assert_eq!(selected.as_inflight().channel_id, channel_id.get());
    assert_eq!(selected.as_inflight().worktree_path, state.worktree_path);
}

#[tokio::test]
async fn status_panel_completion_fallback_posts_after_unknown_message_edit() {
    let (_env_lock, _runtime_root) = isolate_agentdesk_runtime_root();
    let shared = make_status_panel_v2_shared_for_tests();
    let gateway = StatusPanelFallbackGateway::with_edit_error("Unknown Message");
    let provider = ProviderKind::Claude;
    let channel_id = ChannelId::new(1509350490461180105);
    let stale_status_msg_id = MessageId::new(1_500_000_000_000_111);
    let mut last_status_panel_text = String::new();

    let committed = complete_status_panel_v2(
        shared.as_ref(),
        &gateway,
        channel_id,
        Some(stale_status_msg_id),
        &provider,
        1_700_000_000,
        &mut last_status_panel_text,
        false,
        false,
        "test_unknown_status_panel_id",
        1510319194921504929,
    )
    .await;

    assert!(committed);
    assert_eq!(
        gateway
            .edited_message_ids
            .lock()
            .expect("edited ids lock")
            .as_slice(),
        &[stale_status_msg_id]
    );
    let sent_messages = gateway
        .sent_messages
        .lock()
        .expect("sent messages lock")
        .clone();
    assert_eq!(sent_messages.len(), 1);
    assert!(sent_messages[0].contains("완료"));
    assert_eq!(last_status_panel_text, sent_messages[0]);
}

#[tokio::test]
async fn status_panel_completion_purges_pending_bind_for_final_panel() {
    let (_env_lock, _runtime_root) = isolate_agentdesk_runtime_root();
    let shared = make_status_panel_v2_shared_for_tests();
    let gateway = StatusPanelFallbackGateway::default();
    let provider = ProviderKind::Claude;
    let channel_id = ChannelId::new(3_805_401);
    let user_msg_id = 3_805_402;
    let panel = MessageId::new(1_500_000_000_380_540);
    let mut state = test_inflight_state();
    state.provider = provider.as_str().to_string();
    state.channel_id = channel_id.get();
    state.user_msg_id = user_msg_id;
    state.request_owner_user_id = user_msg_id;
    state.current_msg_id = user_msg_id + 1;
    state.status_message_id = Some(panel.get());
    save_inflight_state(&state).expect("save inflight state");
    crate::services::discord::status_panel_orphan_store::enqueue_pending_bind(
        &provider,
        &shared.token_hash,
        channel_id.get(),
        panel.get(),
        Some(InflightTurnIdentity::from_state(&state)),
    );
    assert_eq!(
        crate::services::discord::status_panel_orphan_store::load_pending(
            &provider,
            &shared.token_hash,
        ),
        vec![(channel_id.get(), panel.get())]
    );

    let mut last_status_panel_text = String::new();
    let committed = complete_status_panel_v2(
        shared.as_ref(),
        &gateway,
        channel_id,
        Some(panel),
        &provider,
        1_700_000_000,
        &mut last_status_panel_text,
        false,
        false,
        "test_pending_bind_completion_purge",
        user_msg_id,
    )
    .await;

    assert!(committed);
    assert_eq!(
        gateway
            .edited_message_ids
            .lock()
            .expect("edited ids lock")
            .as_slice(),
        &[panel]
    );
    assert!(
        crate::services::discord::status_panel_orphan_store::load_pending(
            &provider,
            &shared.token_hash,
        )
        .is_empty(),
        "completion success must purge a crash-window pending_bind for the final live panel"
    );
}

fn inflight_row_owned_by(
    provider: &ProviderKind,
    channel_id: u64,
    user_msg_id: u64,
    status_panel_msg_id: u64,
) -> InflightTurnState {
    let mut state = InflightTurnState::new(
        provider.clone(),
        channel_id,
        Some("alias-epilogue-test".to_string()),
        42,
        user_msg_id,
        user_msg_id + 1,
        "turn".to_string(),
        None,
        None,
        None,
        None,
        0,
    );
    state.status_message_id = Some(status_panel_msg_id);
    state
}

// #3161 (codex P1): the production skip-branch -> epilogue-cleanup
// interaction. An OLD bridge turn whose status-panel completion EDIT is
// correctly alias-skipped (a NEWER turn re-adopted its panel between turn
// start and completion) MUST NOT remove the NEWER owner's on-disk inflight
// row in its epilogue. Before the identity guard the removal at the
// `clear_inflight_state` site was unconditional, so the OLD turn deleted the
// NEWER owner's row -> the newer turn's status panel was left permanently
// non-complete.
//
// RED->GREEN: this test drives the REAL on-disk inflight layer and the REAL
// production decision seam (`bridge_epilogue_identity_guards_inflight_clear`
// + `clear_inflight_state_if_matches`). Without the guard the epilogue would
// run the unconditional `clear_inflight_state` (asserted as the regression
// vector below) and the newer owner's row would be gone -> the final
// `load_inflight_state` assertion fails.
#[test]
fn alias_skipped_old_turn_does_not_remove_newer_owners_inflight_row() {
    let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
    let tmp = tempfile::tempdir().unwrap();
    struct EnvGuard;
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") };
        }
    }
    unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", tmp.path().to_str().unwrap()) };
    let _env_guard = EnvGuard;

    let provider = ProviderKind::Claude;
    let channel_id = 3_161_900u64;
    let panel = MessageId::new(1_510_319_194_921_504_931);
    let old_turn_user_msg_id = 7_000_001u64;
    let newer_turn_user_msg_id = 7_000_999u64;

    // A NEWER follow-up turn now owns the on-disk row AND has re-adopted the
    // SAME status panel the OLD turn captured at its start.
    save_inflight_state(&inflight_row_owned_by(
        &provider,
        channel_id,
        newer_turn_user_msg_id,
        panel.get(),
    ))
    .unwrap();

    // Production step 1: the OLD turn re-reads the current on-disk row at
    // completion and decides whether to EDIT the panel. The newer owner of
    // THIS panel must alias-skip the edit.
    let on_disk = load_inflight_state(&provider, channel_id).expect("newer row on disk");
    assert!(
        status_panel_completion_edit_aliases_newer_turn(
            old_turn_user_msg_id,
            Some(panel),
            on_disk.user_msg_id,
            on_disk.status_message_id,
        ),
        "precondition: the OLD turn's panel edit must be alias-skipped"
    );

    // Sanity / regression-vector: the OLD pre-fix behavior (unconditional
    // clear) WOULD have removed the newer owner's row. We assert the guard
    // routes AWAY from that path for a real this-turn identity.
    assert!(
        bridge_epilogue_identity_guards_inflight_clear(old_turn_user_msg_id),
        "a real (non-zero) this-turn identity must be identity-guarded in the epilogue"
    );

    // Production step 2: the OLD turn's epilogue cleanup. This mirrors the
    // exact production fork at the `clear_inflight_state` site.
    if bridge_epilogue_identity_guards_inflight_clear(old_turn_user_msg_id) {
        let outcome = clear_inflight_state_if_matches(&provider, channel_id, old_turn_user_msg_id);
        assert_eq!(
            outcome,
            GuardedClearOutcome::UserMsgMismatch,
            "the OLD turn must NOT clear a row that now belongs to the newer turn"
        );
    } else {
        clear_inflight_state(&provider, channel_id);
    }

    // The newer owner's row must survive the OLD turn's epilogue. (Pre-fix
    // unconditional clear deleted it here -> RED.)
    let survived = load_inflight_state(&provider, channel_id)
        .expect("newer owner's inflight row must survive the OLD turn's epilogue");
    assert_eq!(
        survived.user_msg_id, newer_turn_user_msg_id,
        "the surviving row must still belong to the newer turn"
    );

    // And the NEWER turn can still complete normally: its own epilogue (same
    // turn owns the row) clears it.
    let cleared = clear_inflight_state_if_matches(&provider, channel_id, newer_turn_user_msg_id);
    assert_eq!(
        cleared,
        GuardedClearOutcome::Cleared,
        "the newer turn must still be able to clear its own row at completion"
    );
    assert!(
        load_inflight_state(&provider, channel_id).is_none(),
        "the row is gone once the newer (owning) turn completes"
    );
}

// #3161 (codex P1, id==0 carve-out): the zero-id epilogue race. An OLD
// zero-id turn (recovery / external-input / cluster-relay synthesized;
// `user_msg_id == 0`) finalizes AFTER a NEWER real (non-zero) identity turn
// wrote its inflight row. The pre-fix carve-out ran the UNCONDITIONAL
// `clear_inflight_state`, blind-deleting the newer owner's row -> the newer
// turn's status panel was left permanently non-complete (the same bug, now
// for zero-id callers).
//
// RED->GREEN: this drives the REAL on-disk inflight layer and mirrors the
// exact production fork at the zero-id epilogue site
// (`bridge_epilogue_identity_guards_inflight_clear(0) == false` ->
// `clear_inflight_state_if_matches_zero_owned`). With the old unconditional
// `clear_inflight_state(...)` the final `load_inflight_state` assertion
// would fail because the newer owner's row would be gone.
#[test]
fn zero_id_old_turn_does_not_remove_newer_owners_inflight_row() {
    let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
    let tmp = tempfile::tempdir().unwrap();
    struct EnvGuard;
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") };
        }
    }
    unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", tmp.path().to_str().unwrap()) };
    let _env_guard = EnvGuard;

    let provider = ProviderKind::Claude;
    let channel_id = 3_161_950u64;
    let panel = MessageId::new(1_510_319_194_921_504_999);
    let newer_turn_user_msg_id = 7_001_999u64;

    // A NEWER real (non-zero) follow-up turn now owns the on-disk row.
    save_inflight_state(&inflight_row_owned_by(
        &provider,
        channel_id,
        newer_turn_user_msg_id,
        panel.get(),
    ))
    .unwrap();

    // The OLD turn is zero-id -> the epilogue takes the id==0 carve-out
    // branch (NOT the non-zero identity-guard branch).
    let old_turn_user_msg_id = 0u64;
    assert!(
        !bridge_epilogue_identity_guards_inflight_clear(old_turn_user_msg_id),
        "a zero-id this-turn must NOT take the non-zero identity-guard branch"
    );

    // Production step: the OLD zero-id turn's epilogue cleanup. This mirrors
    // the exact production fork's `else` arm.
    let outcome = clear_inflight_state_if_matches_zero_owned(&provider, channel_id);
    assert_eq!(
        outcome,
        GuardedClearOutcome::UserMsgMismatch,
        "the zero-id turn must NOT clear a row that now belongs to a newer non-zero turn"
    );

    // The newer owner's row must survive the OLD zero-id turn's epilogue.
    // (Pre-fix unconditional clear deleted it here -> RED.)
    let survived = load_inflight_state(&provider, channel_id)
        .expect("newer owner's inflight row must survive the OLD zero-id turn's epilogue");
    assert_eq!(
        survived.user_msg_id, newer_turn_user_msg_id,
        "the surviving row must still belong to the newer turn"
    );
}

// #3161 (codex P1, no-recovery-regression): a zero-id turn must STILL clear
// its OWN zero-id row. The on-disk `user_msg_id` is 0 (a genuine
// zero-id-owned recovery/external-input row), so the zero-owned guarded
// clear removes it. This is the regression guard that the P1 fix did not
// over-correct into refusing all zero-id cleanup.
#[test]
fn zero_id_turn_still_clears_its_own_zero_id_row() {
    let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
    let tmp = tempfile::tempdir().unwrap();
    struct EnvGuard;
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") };
        }
    }
    unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", tmp.path().to_str().unwrap()) };
    let _env_guard = EnvGuard;

    let provider = ProviderKind::Claude;
    let channel_id = 3_161_970u64;

    // A genuine zero-id-owned row (recovery/external-input turn): on-disk
    // `user_msg_id == 0`.
    save_inflight_state(&inflight_row_owned_by(&provider, channel_id, 0, 0)).unwrap();

    let outcome = clear_inflight_state_if_matches_zero_owned(&provider, channel_id);
    assert_eq!(
        outcome,
        GuardedClearOutcome::Cleared,
        "a zero-id turn must still clear its OWN zero-id row (recovery cleanup)"
    );
    assert!(
        load_inflight_state(&provider, channel_id).is_none(),
        "the zero-id-owned row is removed by its own zero-id turn"
    );
}

// #3161 (codex P2): the `InflightCleanupGuard::Drop` is identity-aware. On
// an abnormal exit the Drop must only clear THIS turn's row. We assert the
// exact routing the Drop performs: a non-zero this-turn id routes through
// the identity-guarded clear (preserving a newer owner), while a zero-id
// this-turn routes through the zero-owned clear (preserving a newer
// non-zero owner). The Drop body itself is a thin dispatch over these two
// production helpers, so exercising them with the same inputs proves the
// Drop's identity-awareness without spawning the full bridge task.
#[test]
fn cleanup_guard_drop_routing_is_identity_aware() {
    let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
    let tmp = tempfile::tempdir().unwrap();
    struct EnvGuard;
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") };
        }
    }
    unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", tmp.path().to_str().unwrap()) };
    let _env_guard = EnvGuard;

    let provider = ProviderKind::Claude;

    // Case A: a non-zero guard whose abnormal drop fires AFTER a newer owner
    // re-wrote the row must NOT delete the newer owner's row.
    let channel_a = 3_161_980u64;
    let newer = 7_002_500u64;
    save_inflight_state(&inflight_row_owned_by(&provider, channel_a, newer, 111)).unwrap();
    // The drop carries the OLD turn's (different) non-zero id.
    let old_non_zero = 7_002_111u64;
    let outcome_a = clear_inflight_state_if_matches(&provider, channel_a, old_non_zero);
    assert_eq!(
        outcome_a,
        GuardedClearOutcome::UserMsgMismatch,
        "abnormal-path drop for a non-zero turn must not clear a newer owner's row"
    );
    assert_eq!(
        load_inflight_state(&provider, channel_a)
            .expect("newer owner survives")
            .user_msg_id,
        newer
    );

    // Case B: a zero-id guard whose abnormal drop fires AFTER a newer
    // non-zero owner re-wrote the row must NOT delete it either.
    let channel_b = 3_161_990u64;
    save_inflight_state(&inflight_row_owned_by(&provider, channel_b, newer, 222)).unwrap();
    let outcome_b = clear_inflight_state_if_matches_zero_owned(&provider, channel_b);
    assert_eq!(
        outcome_b,
        GuardedClearOutcome::UserMsgMismatch,
        "abnormal-path drop for a zero-id turn must not clear a newer non-zero owner's row"
    );
    assert_eq!(
        load_inflight_state(&provider, channel_b)
            .expect("newer owner survives")
            .user_msg_id,
        newer
    );

    // Case C: a guard that genuinely owns its row (matching non-zero id)
    // still cleans up on its abnormal drop.
    let channel_c = 3_161_995u64;
    let owner = 7_003_000u64;
    save_inflight_state(&inflight_row_owned_by(&provider, channel_c, owner, 333)).unwrap();
    let outcome_c = clear_inflight_state_if_matches(&provider, channel_c, owner);
    assert_eq!(
        outcome_c,
        GuardedClearOutcome::Cleared,
        "abnormal-path drop must still clean up the turn's OWN row"
    );
    assert!(load_inflight_state(&provider, channel_c).is_none());
}
