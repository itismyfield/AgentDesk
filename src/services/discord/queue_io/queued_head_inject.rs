use super::super::{health, mailbox_snapshot, router, session_runtime};
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::provider::ProviderKind;
use crate::services::turn_orchestrator::QueueIdentity;
use poise::serenity_prelude::ChannelId;

pub(in crate::services::discord) type BindingKey = (
    RuntimeHandoffKind,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
);

pub(in crate::services::discord) fn binding_key(session: &str) -> Option<BindingKey> {
    let binding = crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(session)?;
    Some((
        binding.runtime_kind,
        binding.output_path,
        binding.relay_output_path,
        binding.input_fifo_path,
        binding.session_id,
    ))
}

pub(in crate::services::discord) fn persistence_failed(error: String) -> health::InjectAttempt {
    tracing::warn!(%error, "queued busy injection persistence failed");
    health::InjectAttempt::HandbackFailed("queued_head_blocked")
}

/// `ADK_QUEUED_HEAD_INJECT=1` opts into queued-head injection; absent values keep it off.
pub(in crate::services::discord) fn queued_head_enabled(channel: u64) -> bool {
    #[cfg(all(test, unix))]
    return health::inject_hook::queued_head_enabled(channel);
    #[cfg(not(all(test, unix)))]
    {
        static PROCESS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let _ = channel;
        *PROCESS
            .get_or_init(|| std::env::var("ADK_QUEUED_HEAD_INJECT").is_ok_and(|value| value == "1"))
    }
}

pub(in crate::services::discord) async fn try_busy(
    deps: &router::IntakeDeps<'_>,
    provider: &ProviderKind,
    channel: ChannelId,
    gate: Option<fn(ChannelId, Option<ChannelId>) -> bool>,
) -> bool {
    let Some(gate) = gate else { return false };
    if !health::InjectOrigin::queued_head_ready(provider, channel) {
        return false;
    }
    let parent = session_runtime::resolve_thread_parent(deps.http, channel)
        .await
        .map(|(id, _)| id);
    if !health::InjectOrigin::queued_head_gate_enabled(provider, channel, parent, gate) {
        return false;
    }
    let snapshot = mailbox_snapshot(deps.shared, channel).await;
    let Some(head) = snapshot.intervention_queue.first() else {
        return false;
    };
    // A parked or unreadable head is never typed into the pane automatically.
    use crate::services::discord::busy_followup_retry_store::{RetryClass, input_class};
    if input_class(provider, channel, head) != RetryClass::Ready {
        return false;
    }
    let request = health::HumanInputRequest {
        channel_id: channel,
        provider: provider.clone(),
        text: head.text.clone(),
        author_id: head.author_id.get(),
        source: "queued".into(),
        metadata: None,
        channel_name_hint: None,
    };
    #[cfg(all(test, unix))]
    tests::OFFERED_HEADS.lock().unwrap().push(head.message_id);
    let origin = health::InjectOrigin::QueuedHead {
        expected: QueueIdentity::of(head),
        parent,
        gate,
    };
    !matches!(
        health::inject_human_input(deps.shared, &request, origin).await,
        health::InjectAttempt::NotSent(_)
    )
}

#[cfg(all(test, unix))]
#[path = "queued_head_inject_tests.rs"]
mod tests;
