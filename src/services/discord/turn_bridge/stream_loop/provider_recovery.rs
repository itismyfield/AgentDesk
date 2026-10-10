//! Persist before waking the watchdog; never stop/start runtimes from their own bridge.
use crate::services::agent_recovery::{self, CheckpointPayload, ObserveInput, RecoveryLease};
use crate::services::discord::{
    SharedData,
    inflight::{InflightTurnIdentity, InflightTurnState},
};
use crate::services::provider::ProviderKind;
use poise::serenity_prelude::ChannelId;

pub(super) async fn on_error(
    shared: &SharedData,
    provider: &ProviderKind,
    channel: ChannelId,
    lease: Option<&RecoveryLease>,
    expected: &InflightTurnIdentity,
    inflight: &InflightTurnState,
    allow_profile_retry: bool,
    any_tool_used: bool,
    partial_response: &str,
    message: &str,
    stderr: &str,
) -> bool {
    if before_start_recovery_blocked(provider, channel, &shared.token_hash, expected, inflight) {
        return false;
    }
    // A profile retry owns dispatch recovery; do not also request an agent takeover.
    if allow_profile_retry
        && try_profile_retry(
            provider,
            channel,
            expected,
            inflight,
            any_tool_used,
            partial_response,
            message,
            stderr,
        )
    {
        return true;
    }
    let Some(lease) = lease else {
        return false;
    };
    let Some(signal) = agent_recovery::trigger_from_error_message(message)
        .or_else(|| agent_recovery::trigger_from_error_message(stderr))
    else {
        return false;
    };
    if !crate::services::discord::inflight::load_inflight_state_read_only(provider, channel.get())
        .is_some_and(|current| expected.matches_state(&current))
    {
        return false;
    }
    let workspace = shared
        .core
        .lock()
        .await
        .sessions
        .get(&channel)
        .and_then(|session| session.current_path.clone());
    let checkpoint = CheckpointPayload::compact(
        &lease.active_writer_agent_id,
        inflight.user_text.chars().take(4000).collect::<String>(),
        partial_response.chars().take(8000).collect::<String>(),
        "",
        Vec::new(),
        "Inspect the inherited workspace and continue the unfinished request.",
        &inflight.user_text,
    );
    match agent_recovery::observe_provider_error(
        lease,
        ObserveInput {
            channel_id: channel.get().to_string(),
            primary_turn_id: inflight.effective_finalizer_turn_id().to_string(),
            signal,
        },
        checkpoint,
        workspace,
    )
    .await
    {
        Ok(true) => agent_recovery::recovery_wakeup(provider).notify_one(),
        Ok(false) => {}
        Err(error) => {
            tracing::warn!(channel_id = channel.get(), error = %error, "provider error recovery not committed; retaining normal error handling")
        }
    }
    false
}

/// Retry only an anchored, unprocessed request. A tool may have committed an
/// external effect even when the provider subsequently reports a limit error.
pub(super) fn profile_retry_eligible(
    request: u64,
    any_tool_used: bool,
    partial_response: &str,
    message: &str,
    stderr: &str,
) -> bool {
    request != 0
        && !any_tool_used
        && partial_response.trim().is_empty()
        && agent_recovery::trigger_from_error_message(message)
            .or_else(|| agent_recovery::trigger_from_error_message(stderr))
            .is_some()
}

pub(super) fn try_profile_retry(
    provider: &ProviderKind,
    channel: ChannelId,
    expected: &InflightTurnIdentity,
    inflight: &InflightTurnState,
    any_tool_used: bool,
    partial_response: &str,
    message: &str,
    stderr: &str,
) -> bool {
    if inflight.turn_source != crate::services::discord::inflight::TurnSource::Managed
        || !profile_retry_eligible(
            inflight.user_msg_id,
            any_tool_used || inflight.any_tool_used,
            partial_response,
            message,
            stderr,
        )
        || !crate::services::discord::inflight::load_inflight_state_read_only(
            provider,
            channel.get(),
        )
        .is_some_and(|current| expected.matches_state(&current))
    {
        return false;
    }
    // A durable agent takeover pins its account and must keep that authority.
    if !matches!(
        agent_recovery::pinned_auth_profile(&channel.get().to_string(), provider, None),
        Ok(None)
    ) {
        return false;
    }
    match crate::services::discord::org_schema::advance_auth_profile(
        provider,
        channel.get(),
        inflight.user_msg_id,
    ) {
        Ok(Some((from, to))) => {
            tracing::info!(provider = provider.as_str(), channel_id = channel.get(), from_profile = %from, to_profile = %to, "retrying unprocessed request with another auth profile");
            true
        }
        Ok(None) => false,
        Err(error) => {
            tracing::warn!(channel_id = channel.get(), %error, "profile fallback unavailable");
            false
        }
    }
}

#[cfg(test)]
mod profile_tests {
    use super::profile_retry_eligible;

    #[test]
    fn only_unprocessed_anchored_classified_failures_are_replayed() {
        for error in [
            "HTTP 429",
            "quota exhausted",
            "provider produced no output for 180 seconds",
            "tmux session dead",
        ] {
            assert!(profile_retry_eligible(12, false, "", error, ""));
            assert!(!profile_retry_eligible(12, true, "", error, ""));
            assert!(!profile_retry_eligible(
                12,
                false,
                "partial answer",
                error,
                ""
            ));
            assert!(!profile_retry_eligible(0, false, "", error, ""));
        }
        for error in [
            "usage fetch failed",
            "permission denied",
            "user cancelled",
            "invalid configuration",
        ] {
            assert!(!profile_retry_eligible(12, false, "", error, ""));
        }
        assert!(profile_retry_eligible(
            12,
            false,
            "",
            "provider failed",
            "RESOURCE_EXHAUSTED"
        ));
    }
}

fn before_start_recovery_blocked(
    provider: &ProviderKind,
    channel: ChannelId,
    token_hash: &str,
    expected: &InflightTurnIdentity,
    inflight: &InflightTurnState,
) -> bool {
    use crate::services::provider::cancel_token_claude_interrupt::{
        herdr_stop_settlement_available, herdr_turn,
    };
    if !herdr_stop_settlement_available()
        || crate::services::provider::herdr_before_start::mutant("on_error_ignores_user_stop")
    {
        return false;
    }
    let herdr = inflight.host_locator.as_ref().is_some_and(|locator| {
        serde_json::to_value(locator)
            .ok()
            .is_some_and(|wire| wire["host_kind"] == "herdr")
    });
    if !herdr {
        return false;
    }
    let Some(nonce) = inflight.turn_nonce.as_deref() else {
        return true;
    };
    let Some(logical) = inflight.tmux_session_name.as_deref() else {
        return true;
    };
    let Some(state) = herdr_turn(logical, nonce) else {
        return true;
    };
    if state.owner.discord_token_hash != token_hash
        || state.owner.provider != provider.as_str()
        || state.owner.channel_id != channel.get().to_string()
        || !expected.matches_state(inflight)
        || !crate::services::discord::inflight::load_inflight_state_read_only(
            provider,
            channel.get(),
        )
        .is_some_and(|row| {
            expected.matches_state(&row)
                && row.turn_nonce == inflight.turn_nonce
                && row.provider == inflight.provider
                && row.born_generation == inflight.born_generation
        })
    {
        return true;
    }
    state.user_stop.load(std::sync::atomic::Ordering::Acquire)
        || state.closed_probe().unwrap_or(true)
}

#[cfg(all(test, unix))]
mod coldstop_recovery_tests {
    use super::*;
    use crate::db::dispatched_sessions::hosted_execution::HostedOwner;
    use crate::services::provider::CancelToken;
    use crate::services::provider::cancel_token_claude_interrupt::HERDR_SETTLEMENT_OVERRIDE;
    use std::sync::atomic::Ordering;
    #[tokio::test(flavor = "current_thread")]
    async fn coldstop_on_error_blocks_profile_and_takeover_before_any_recovery() {
        let root = tempfile::tempdir().unwrap();
        let _env = crate::config::set_agentdesk_root_for_test(root.path());
        let shared = crate::services::discord::make_shared_data_for_tests_with_storage(None);
        let provider = shared.provider.clone();
        let actor = CancelToken::new();
        let channel = ChannelId::new(913);
        let logical = "AgentDesk-cold-recovery";
        let owner = HostedOwner {
            provider: provider.as_str().into(),
            discord_token_hash: shared.token_hash.clone(),
            channel_id: "913".into(),
            logical_key: logical.into(),
            owner_node: "node".into(),
            runtime_root: "root".into(),
        };
        let state = actor.prepare_herdr_interrupt(provider.clone(), &owner);
        let mut row = InflightTurnState::new(
            provider.clone(),
            channel.get(),
            None,
            1,
            913,
            0,
            "cold".into(),
            None,
            Some(logical.into()),
            None,
            None,
            0,
        );
        row.turn_nonce = actor.turn_nonce().map(str::to_owned);
        let mut wire = serde_json::to_value(&row).unwrap();
        wire["host_locator"] = serde_json::json!({ "host_kind": "herdr", "host_session_id": "session", "pane": "pane", "execution_node": "node" });
        row = serde_json::from_value(wire).unwrap();
        crate::services::discord::inflight::save_inflight_state(&row).unwrap();
        let expected = InflightTurnIdentity::from_state(&row);
        assert!(!before_start_recovery_blocked(
            &provider,
            channel,
            &shared.token_hash,
            &expected,
            &row
        ));
        state.user_stop.store(true, Ordering::Release);
        assert!(before_start_recovery_blocked(
            &provider,
            channel,
            &shared.token_hash,
            &expected,
            &row
        ));
        let before = serde_json::to_vec(
            &crate::services::discord::inflight::load_inflight_state_read_only(
                &provider,
                channel.get(),
            )
            .unwrap(),
        )
        .unwrap();
        assert!(
            !on_error(
                &shared,
                &provider,
                channel,
                None,
                &expected,
                &row,
                true,
                false,
                "",
                "quota exhausted",
                ""
            )
            .await
        );
        assert_eq!(
            before,
            serde_json::to_vec(
                &crate::services::discord::inflight::load_inflight_state_read_only(
                    &provider,
                    channel.get()
                )
                .unwrap()
            )
            .unwrap()
        );
        HERDR_SETTLEMENT_OVERRIDE.set(false);
        assert!(!before_start_recovery_blocked(
            &provider,
            channel,
            &shared.token_hash,
            &expected,
            &row
        ));
        HERDR_SETTLEMENT_OVERRIDE.set(true);
        let mut missing = row.clone();
        missing.turn_nonce = Some("missing".into());
        assert!(before_start_recovery_blocked(
            &provider,
            channel,
            &shared.token_hash,
            &expected,
            &missing
        ));
        state.user_stop.store(false, Ordering::Release);
        let lock = state.submission.lock().unwrap();
        assert!(before_start_recovery_blocked(
            &provider,
            channel,
            &shared.token_hash,
            &expected,
            &row
        ));
        drop(lock);
    }
}
