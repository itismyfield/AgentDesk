#[cfg(all(unix, any(test, feature = "test-support")))]
#[path = "queued_head_inject_hook_test_support.rs"]
pub(in crate::services::discord) mod hook_tests;

use std::time::Duration;

use super::super::{mailbox_snapshot, session_runtime};
use super::human_input::{HumanInputRequest, InjectAttempt};
use super::transport::IntakeDeps;
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

pub(in crate::services::discord) fn persistence_failed(error: String) -> InjectAttempt {
    tracing::warn!(%error, "queued busy injection persistence failed");
    InjectAttempt::HandbackFailed("queued_head_blocked")
}

/// `ADK_QUEUED_HEAD_INJECT=1` opts into queued-head injection; absent values keep it off.
pub(in crate::services::discord) fn queued_head_env_enabled() -> bool {
    static PROCESS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *PROCESS.get_or_init(|| crate::utils::env_flag("ADK_QUEUED_HEAD_INJECT", false))
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
        #[cfg(all(unix, any(test, feature = "test-support")))]
        hook_tests::attempt(self);
    }
}

/// Offers the first ready input, behind nothing but parked ones, to the busy pane. `true` ends the
/// caller's kickoff: something was reserved or the head is held, so no idle start may follow.
pub(in crate::services::discord) async fn try_busy(
    deps: &IntakeDeps<'_>,
    provider: &ProviderKind,
    channel: ChannelId,
    known_parent: Option<Option<ChannelId>>,
    gate: Option<fn(ChannelId, Option<ChannelId>) -> bool>,
    trigger: &'static str,
) -> bool {
    let Some(gate) = gate else { return false };
    let ports = &deps.shared.router_ports;
    if !(ports.queued_head_ready)(provider, channel) {
        return false;
    }
    // Some(None) records a validated non-thread; only unresolved callers perform a lookup.
    let parent = match known_parent {
        Some(parent) => parent,
        None => session_runtime::resolve_thread_parent(deps.http, channel)
            .await
            .map(|(id, _)| id),
    };
    if !(ports.queued_head_ready)(provider, channel) || !gate(channel, parent) {
        return false;
    }
    // The durable echo may yield before the enqueue's caller releases its transition.
    // This automatic wake retries briefly without parking a transition waiter.
    if trigger == "post_enqueue_queued_head" {
        let transition = deps.shared.session_transition_locks.lock(channel);
        let released = async {
            while transition.try_lock().is_err() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        let _ = tokio::time::timeout(Duration::from_secs(3), released).await;
    }
    let snapshot = mailbox_snapshot(deps.shared, channel).await;
    // A parked or unreadable input is never typed automatically; the actor reads again itself.
    let class = |item: &Intervention| input_class(provider, channel, item);
    let Prefix::Ready { at } = prefix::select(&snapshot.intervention_queue, class) else {
        return false;
    };
    let head = &snapshot.intervention_queue[at];
    // A Decision original never joins a running turn: it waits for the idle start, whose
    // admission checks its event.
    if super::decision_drain::queued::origin(head).is_some() {
        return false;
    }
    let request = HumanInputRequest {
        channel_id: channel,
        provider: provider.clone(),
        text: head.text.clone(),
        author_id: head.author_id.get(),
        source: "queued".into(),
        metadata: None,
        channel_name_hint: None,
    };
    #[cfg(all(unix, any(test, feature = "test-support")))]
    hook_tests::offer(channel, head.message_id);
    let attempt =
        (ports.inject_queued_head)(deps.shared, &request, QueueIdentity::of(head), parent, gate)
            .await;
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
        && matches!(attempt, InjectAttempt::NotSent(veto) | InjectAttempt::HandedBack { veto, .. } if preparation_veto(veto))
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
        InjectAttempt::NotSent(_) | InjectAttempt::HandedBack { .. }
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
