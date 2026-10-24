use super::super::{health, mailbox_snapshot, router, session_runtime};
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::discord::busy_followup_retry_store::input_class;
use crate::services::provider::ProviderKind;
use crate::services::turn_orchestrator::prefix::{self, Prefix};
use crate::services::turn_orchestrator::{Intervention, QueueIdentity};
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

/// One queued-head attempt as an operator reads it: ids, coded reasons and where the head stands.
/// It has no text field, so an input body cannot reach the log through it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::services::discord) struct AttemptRow {
    pub channel_id: u64,
    pub trigger: &'static str,
    pub head_message_id: u64,
    pub queued_generation: u64,
    pub skipped_parked: usize,
    pub outcome: &'static str,
    pub reason: &'static str,
    pub head_state: &'static str,
}

impl AttemptRow {
    fn emit(&self) {
        tracing::info!(
            channel_id = self.channel_id,
            trigger = self.trigger,
            head_message_id = self.head_message_id,
            queued_generation = self.queued_generation,
            skipped_parked = self.skipped_parked,
            outcome = self.outcome,
            reason = self.reason,
            head_state = self.head_state,
            "queued_head_attempt"
        );
        #[cfg(all(test, unix))]
        tests::ATTEMPT_ROWS.lock().unwrap().push(self.clone());
    }
}

/// Offers the first ready input, behind nothing but parked ones, to the busy pane. `true` ends the
/// caller's kickoff: something was reserved or the head is held, so no idle start may follow.
pub(in crate::services::discord) async fn try_busy(
    deps: &router::IntakeDeps<'_>,
    provider: &ProviderKind,
    channel: ChannelId,
    gate: Option<fn(ChannelId, Option<ChannelId>) -> bool>,
    trigger: &'static str,
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
    // A parked or unreadable input is never typed automatically; the actor reads again itself.
    let class = |item: &Intervention| input_class(provider, channel, item);
    let Prefix::Ready { at } = prefix::select(&snapshot.intervention_queue, class) else {
        return false;
    };
    let head = &snapshot.intervention_queue[at];
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
    let attempt = health::inject_human_input(deps.shared, &request, origin).await;
    let (outcome, reason, head_state) = attempt.queued_report();
    AttemptRow {
        channel_id: channel.get(),
        trigger,
        head_message_id: head.message_id.get(),
        queued_generation: head.queued_generation,
        skipped_parked: at,
        outcome,
        reason,
        head_state,
    }
    .emit();
    if *provider == ProviderKind::Claude
        && matches!(attempt, health::InjectAttempt::NotSent(veto) | health::InjectAttempt::HandedBack { veto, .. } if preparation_veto(veto))
    {
        super::super::turn_bridge::followup_requeue::preparation_notice::enqueue(
            deps.shared,
            provider,
            channel,
            head.message_id.get(),
            head.queued_generation,
            reason,
        )
        .await;
        return true;
    }
    // Other non-submission outcomes retain their existing idle-start policy.
    !matches!(
        attempt,
        health::InjectAttempt::NotSent(_) | health::InjectAttempt::HandedBack { .. }
    )
}

fn preparation_veto(veto: &str) -> bool {
    matches!(
        veto,
        "draft"
            | "modal"
            | "unpredictable_render"
            | "pane_unavailable"
            | "human_attached"
            | "attach_unknown"
            | "lock_contended"
    )
}

#[cfg(all(test, unix))]
#[path = "queued_head_inject_tests.rs"]
mod tests;
