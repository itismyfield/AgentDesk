use super::message_handler::{self, IntakeDeps, IntakeRequest};
use crate::services::cluster::intake_router_hook::{
    IntakeBlockedReason, IntakeRouterContext, IntakeRouterDecision, effective_intake_routing_mode,
    try_route_intake,
};
use crate::services::provider::ProviderKind;

mod notice;
mod queued;
mod skill;
#[cfg(test)]
mod tests;

use notice::notify_blocked_intake;
pub(crate) use queued::{
    QueuedAdmissionDisposition, admit_queued_intake, finish_admitted_queued_intake,
};
pub(crate) use skill::dispatch_skill_intake;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IntakeOrigin {
    LiveMessage,
    QueuedDrain,
    SlashSkill,
    TextSkill,
}

impl IntakeOrigin {
    fn should_notify_blocked(self) -> bool {
        !matches!(self, Self::QueuedDrain)
    }
}

pub(crate) struct IntakeSubmission {
    pub(crate) provider: ProviderKind,
    pub(crate) request: IntakeRequest,
    pub(crate) origin: IntakeOrigin,
    pub(crate) preserve_on_cancel: bool,
    pub(crate) has_nonportable_uploads: bool,
    pub(crate) preloaded_uploads: Vec<String>,
    pub(crate) voice_announcement: Option<crate::voice::prompt::VoiceTranscriptAnnouncement>,
}

#[derive(Debug)]
pub(crate) struct LocalAdmissionPermit(());

#[derive(Debug)]
pub(crate) enum IntakeAdmission {
    Local(LocalAdmissionPermit),
    Forwarded {
        target_instance_id: String,
        outbox_id: i64,
    },
    SkippedDuplicate,
    DeferredOpenRoute {
        target_instance_id: String,
    },
    Blocked {
        reason: IntakeBlockedReason,
    },
}

pub(crate) async fn admit_text_intake(
    deps: &IntakeDeps<'_>,
    submission: &IntakeSubmission,
) -> IntakeAdmission {
    let mode = effective_intake_routing_mode();
    let Some(pool) = deps.shared.pg_pool.as_ref() else {
        if matches!(
            mode,
            crate::services::cluster::intake_router_hook::IntakeRoutingMode::Enforce
        ) {
            return IntakeAdmission::Blocked {
                reason: IntakeBlockedReason::RoutingDependencyFailed {
                    detail: "Postgres pool unavailable for owner lookup".to_string(),
                },
            };
        }
        return IntakeAdmission::Local(LocalAdmissionPermit(()));
    };

    let request = &submission.request;
    let leader_instance_id =
        crate::services::cluster::node_registry::resolve_self_instance_id_without_config();
    let channel_id = request.channel_id.get().to_string();
    let user_msg_id = request.user_msg_id.get().to_string();
    let request_owner_id = request.request_owner.get().to_string();
    let node_override =
        super::super::commands::channel_node_override(deps.shared, request.channel_id);
    let turn_kind = match request.turn_kind {
        super::TurnKind::Foreground => "foreground",
        super::TurnKind::BackgroundTrigger => "background_trigger",
    };
    let ctx = IntakeRouterContext {
        mode,
        leader_instance_id: &leader_instance_id,
        provider: submission.provider.as_str(),
        channel_id: &channel_id,
        user_msg_id: &user_msg_id,
        request_owner_id: &request_owner_id,
        request_owner_name: Some(&request.request_owner_name),
        user_text: &request.user_text,
        reply_context: request.reply_context.as_deref(),
        has_reply_boundary: request.has_reply_boundary,
        dm_hint: request.dm_hint,
        turn_kind,
        merge_consecutive: request.merge_consecutive,
        reply_to_user_message: request.reply_to_user_message,
        defer_watcher_resume: request.defer_watcher_resume,
        wait_for_completion: request.wait_for_completion,
        preserve_on_cancel: submission.preserve_on_cancel,
        node_override_instance_id: node_override.as_deref(),
        has_nonportable_uploads: submission.has_nonportable_uploads
            || !submission.preloaded_uploads.is_empty(),
    };

    let decision = try_route_intake(pool, &ctx).await;
    let admission = match decision {
        IntakeRouterDecision::RanLocal { reason } => {
            tracing::debug!(
                ?reason,
                channel_id,
                user_msg_id,
                "[intake_dispatch] admitted local"
            );
            IntakeAdmission::Local(LocalAdmissionPermit(()))
        }
        IntakeRouterDecision::Observed { outcome } => {
            tracing::info!(
                ?outcome,
                channel_id,
                user_msg_id,
                "[intake_dispatch] owner-aware observe route admitted local"
            );
            IntakeAdmission::Local(LocalAdmissionPermit(()))
        }
        IntakeRouterDecision::Forwarded {
            target_instance_id,
            outbox_id,
        } => IntakeAdmission::Forwarded {
            target_instance_id,
            outbox_id,
        },
        IntakeRouterDecision::SkippedDuplicate => IntakeAdmission::SkippedDuplicate,
        IntakeRouterDecision::DeferredOpenRoute { target_instance_id } => {
            IntakeAdmission::DeferredOpenRoute { target_instance_id }
        }
        IntakeRouterDecision::Blocked { reason } => IntakeAdmission::Blocked { reason },
    };
    log_nonlocal_admission(&admission, &channel_id, &user_msg_id);
    admission
}

/// Common convenience path for producers that do not already own a durable
/// queued Intervention. Queue drains call `admit_text_intake` first so they can
/// preserve the original item before any marker/card teardown.
pub(crate) async fn dispatch_text_intake(
    deps: &IntakeDeps<'_>,
    submission: IntakeSubmission,
) -> Result<(), super::super::Error> {
    let admission = admit_text_intake(deps, &submission).await;
    match admission {
        IntakeAdmission::Local(permit) => {
            finish_admitted_local(deps, permit, submission).await?;
        }
        IntakeAdmission::DeferredOpenRoute { .. } => {
            defer_live_submission(deps, submission).await;
        }
        IntakeAdmission::Blocked { ref reason } if submission.origin.should_notify_blocked() => {
            notify_blocked_intake(deps, &submission, reason).await;
        }
        IntakeAdmission::Forwarded { .. }
        | IntakeAdmission::SkippedDuplicate
        | IntakeAdmission::Blocked { .. } => {}
    }
    Ok(())
}

pub(crate) async fn finish_admitted_local(
    deps: &IntakeDeps<'_>,
    _permit: LocalAdmissionPermit,
    submission: IntakeSubmission,
) -> Result<(), super::super::Error> {
    let preserve_on_cancel = submission.preserve_on_cancel;
    let queued_drain = matches!(submission.origin, IntakeOrigin::QueuedDrain);
    message_handler::finish_admitted_local(
        deps,
        submission.request,
        preserve_on_cancel,
        queued_drain,
        submission.preloaded_uploads,
        submission.voice_announcement,
    )
    .await
}

async fn defer_live_submission(deps: &IntakeDeps<'_>, submission: IntakeSubmission) {
    if matches!(submission.origin, IntakeOrigin::QueuedDrain) {
        return;
    }
    let request = submission.request;
    let channel_id = request.channel_id;
    let intervention = super::response_format::build_race_requeued_intervention(
        request.request_owner,
        request.user_msg_id,
        &request.user_text,
        submission.preserve_on_cancel,
        request.reply_context,
        request.has_reply_boundary,
        request.merge_consecutive,
        submission.preloaded_uploads,
        submission.voice_announcement,
    );
    let _outcome = super::super::queue_io::with_post_enqueue_idle_queue_kick_suppressed(
        super::super::mailbox_enqueue_intervention(
            deps.shared,
            &submission.provider,
            channel_id,
            intervention,
        ),
    )
    .await;
    // The in-memory enqueue may still have succeeded when durable persistence
    // reports an error. Always arm the lost-wakeup net when backlog exists.
    super::super::arm_slow_idle_queue_backstop_if_queue_nonempty(
        deps.shared,
        &submission.provider,
        channel_id,
        "intake_open_route_deferred",
    )
    .await;
}

fn log_nonlocal_admission(admission: &IntakeAdmission, channel_id: &str, user_msg_id: &str) {
    match admission {
        IntakeAdmission::Forwarded {
            target_instance_id,
            outbox_id,
        } => tracing::info!(
            %target_instance_id,
            outbox_id,
            channel_id,
            user_msg_id,
            "[intake_dispatch] forwarded; local execution fenced"
        ),
        IntakeAdmission::SkippedDuplicate => tracing::info!(
            channel_id,
            user_msg_id,
            "[intake_dispatch] duplicate skipped; local execution fenced"
        ),
        IntakeAdmission::DeferredOpenRoute { target_instance_id } => tracing::info!(
            %target_instance_id,
            channel_id,
            user_msg_id,
            "[intake_dispatch] distinct open route deferred; local execution fenced"
        ),
        IntakeAdmission::Blocked { reason } => tracing::warn!(
            ?reason,
            channel_id,
            user_msg_id,
            "[intake_dispatch] unsafe placement blocked; local execution fenced"
        ),
        IntakeAdmission::Local(_) => {}
    }
}
