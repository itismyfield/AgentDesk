use super::*;

fn tui_direct_bridge_uncommitted_delivery_error(
    provider: &ProviderKind,
    channel_id: ChannelId,
    user_msg_id: MessageId,
    current_msg_id: MessageId,
) -> Option<String> {
    let state = super::super::inflight::load_inflight_state(provider, channel_id.get())?;
    if state.user_msg_id != user_msg_id.get() || state.current_msg_id != current_msg_id.get() {
        return None;
    }
    if state.terminal_delivery_committed {
        return None;
    }
    if !state.full_response.trim().is_empty()
        && state.response_sent_offset >= state.full_response.len()
    {
        return None;
    }

    Some(format!(
        "TUI-direct bridge finished without committed terminal delivery for provider {} channel {} user_msg_id {} current_msg_id {}",
        provider.as_str(),
        channel_id.get(),
        user_msg_id.get(),
        current_msg_id.get()
    ))
}

#[allow(clippy::too_many_arguments)]
pub(super) fn ensure_tui_direct_bridge_delivery_committed(
    provider: &ProviderKind,
    channel_id: ChannelId,
    user_msg_id: MessageId,
    current_msg_id: MessageId,
    tmux_session_name: &str,
    lease: &ExternalInputRelayLease,
    prompt_anchor_message_id: Option<u64>,
    streamed: bool,
) -> Result<(), String> {
    let Some(error) = tui_direct_bridge_uncommitted_delivery_error(
        provider,
        channel_id,
        user_msg_id,
        current_msg_id,
    ) else {
        return Ok(());
    };

    if streamed {
        tracing::warn!(
            channel_id = channel_id.get(),
            tmux_session_name = %tmux_session_name,
            provider = %provider.as_str(),
            turn_id = lease.turn_id.as_deref().unwrap_or(""),
            session_key = lease.session_key.as_deref().unwrap_or(""),
            relay_owner = lease.relay_owner.as_str(),
            runtime_kind = lease.runtime_kind.map(|kind| kind.as_str()).unwrap_or("unknown"),
            current_msg_id = current_msg_id.get(),
            prompt_anchor_message_id,
            error = %error,
            "TUI-direct bridge adapter finished without committed streamed response relay"
        );
    } else {
        tracing::warn!(
            channel_id = channel_id.get(),
            tmux_session_name = %tmux_session_name,
            provider = %provider.as_str(),
            turn_id = lease.turn_id.as_deref().unwrap_or(""),
            session_key = lease.session_key.as_deref().unwrap_or(""),
            relay_owner = lease.relay_owner.as_str(),
            runtime_kind = lease.runtime_kind.map(|kind| kind.as_str()).unwrap_or("unknown"),
            current_msg_id = current_msg_id.get(),
            prompt_anchor_message_id,
            error = %error,
            "TUI-direct bridge adapter finished without committed response relay"
        );
    }
    Err(error)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    const AGENTDESK_ROOT_DIR_ENV: &str = "AGENTDESK_ROOT_DIR";

    struct EnvGuard(Option<String>);

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match self.0.take() {
                Some(value) => unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, value) },
                None => unsafe { std::env::remove_var(AGENTDESK_ROOT_DIR_ENV) },
            }
        }
    }

    #[test]
    fn tui_direct_bridge_completion_rejects_uncommitted_matching_inflight() {
        let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
        let previous = std::env::var(AGENTDESK_ROOT_DIR_ENV).ok();
        let _guard = EnvGuard(previous);
        let temp = tempfile::tempdir().expect("temp runtime root");
        unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, temp.path()) };

        let provider = ProviderKind::Codex;
        let channel_id = ChannelId::new(88_001);
        let user_msg_id = MessageId::new(88_002);
        let current_msg_id = MessageId::new(88_003);
        let mut lease = ExternalInputRelayLease::unassigned(Some(channel_id.get()));
        lease.relay_owner = ExternalInputRelayOwner::BridgeAdapter;
        let output_path = temp.path().join("out.jsonl");
        let state = super::super::build_tui_direct_bridge_inflight_state(
            provider.clone(),
            channel_id,
            user_msg_id,
            current_msg_id,
            "prompt",
            "AgentDesk-codex-test",
            output_path.as_path(),
            0,
            &lease,
        );
        super::super::super::inflight::save_inflight_state(&state).expect("save inflight");

        let error = tui_direct_bridge_uncommitted_delivery_error(
            &provider,
            channel_id,
            user_msg_id,
            current_msg_id,
        )
        .expect("matching uncommitted inflight must be an error");
        assert!(error.contains("without committed terminal delivery"));

        let mut fallback_delivered = state.clone();
        fallback_delivered.full_response = "fallback already delivered".to_string();
        fallback_delivered.response_sent_offset = fallback_delivered.full_response.len();
        super::super::super::inflight::save_inflight_state(&fallback_delivered)
            .expect("save fallback-delivered inflight");
        assert!(
            tui_direct_bridge_uncommitted_delivery_error(
                &provider,
                channel_id,
                user_msg_id,
                current_msg_id,
            )
            .is_none(),
            "fallback delivery that advanced response_sent_offset must not be retried"
        );

        let mut committed = fallback_delivered;
        committed.terminal_delivery_committed = true;
        super::super::super::inflight::save_inflight_state(&committed)
            .expect("save committed inflight");
        assert!(
            tui_direct_bridge_uncommitted_delivery_error(
                &provider,
                channel_id,
                user_msg_id,
                current_msg_id,
            )
            .is_none(),
            "committed terminal delivery must not be reported as placeholder-only failure"
        );
    }
}
