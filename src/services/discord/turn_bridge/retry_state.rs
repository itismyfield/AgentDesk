/// Retry-state management for turn_bridge.
///
/// Provides helpers to clear, reset, and manage the in-flight retry state
/// during Gemini/Qwen auto-retry boundaries and session recovery.
use super::super::*;
use super::recovery_text::release_retry_pending;
#[cfg(unix)]
use crate::services::tmux_diagnostics::record_tmux_exit_reason;

/// #2452 H6 graduation: schedule the history-aware auto-retry via the
/// gateway's `_with_completion` variant, then release the
/// `RETRY_PENDING` dedup lockout AS SOON AS the gateway's completion
/// oneshot resolves. A 120s `tokio::time::timeout` safety net guarantees
/// the lockout cannot leak indefinitely even if the spawned scheduler
/// panics or wedges before sending on `completion_tx`.
///
/// The legacy 30s sleep inside `auto_retry_with_history` is preserved as
/// a back-compat fallback for callers that hit the trait's default
/// `_with_completion` impl (which sends on `completion_tx` immediately
/// after the inner `auto_retry_with_history` returns) — both paths
/// remove the same `channel_id` from the `RETRY_PENDING` set, so a
/// double-remove is a no-op.
pub(in crate::services::discord) fn spawn_retry_with_history_with_release(
    gateway: std::sync::Arc<dyn gateway::TurnGateway>,
    channel_id: ChannelId,
    user_msg_id: MessageId,
    retry_text: String,
) {
    let (completion_tx, completion_rx) = tokio::sync::oneshot::channel::<()>();
    super::super::task_supervisor::spawn_observed("retry_with_history_dispatch", async move {
        gateway
            .schedule_retry_with_history_with_completion(
                channel_id,
                user_msg_id,
                &retry_text,
                completion_tx,
            )
            .await;
    });
    super::super::task_supervisor::spawn_observed("retry_with_history_release", async move {
        // 120s safety net: if completion_tx is dropped without a send
        // (panic, wedged future), the recv resolves with Err and we still
        // release. If 120s elapses with neither send nor drop, force
        // release so the lockout cannot leak forever.
        let _ = tokio::time::timeout(std::time::Duration::from_secs(120), completion_rx).await;
        release_retry_pending(channel_id);
    });
}

pub(super) fn clear_local_session_state(
    new_session_id: &mut Option<String>,
    new_raw_provider_session_id: &mut Option<String>,
    inflight_state: &mut InflightTurnState,
) {
    *new_session_id = None;
    *new_raw_provider_session_id = None;
    inflight_state.session_id = None;
}

pub(super) fn should_reset_gemini_retry_attempt_state(
    full_response: &str,
    current_tool_line: Option<&str>,
    any_tool_used: bool,
    has_post_tool_text: bool,
) -> bool {
    !full_response.trim().is_empty()
        || current_tool_line.is_some()
        || any_tool_used
        || has_post_tool_text
}

fn normalized_response_sent_offset(full_response: &str, response_sent_offset: usize) -> usize {
    let mut offset = response_sent_offset.min(full_response.len());
    while offset > 0 && !full_response.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

pub(super) fn sync_response_delivery_state(
    full_response: &str,
    response_sent_offset: &mut usize,
    inflight_state: &mut InflightTurnState,
) {
    *response_sent_offset = normalized_response_sent_offset(full_response, *response_sent_offset);
    inflight_state.full_response = full_response.to_string();
    inflight_state.response_sent_offset = *response_sent_offset;
}

pub(super) fn sync_terminal_error_delivery_state(
    full_response: &str,
    response_sent_offset: &mut usize,
    inflight_state: &mut InflightTurnState,
) {
    *response_sent_offset = 0;
    sync_response_delivery_state(full_response, response_sent_offset, inflight_state);
}

pub(super) fn bridge_confirmed_response_sent_offset_seed(
    owner: super::super::inflight::RelayOwnerKind,
    response_sent_offset: usize,
) -> usize {
    match owner {
        super::super::inflight::RelayOwnerKind::None => response_sent_offset,
        super::super::inflight::RelayOwnerKind::Watcher
        | super::super::inflight::RelayOwnerKind::StandbyRelay
        | super::super::inflight::RelayOwnerKind::SessionBoundRelay
        | super::super::inflight::RelayOwnerKind::Unknown => 0,
    }
}

pub(super) fn bridge_should_reclaim_relay_from_missing_watcher(
    watcher_owns_assistant_relay: bool,
    standby_relay_owns_output: bool,
    live_watcher_registered: bool,
) -> bool {
    watcher_owns_assistant_relay && !standby_relay_owns_output && !live_watcher_registered
}

fn refresh_delivery_rewind_state(inflight_state: &mut InflightTurnState) -> bool {
    let Some(provider) = inflight_state.provider_kind() else {
        return false;
    };
    let expected_identity =
        super::super::inflight::InflightTurnIdentity::from_state(inflight_state);
    let Some(reloaded) =
        super::super::inflight::load_inflight_state(&provider, inflight_state.channel_id)
    else {
        return false;
    };
    if !expected_identity.matches_state(&reloaded) {
        return false;
    }
    *inflight_state = reloaded;
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DeliveryRewindPersistOutcome {
    Saved,
    Rejected,
    PersistError,
}

impl DeliveryRewindPersistOutcome {
    fn saved(self) -> bool {
        matches!(self, Self::Saved)
    }
}

fn persist_delivery_rewind(
    inflight_state: &mut InflightTurnState,
    reason: super::super::inflight::InflightDeliveryRewindReason,
    channel_id: ChannelId,
) -> DeliveryRewindPersistOutcome {
    match super::super::inflight::save_inflight_delivery_rewind_if_matches_identity(
        inflight_state,
        reason,
    ) {
        Ok(saved) => {
            if saved {
                refresh_delivery_rewind_state(inflight_state);
                DeliveryRewindPersistOutcome::Saved
            } else {
                DeliveryRewindPersistOutcome::Rejected
            }
        }
        Err(error) => {
            tracing::warn!(
                channel_id = channel_id.get(),
                reason = reason.as_str(),
                error = %error,
                "turn_bridge failed to persist legitimate delivery rewind; preserving local rewind state"
            );
            DeliveryRewindPersistOutcome::PersistError
        }
    }
}

pub(super) fn persist_terminal_error_delivery_rewind(
    inflight_state: &mut InflightTurnState,
    channel_id: ChannelId,
) -> DeliveryRewindPersistOutcome {
    persist_delivery_rewind(
        inflight_state,
        super::super::inflight::InflightDeliveryRewindReason::TerminalErrorReset,
        channel_id,
    )
}

pub(super) fn persist_reclaim_delivery_rewind(
    inflight_state: &mut InflightTurnState,
    channel_id: ChannelId,
) -> DeliveryRewindPersistOutcome {
    persist_delivery_rewind(
        inflight_state,
        super::super::inflight::InflightDeliveryRewindReason::MissingWatcherReclaim,
        channel_id,
    )
}

pub(super) fn sync_terminal_error_delivery_state_for_bridge_owner(
    full_response: &str,
    response_sent_offset: &mut usize,
    bridge_confirmed_response_sent_offset: &mut usize,
    inflight_state: &mut InflightTurnState,
    channel_id: ChannelId,
    watcher_relay_owns_output: bool,
) -> bool {
    if watcher_relay_owns_output {
        inflight_state.set_relay_owner_kind(super::super::inflight::RelayOwnerKind::None);
    }
    sync_terminal_error_delivery_state(full_response, response_sent_offset, inflight_state);
    let persisted = persist_terminal_error_delivery_rewind(inflight_state, channel_id);
    if matches!(persisted, DeliveryRewindPersistOutcome::Rejected) {
        refresh_delivery_rewind_state(inflight_state);
    }
    *response_sent_offset = inflight_state.response_sent_offset;
    *bridge_confirmed_response_sent_offset = *response_sent_offset;
    persisted.saved()
}

pub(super) fn rewind_delivery_on_reclaim(
    full_response: &str,
    bridge_confirmed_response_sent_offset: usize,
    response_sent_offset: &mut usize,
    inflight_state: &mut InflightTurnState,
    channel_id: ChannelId,
) -> bool {
    if *response_sent_offset <= bridge_confirmed_response_sent_offset {
        return false;
    }
    *response_sent_offset = bridge_confirmed_response_sent_offset;
    sync_response_delivery_state(full_response, response_sent_offset, inflight_state);
    tracing::warn!(
        channel_id = channel_id.get(),
        response_sent_offset,
        "turn_bridge rewound response_sent_offset after reclaiming missing watcher"
    );
    true
}

pub(super) fn rewind_and_persist_delivery_on_reclaim(
    full_response: &str,
    bridge_confirmed_response_sent_offset: usize,
    response_sent_offset: &mut usize,
    inflight_state: &mut InflightTurnState,
    channel_id: ChannelId,
) -> bool {
    let pre_reclaim_state = inflight_state.clone();
    let pre_reclaim_response_sent_offset = *response_sent_offset;
    if !rewind_delivery_on_reclaim(
        full_response,
        bridge_confirmed_response_sent_offset,
        response_sent_offset,
        inflight_state,
        channel_id,
    ) {
        return false;
    }
    match persist_reclaim_delivery_rewind(inflight_state, channel_id) {
        DeliveryRewindPersistOutcome::Saved => {
            *response_sent_offset = inflight_state.response_sent_offset;
            return true;
        }
        DeliveryRewindPersistOutcome::Rejected => {
            if refresh_delivery_rewind_state(inflight_state) {
                *response_sent_offset = inflight_state.response_sent_offset;
            } else {
                *inflight_state = pre_reclaim_state;
                *response_sent_offset = pre_reclaim_response_sent_offset;
            }
        }
        DeliveryRewindPersistOutcome::PersistError => {}
    }
    false
}

pub(super) fn clear_response_delivery_state(
    full_response: &mut String,
    response_sent_offset: &mut usize,
    inflight_state: &mut InflightTurnState,
) {
    full_response.clear();
    *response_sent_offset = 0;
    inflight_state.full_response.clear();
    inflight_state.response_sent_offset = 0;
}

pub(super) fn reset_gemini_retry_attempt_state(
    full_response: &mut String,
    current_tool_line: &mut Option<String>,
    prev_tool_status: &mut Option<String>,
    last_tool_name: &mut Option<String>,
    last_tool_summary: &mut Option<String>,
    any_tool_used: &mut bool,
    has_post_tool_text: &mut bool,
    response_sent_offset: &mut usize,
    inflight_state: &mut InflightTurnState,
) {
    clear_response_delivery_state(full_response, response_sent_offset, inflight_state);
    *current_tool_line = None;
    *prev_tool_status = None;
    *last_tool_name = None;
    *last_tool_summary = None;
    *any_tool_used = false;
    *has_post_tool_text = false;
    inflight_state.current_tool_line = None;
    inflight_state.prev_tool_status = None;
    inflight_state.any_tool_used = false;
    inflight_state.has_post_tool_text = false;
}

pub(super) fn handle_gemini_retry_boundary(
    full_response: &mut String,
    current_tool_line: &mut Option<String>,
    prev_tool_status: &mut Option<String>,
    last_tool_name: &mut Option<String>,
    last_tool_summary: &mut Option<String>,
    any_tool_used: &mut bool,
    has_post_tool_text: &mut bool,
    response_sent_offset: &mut usize,
    last_edit_text: &mut String,
    new_session_id: &mut Option<String>,
    new_raw_provider_session_id: &mut Option<String>,
    inflight_state: &mut InflightTurnState,
) -> bool {
    let had_local_session = new_session_id.is_some() || inflight_state.session_id.is_some();
    let should_reset = should_reset_gemini_retry_attempt_state(
        full_response,
        current_tool_line.as_deref(),
        *any_tool_used,
        *has_post_tool_text,
    );

    if had_local_session {
        clear_local_session_state(new_session_id, new_raw_provider_session_id, inflight_state);
    }

    if should_reset {
        reset_gemini_retry_attempt_state(
            full_response,
            current_tool_line,
            prev_tool_status,
            last_tool_name,
            last_tool_summary,
            any_tool_used,
            has_post_tool_text,
            response_sent_offset,
            inflight_state,
        );
        last_edit_text.clear();
    }

    had_local_session || should_reset
}

pub(super) async fn reset_session_for_auto_retry(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    cancel_token: &Arc<CancelToken>,
    adk_session_key: Option<&str>,
    new_session_id: &mut Option<String>,
    new_raw_provider_session_id: &mut Option<String>,
    inflight_state: &mut InflightTurnState,
    reason: &str,
) {
    clear_local_session_state(new_session_id, new_raw_provider_session_id, inflight_state);
    let _ = crate::services::discord::inflight::save_inflight_state_if_identity_unchanged(
        inflight_state,
        "turn_bridge::retry_state::reset_session_for_auto_retry",
    );

    let stale_sid = {
        let mut data = shared.core.lock().await;
        let old = data
            .sessions
            .get(&channel_id)
            .and_then(|s| s.session_id.clone());
        if let Some(session) = data.sessions.get_mut(&channel_id) {
            session.clear_provider_session();
        }
        old
    };

    if let Some(key) = adk_session_key {
        super::super::adk_session::clear_provider_session_id(key, shared.api_port).await;
    }

    if let Some(ref sid) = stale_sid {
        let _ = super::super::internal_api::clear_stale_session_id(sid).await;
    }

    #[cfg(unix)]
    if let Some(name) = cancel_token.tmux_session_name() {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!(
            "  [{ts}] ♻ auto-retry: killing tmux session {name} before retry ({reason})"
        );
        crate::services::termination_audit::record_termination_for_tmux(
            &name,
            None,
            "turn_bridge",
            "auto_retry_fresh_session",
            Some(&format!(
                "forcing fresh session before auto-retry: {reason}"
            )),
            None,
        );
        record_tmux_exit_reason(
            &name,
            &format!("forcing fresh session before auto-retry: {reason}"),
        );
        crate::services::platform::tmux::kill_session(
            &name,
            &format!("forcing fresh session before auto-retry: {reason}"),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EnvReset(Option<std::ffi::OsString>);

    impl Drop for EnvReset {
        fn drop(&mut self) {
            match self.0.take() {
                Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
                None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
            }
        }
    }

    // #4091 r6: env-var mutation only. Every caller ALREADY holds
    // `shared_test_env_lock()` (bound as `_env_lock`) before calling this, so
    // acquiring the lock here would self-deadlock the non-reentrant
    // std::sync::Mutex. Serialization is the caller's `_env_lock` guard.
    fn set_runtime_root(path: &std::path::Path) -> EnvReset {
        let previous = std::env::var_os("AGENTDESK_ROOT_DIR");
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", path) };
        EnvReset(previous)
    }

    fn inflight(full_response: &str, response_sent_offset: usize) -> InflightTurnState {
        let mut state = InflightTurnState::new(
            ProviderKind::Claude,
            1,
            Some("adk-cc".to_string()),
            42,
            5001,
            5002,
            "prompt".to_string(),
            Some("session".to_string()),
            Some("AgentDesk-claude-adk-cc-1".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            None,
            10,
        );
        state.full_response = full_response.to_string();
        state.response_sent_offset = response_sent_offset;
        state
    }

    #[test]
    fn bridge_confirmed_seed_trusts_only_bridge_owned_offsets() {
        use super::super::super::inflight::RelayOwnerKind;

        assert_eq!(
            bridge_confirmed_response_sent_offset_seed(RelayOwnerKind::None, 17),
            17
        );
        for owner in [
            RelayOwnerKind::Watcher,
            RelayOwnerKind::StandbyRelay,
            RelayOwnerKind::SessionBoundRelay,
            RelayOwnerKind::Unknown,
        ] {
            assert_eq!(
                bridge_confirmed_response_sent_offset_seed(owner, 17),
                0,
                "persisted suppress offsets from {owner:?} are not bridge-confirmed delivery"
            );
        }
    }

    #[test]
    fn reclaim_rewinds_watcher_suppression_offset_to_bridge_confirmed_point() {
        let full_response = "visible prefix\nhidden tail";
        let bridge_confirmed = "visible prefix\n".len();
        let mut response_sent_offset = full_response.len();
        let mut state = inflight(full_response, response_sent_offset);

        rewind_delivery_on_reclaim(
            full_response,
            bridge_confirmed,
            &mut response_sent_offset,
            &mut state,
            ChannelId::new(1),
        );

        assert_eq!(response_sent_offset, bridge_confirmed);
        assert_eq!(&full_response[response_sent_offset..], "hidden tail");
        assert_eq!(state.response_sent_offset, bridge_confirmed);
    }

    #[test]
    fn retry_state_authority_on_reclaim_rewind_persists() {
        use crate::services::discord::outbound::delivery_record as dr;

        let _env_lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let temp = tempfile::TempDir::new().expect("temp root");
        let _root = set_runtime_root(temp.path());
        let _authority = dr::authority_test_seam::force(true);

        let full_response = "visible prefix\nhidden tail";
        let channel = ChannelId::new(41_100_001);
        let mut state = inflight(full_response, full_response.len());
        state.channel_id = channel.get();
        state.status_message_id = Some(41_100_091);
        super::super::super::inflight::save_inflight_state(&state).expect("seed inflight");
        state.status_message_id = None;

        let mut response_sent_offset = full_response.len();
        let bridge_confirmed = bridge_confirmed_response_sent_offset_seed(
            super::super::super::inflight::RelayOwnerKind::Watcher,
            response_sent_offset,
        );
        assert!(rewind_and_persist_delivery_on_reclaim(
            full_response,
            bridge_confirmed,
            &mut response_sent_offset,
            &mut state,
            channel,
        ));
        assert_eq!(
            state.status_message_id,
            Some(41_100_091),
            "RMW result must refresh in-memory state before any later full save"
        );
        super::super::super::inflight::save_inflight_state(&state)
            .expect("later full save must be harmless");

        let persisted = super::super::super::inflight::load_inflight_state(
            &ProviderKind::Claude,
            channel.get(),
        )
        .expect("persisted rewind");
        assert_eq!(persisted.response_sent_offset, 0);
        assert_eq!(persisted.full_response, full_response);
        assert_eq!(
            persisted.status_message_id,
            Some(41_100_091),
            "delivery rewind must not overwrite unrelated same-turn durable fields"
        );
    }

    #[test]
    fn retry_state_reclaim_rewind_refuses_watcher_committed_row() {
        let _env_lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let temp = tempfile::TempDir::new().expect("temp root");
        let _root = set_runtime_root(temp.path());

        let full_response = "watcher already delivered this body";
        let channel = ChannelId::new(41_100_004);
        let mut committed = inflight(full_response, full_response.len());
        committed.channel_id = channel.get();
        committed.terminal_delivery_committed = true;
        committed.set_relay_owner_kind(super::super::super::inflight::RelayOwnerKind::Watcher);
        super::super::super::inflight::save_inflight_state(&committed)
            .expect("seed watcher committed row");

        let mut stale_reclaim = committed.clone();
        stale_reclaim.response_sent_offset = 0;
        stale_reclaim.terminal_delivery_committed = false;
        stale_reclaim.set_relay_owner_kind(super::super::super::inflight::RelayOwnerKind::None);

        assert_eq!(
            persist_reclaim_delivery_rewind(&mut stale_reclaim, channel),
            DeliveryRewindPersistOutcome::Rejected,
            "reclaim RMW must not reopen an already committed watcher row"
        );

        let persisted = super::super::super::inflight::load_inflight_state(
            &ProviderKind::Claude,
            channel.get(),
        )
        .expect("persisted committed row");
        assert!(persisted.terminal_delivery_committed);
        assert_eq!(persisted.response_sent_offset, full_response.len());
        assert_eq!(persisted.full_response, full_response);
    }

    #[test]
    fn refresh_delivery_rewind_state_refuses_replacement_turn_row() {
        let _env_lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let temp = tempfile::TempDir::new().expect("temp root");
        let _root = set_runtime_root(temp.path());

        let channel = ChannelId::new(41_100_005);
        let mut original = inflight("old turn error text", 0);
        original.channel_id = channel.get();
        original.user_msg_id = 51_001;
        original.turn_start_offset = Some(100);
        super::super::super::inflight::save_inflight_state(&original).expect("seed original row");

        let mut replacement = inflight("new turn body already delivered", 29);
        replacement.channel_id = channel.get();
        replacement.user_msg_id = 51_002;
        replacement.started_at = format!("{}-replacement", original.started_at);
        replacement.turn_start_offset = Some(200);
        super::super::super::inflight::save_inflight_state(&replacement)
            .expect("replace with newer row");

        let original_identity =
            super::super::super::inflight::InflightTurnIdentity::from_state(&original);

        assert!(
            !refresh_delivery_rewind_state(&mut original),
            "reload must fail closed when the on-disk row now belongs to another turn"
        );
        assert!(
            original_identity.matches_state(&original),
            "bridge state must keep the pre-refresh turn identity"
        );
        assert_eq!(original.full_response, "old turn error text");
        assert_eq!(original.response_sent_offset, 0);
    }

    #[test]
    fn retry_state_terminal_error_reset_persists_without_debug_panic() {
        use crate::services::discord::outbound::delivery_record as dr;

        let _env_lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let temp = tempfile::TempDir::new().expect("temp root");
        let _root = set_runtime_root(temp.path());
        let _authority = dr::authority_test_seam::force(false);

        let channel = ChannelId::new(41_100_002);
        let mut state = inflight("streamed answer body", "streamed answer body".len());
        state.channel_id = channel.get();
        super::super::super::inflight::save_inflight_state(&state).expect("seed inflight");

        let error_response = "Error: provider transport failed";
        let mut response_sent_offset = state.response_sent_offset;
        sync_terminal_error_delivery_state(error_response, &mut response_sent_offset, &mut state);

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            persist_terminal_error_delivery_rewind(&mut state, channel)
        }));
        assert_eq!(
            result.expect("reasoned rewind must not trip debug assert"),
            DeliveryRewindPersistOutcome::Saved
        );

        let persisted = super::super::super::inflight::load_inflight_state(
            &ProviderKind::Claude,
            channel.get(),
        )
        .expect("persisted error reset");
        assert_eq!(persisted.response_sent_offset, 0);
        assert_eq!(persisted.full_response, error_response);
    }

    #[test]
    fn retry_state_rmw_error_preserves_local_terminal_error_offset() {
        let _env_lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let temp = tempfile::TempDir::new().expect("temp root");
        let _root = set_runtime_root(temp.path());

        let channel = ChannelId::new(41_100_006);
        let full_response = "streamed answer body";
        let mut state = inflight(full_response, full_response.len());
        state.channel_id = channel.get();
        super::super::super::inflight::save_inflight_state(&state).expect("seed inflight");

        let lock_path = temp
            .path()
            .join("runtime")
            .join("discord_inflight")
            .join(ProviderKind::Claude.as_str())
            .join(format!("{}.json.lock", channel.get()));
        // The seed save above already created the lock file; replace it with a
        // directory so the RMW lock open fails with a real IO error.
        let _ = std::fs::remove_file(&lock_path);
        std::fs::create_dir(&lock_path).expect("turn lock path into directory");

        let error_response = "Error: provider transport failed";
        let mut response_sent_offset = state.response_sent_offset;
        let mut bridge_confirmed = response_sent_offset;

        assert!(
            !sync_terminal_error_delivery_state_for_bridge_owner(
                error_response,
                &mut response_sent_offset,
                &mut bridge_confirmed,
                &mut state,
                channel,
                false,
            ),
            "RMW IO/flock errors are not persisted successes"
        );

        assert_eq!(
            response_sent_offset, 0,
            "local delivery frontier must keep the error text deliverable"
        );
        assert_eq!(bridge_confirmed, 0);
        assert_eq!(state.response_sent_offset, 0);
        assert_eq!(state.full_response, error_response);

        let persisted = super::super::super::inflight::load_inflight_state(
            &ProviderKind::Claude,
            channel.get(),
        )
        .expect("original persisted row remains readable");
        assert_eq!(persisted.response_sent_offset, full_response.len());
        assert_eq!(persisted.full_response, full_response);
    }

    #[test]
    fn watcher_relay_error_frame_reclaims_bridge_delivery_frontier() {
        use crate::services::discord::outbound::delivery_record as dr;

        let _env_lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let temp = tempfile::TempDir::new().expect("temp root");
        let _root = set_runtime_root(temp.path());
        let _authority = dr::authority_test_seam::force(true);

        let full_response = "w".repeat(500);
        let mut response_sent_offset = full_response.len();
        let mut bridge_confirmed = 0;
        let channel = ChannelId::new(41_100_003);
        let mut state = inflight(&full_response, response_sent_offset);
        state.channel_id = channel.get();
        state.set_relay_owner_kind(super::super::super::inflight::RelayOwnerKind::Watcher);
        super::super::super::inflight::save_inflight_state(&state)
            .expect("seed watcher-owned inflight");
        let error_response = "Error: provider failed";

        assert!(sync_terminal_error_delivery_state_for_bridge_owner(
            error_response,
            &mut response_sent_offset,
            &mut bridge_confirmed,
            &mut state,
            channel,
            true,
        ));
        assert_eq!(response_sent_offset, 0);
        assert_eq!(bridge_confirmed, 0);
        assert_eq!(state.response_sent_offset, 0);
        assert_eq!(state.full_response, error_response);
        assert_eq!(
            state.effective_relay_owner_kind(),
            super::super::super::inflight::RelayOwnerKind::None
        );
        let delivered = super::super::response_delivery::terminal_delivery_response_after_offset(
            error_response,
            response_sent_offset,
            None,
        );
        assert_eq!(delivered, error_response);

        let persisted = super::super::super::inflight::load_inflight_state(
            &ProviderKind::Claude,
            channel.get(),
        )
        .expect("persisted terminal error reset");
        assert_eq!(persisted.response_sent_offset, 0);
        assert_eq!(persisted.full_response, error_response);
        assert_eq!(
            persisted.effective_relay_owner_kind(),
            super::super::super::inflight::RelayOwnerKind::None
        );
    }
}

#[cfg(test)]
mod sentinel_overwrite_clamp_tests {
    use super::super::response_delivery::done_result_requires_full_terminal_replay;

    // #3419 R3: the existing offset-reset gate requires a >DISCORD_MSG_LIMIT
    // (8000-byte) authoritative replay, so a short sentinel NEVER trips it —
    // which is exactly why the SEPARATE clamp at the swap site is needed. This
    // pins that gap so a future refactor cannot silently make the clamp dead
    // (e.g. by assuming the replay gate already handles sentinels).
    #[test]
    fn replay_gate_does_not_reset_offset_for_short_sentinel() {
        let sentinel = "⚠ tool-only turn, no assistant text"; // ~40 bytes, < 8000
        assert!(sentinel.len() <= super::super::super::DISCORD_MSG_LIMIT);
        // streamed text this turn, prior offset > 0, sentinel == full_response:
        // the replay gate STILL returns false because of the length floor.
        assert!(!done_result_requires_full_terminal_replay(
            sentinel, sentinel, 900, true,
        ));
    }

    // #3419 R3 / codex MEDIUM: drive the REAL normalizer the bridge now calls
    // (`sync_response_delivery_state` → `normalized_response_sent_offset`), not a
    // re-implemented clamp, so a mutation that drops the clamp OR the char-boundary
    // walk-back is caught against actual production behaviour.
    #[test]
    fn sync_clamps_offset_within_replaced_body() {
        use crate::services::discord::InflightTurnState;
        use crate::services::provider::ProviderKind;

        let replaced = "⚠ tool-only turn, no assistant text".to_string();
        let mut offset = 900usize; // tracked the long pre-swap body
        let mut state = InflightTurnState::new(
            ProviderKind::Claude,
            1,
            Some("adk-cc".to_string()),
            42,
            5001,
            5002,
            "prompt".to_string(),
            Some("session".to_string()),
            Some("AgentDesk-claude-adk-cc-1".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            None,
            10,
        );
        super::sync_response_delivery_state(&replaced, &mut offset, &mut state);
        // Clamped to len() (out-of-bounds prior offset) and char-boundary valid.
        assert_eq!(offset, replaced.len());
        assert!(replaced.is_char_boundary(offset));
        assert_eq!(state.response_sent_offset, offset);
        assert_eq!(state.full_response, replaced);
        // Mutation guard: the UNCLAMPED prior offset is out of bounds (the wedge).
        assert!(replaced.get(900..).is_none());
        assert!(replaced.get(offset..).is_some());
    }

    // #3419 R3 / codex MEDIUM (the char-boundary case the bare `.min(len)` missed):
    // a replacement BEGINNING with a multibyte sentinel (`⚠` = 3 bytes) with a
    // prior offset of 1. `1 < len()`, so `.min(len)` would leave it UNCHANGED at 1
    // — but byte 1 is INSIDE `⚠`, violating the char-boundary invariant and
    // panicking any later `full_response[offset..]` slice. The normalizer must walk
    // BACK to the nearest valid boundary (0).
    #[test]
    fn sync_normalizes_prior_offset_inside_leading_multibyte_char() {
        use crate::services::discord::InflightTurnState;
        use crate::services::provider::ProviderKind;

        let replaced = "⚠ tool-only turn, no assistant text".to_string();
        // Precondition: byte 1 is genuinely mid-multibyte (the bug surface).
        assert!(!replaced.is_char_boundary(1));
        let mut offset = 1usize; // prior valid offset that now lands mid-char
        let mut state = InflightTurnState::new(
            ProviderKind::Claude,
            1,
            Some("adk-cc".to_string()),
            42,
            5001,
            5002,
            "prompt".to_string(),
            Some("session".to_string()),
            Some("AgentDesk-claude-adk-cc-1".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            None,
            10,
        );
        super::sync_response_delivery_state(&replaced, &mut offset, &mut state);
        // Normalized back to the leading boundary (0) — NOT left at the mid-char 1.
        assert_eq!(offset, 0);
        assert!(replaced.is_char_boundary(offset));
        assert_eq!(state.response_sent_offset, 0);
        // Mutation guard: a bare `.min(len)` (no walk-back) would leave offset 1,
        // which is NOT a char boundary and would panic on `&replaced[1..]`.
        assert!(replaced.get(1..).is_none());
        assert!(replaced.get(offset..).is_some());
    }
}
