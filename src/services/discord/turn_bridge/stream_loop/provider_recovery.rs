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
    partial_response: &str,
    message: &str,
    stderr: &str,
) {
    let Some(lease) = lease else {
        return;
    };
    let Some(signal) = agent_recovery::trigger_from_error_message(message)
        .or_else(|| agent_recovery::trigger_from_error_message(stderr))
    else {
        return;
    };
    if !crate::services::discord::inflight::load_inflight_state_read_only(provider, channel.get())
        .is_some_and(|current| expected.matches_state(&current))
    {
        return;
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
