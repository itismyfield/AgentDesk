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
