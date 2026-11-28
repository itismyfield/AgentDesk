//! The router functions every `SharedData` carries as its `RouterPorts`.

use std::sync::Arc;

use poise::serenity_prelude::ChannelId;

use crate::services::discord::SharedData;
use crate::services::discord::health::{InjectOrigin, inject_human_input};
use crate::services::discord::queue_io::human_input::{HumanInputRequest, InjectAttempt};
use crate::services::discord::queue_io::transport::IntakeDeps;
use crate::services::discord::shared_state::{
    PortFuture, PromoteBlockedFuture, QueuedAdmission, QueuedRun, RouterPorts,
};
use crate::services::provider::ProviderKind;
use crate::services::turn_orchestrator::{DispatchLease, Intervention, QueueIdentity};

pub(in crate::services::discord) const ROUTER_PORTS: RouterPorts = RouterPorts {
    #[cfg(unix)]
    owner_wake: super::message_handler::owner_wake,
    checkpoint_waits: super::checkpoint_waits_for_admission,
    promote_readiness_blocked,
    admit_and_finish_queued,
    start_exact_retry,
    defer_promoted_dispatch,
    queued_head_inject_covers: super::queued_head_inject_covers,
    queued_head_ready: InjectOrigin::queued_head_ready,
    inject_queued_head,
    owned_original: super::input_payload::owned_original,
    #[cfg(unix)]
    capture_dormant_partial: |shared, row, output| {
        Box::pin(crate::services::discord::tui_prompt_relay::capture_dormant_partial(shared, row, output))
    },
    #[cfg(unix)]
    attach_restarted_herdr_source: |pool, owner, channel, reader| {
        Box::pin(crate::services::discord::tui_prompt_relay::herdr_source::attach_restarted_herdr_source(
            pool, owner, channel, reader,
        ))
    },
    #[cfg(unix)]
    rehydrate_codex_rollout: crate::services::discord::tui_prompt_relay::rehydration::codex_tui_rehydrated_binding_from_rollout_path,
    #[cfg(unix)]
    other_session_claimed_claude_transcripts: crate::services::discord::tui_prompt_relay::other_session_claimed_transcripts,
    #[cfg(unix)]
    resolve_rehydrated_claude_channel: crate::services::discord::tui_prompt_relay::resolve_rehydrated_claude_tmux_channel_id,
    #[cfg(unix)]
    claude_tui_launch_context: crate::services::discord::tui_prompt_relay::claude_tui_launch_context,
    #[cfg(unix)]
    owner_start_session_key,
};

fn inject_queued_head<'a>(
    shared: &'a Arc<SharedData>,
    request: &'a HumanInputRequest,
    expected: QueueIdentity,
    parent: Option<ChannelId>,
    gate: fn(ChannelId, Option<ChannelId>) -> bool,
) -> PortFuture<'a, InjectAttempt> {
    let origin = InjectOrigin::QueuedHead {
        expected,
        parent,
        gate,
    };
    Box::pin(inject_human_input(shared, request, origin))
}

fn promote_readiness_blocked<'a>(
    shared: &'a Arc<SharedData>,
    provider: &'a ProviderKind,
    channel: ChannelId,
) -> PromoteBlockedFuture<'a> {
    Box::pin(super::hosted_tui_promote_readiness_blocked(
        shared, provider, channel,
    ))
}

/// The queued admission both queue runners share: admit, run the caller's step, finish.
fn admit_and_finish_queued<'a>(
    deps: &'a IntakeDeps<'a>,
    admission: QueuedAdmission<'a>,
    after_admission: PortFuture<'a, bool>,
) -> PortFuture<'a, QueuedRun> {
    use super::QueuedAdmissionDisposition as Disposition;
    let representative = admission.intervention.message_id.get();
    let sources = admission.intervention.source_message_ids.iter();
    let sources = sources.map(|id| id.get()).collect();
    let run = async move {
        let QueuedAdmission {
            provider,
            channel_id,
            intervention,
            request_owner,
            request_owner_name,
            defer_watcher_resume,
            wait_for_completion,
            backstop_reason,
            dispatch_lease,
        } = admission;
        let admitted = match super::admit_queued_intake(
            deps,
            provider,
            channel_id,
            intervention,
            request_owner,
            request_owner_name,
            defer_watcher_resume,
            wait_for_completion,
            backstop_reason,
            dispatch_lease,
        )
        .await
        {
            Disposition::Admitted(admitted) => admitted,
            Disposition::Deferred
            | Disposition::RejectedNonPortableAttachment
            | Disposition::ConsumedToHold
            | Disposition::ClosedDecision => return QueuedRun::NotAdmitted,
            Disposition::RejectedRestore => return QueuedRun::RejectedRestore,
        };
        let after_admission = after_admission.await;
        let result = super::finish_admitted_queued_intake(deps, admitted).await;
        QueuedRun::Finished {
            after_admission,
            result,
        }
    };
    Box::pin(crate::logging::intake_stage::merged(
        representative,
        sources,
        run,
    ))
}

fn start_exact_retry<'a>(
    http: &'a Arc<poise::serenity_prelude::http::Http>,
    shared: &'a Arc<SharedData>,
    channel_id: ChannelId,
    retry: crate::services::discord::replay_policy::ExactRetry,
) -> PortFuture<'a, crate::services::discord::replay_policy::ExactHandoff> {
    Box::pin(super::start_exact_retry(http, shared, channel_id, retry))
}

fn defer_promoted_dispatch<'a>(
    shared: &'a Arc<SharedData>,
    provider: &'a ProviderKind,
    channel_id: ChannelId,
    intervention: &'a Intervention,
    dispatch_lease: Arc<DispatchLease>,
) -> PortFuture<'a, bool> {
    Box::pin(super::defer_promoted_dispatch_if_hosted_tui_busy(
        shared,
        provider,
        channel_id,
        intervention,
        dispatch_lease,
    ))
}

#[cfg(unix)]
fn owner_start_session_key<'a>(
    shared: &'a Arc<SharedData>,
    channel: ChannelId,
    context: &'a serde_json::Value,
) -> PortFuture<'a, Option<String>> {
    Box::pin(super::message_handler::owner_start_session_key(
        shared, channel, context,
    ))
}
