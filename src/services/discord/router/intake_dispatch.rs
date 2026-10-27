use super::message_handler::{self, IntakeDeps, IntakeRequest};
use crate::db::intake_outbox_status::IntakeOutboxStatus;
use crate::services::cluster::intake_router_hook::{
    IntakeBlockedReason, IntakeRouterContext, IntakeRouterDecision, ResolvedSessionOwner,
    try_route_intake,
};
use crate::services::cluster::intake_routing_config::OwnerAuthorityChannelOptIn;
use crate::services::provider::ProviderKind;
use crate::services::tui_o::cutover::intake_route::{self, IntakeRoute};
use poise::serenity_prelude as serenity;

#[derive(Debug)]
pub(in crate::services::discord::router) struct QueuedTransitionBusyHandoff {
    pub(in crate::services::discord::router) owned_uploads:
        crate::services::cluster::attachment_transfer::uploads::PendingUploads,
}
impl std::fmt::Display for QueuedTransitionBusyHandoff {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("queued intake transition busy")
    }
}
impl std::error::Error for QueuedTransitionBusyHandoff {}

mod attachment;
#[cfg(test)]
mod home_order_tests;
#[cfg(test)]
mod initial_fresh_tests;
mod notice;
mod policy_channel;
mod queued;
#[cfg(test)]
pub(super) use queued::transition_busy_tests::after_upload_take;
pub(crate) mod replay_admission;
#[cfg(test)]
mod replay_admission_tests;
mod skill;
#[cfg(test)]
mod tests;

pub(crate) use attachment::{prepare_admitted_live_attachments, resolve_attachment_admission};
use notice::notify_blocked_intake;
pub(crate) use queued::{
    QueuedAdmissionDisposition, admit_queued_intake, finish_admitted_queued_intake,
};
pub(crate) use skill::dispatch_skill_intake;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IntakeOrigin {
    LiveMessage,
    /// A queue item, carrying the route it resolved before it was queued.
    QueuedDrain(Option<crate::services::turn_orchestrator::QueuedRoute>),
    SlashSkill,
    RawAttachment,
}

impl IntakeOrigin {
    fn should_notify_blocked(self, reason: &IntakeBlockedReason) -> bool {
        !matches!(self, Self::QueuedDrain(_))
            || matches!(
                reason,
                IntakeBlockedReason::NonPortableAttachmentForeignOwner { .. }
                    | IntakeBlockedReason::NonPortableAttachmentRoutedTarget { .. }
                    | IntakeBlockedReason::AttachmentUnavailable { .. }
            )
    }
}

pub(crate) struct IntakeSubmission {
    pub(crate) provider: ProviderKind,
    pub(crate) request: IntakeRequest,
    pub(crate) origin: IntakeOrigin,
    pub(crate) preserve_on_cancel: bool,
    pub(crate) has_nonportable_uploads: bool,
    pub(in crate::services::discord::router) attachments:
        Vec<message_handler::AttachmentDescriptor>,
    pub(crate) preloaded_uploads:
        crate::services::cluster::attachment_transfer::uploads::PendingUploads,
    pub(crate) voice_announcement: Option<crate::voice::prompt::VoiceTranscriptAnnouncement>,
}

#[derive(Debug)]
pub(crate) struct LocalAdmissionPermit {
    channel_id: serenity::ChannelId,
    request_owner: serenity::UserId,
    prepared_uploads: crate::services::cluster::attachment_transfer::uploads::PendingUploads,
}

impl LocalAdmissionPermit {
    fn for_submission(submission: &IntakeSubmission) -> Self {
        Self {
            channel_id: submission.request.channel_id,
            request_owner: submission.request.request_owner,
            prepared_uploads: Vec::new(),
        }
    }

    fn permits_submission(&self, submission: &IntakeSubmission) -> bool {
        self.channel_id == submission.request.channel_id
            && self.request_owner == submission.request.request_owner
    }
}

#[derive(Debug)]
pub(crate) enum IntakeAdmission {
    #[cfg(unix)]
    ConsentHeld(
        Box<
            Result<super::herdr_consent::admission::Hold, super::herdr_consent::admission::Refusal>,
        >,
    ),
    Local(LocalAdmissionPermit),
    Forwarded {
        target_instance_id: String,
        outbox_id: i64,
    },
    SkippedDuplicate,
    ConsumedToHold,
    /// The replay check could not decide; the input waits in durable custody instead of starting.
    ReplayDeferred(replay_admission::ReplayDefer),
    DeferredOpenRoute {
        target_instance_id: String,
        prepared_uploads: crate::services::cluster::attachment_transfer::uploads::PendingUploads,
    },
    Blocked {
        reason: IntakeBlockedReason,
    },
    /// A Codex takeover turn still runs on Legacy; the input waits queued for turn mode, with its
    /// attachments made portable or the reason they could not be.
    DeferredTurnMode(
        Result<crate::services::cluster::attachment_transfer::uploads::PendingUploads, String>,
    ),
}

#[cfg(all(test, unix))]
impl IntakeAdmission {
    pub(in crate::services::discord) fn is_local_for_tests(&self) -> bool {
        matches!(self, Self::Local(_))
    }
}

/// Without an activation supplier the replay check keeps the legacy hold lookup and its outcomes.
fn replay_starts_dormant(channel_id: &str) -> bool {
    replay_admission::dormant_start(channel_id).is_some()
}

/// The legacy hold lookup: a lookup failure or a mixed input blocks instead of waiting in custody.
async fn dormant_hold_admission(
    pool: &sqlx::PgPool,
    submission: &IntakeSubmission,
    channel_id: &str,
) -> Option<IntakeAdmission> {
    let mut sources: Vec<String> = std::iter::once(submission.request.user_msg_id)
        .chain(submission.request.source_message_ids.iter().copied())
        .map(|id| id.get().to_string())
        .collect();
    sources.sort_unstable();
    sources.dedup();
    // Consume every absorbed source before routing or attachment effects; mixed items stay intact.
    let detail = match crate::db::replay_disposition::unblocked_sources(
        pool,
        submission.provider.as_str(),
        channel_id,
        &sources,
    )
    .await
    {
        Ok(unblocked) if unblocked.len() == sources.len() => None,
        Ok(unblocked) if unblocked.is_empty() => return Some(IntakeAdmission::ConsumedToHold),
        Ok(_) => Some("intake mixes held and new sources".to_string()),
        Err(error) => Some(format!("replay hold lookup failed: {error}")),
    };
    detail.map(|detail| IntakeAdmission::Blocked {
        reason: IntakeBlockedReason::RoutingDependencyFailed { detail },
    })
}

/// Makes the Discord attachments portable and validates every bundle the input would carry.
async fn portable_uploads(
    pool: &sqlx::PgPool,
    submission: &IntakeSubmission,
    channel_id: &str,
) -> Result<
    (
        crate::services::cluster::attachment_transfer::uploads::PendingUploads,
        Vec<crate::services::cluster::attachment_transfer::uploads::BundleRef>,
    ),
    IntakeBlockedReason,
> {
    let mut prepared_uploads = Vec::new();
    if !submission.attachments.is_empty() {
        let identity = crate::services::cluster::attachment_transfer::AttachmentMessageIdentity {
            provider: submission.provider.as_str().to_string(),
            channel_id: channel_id.to_string(),
            user_msg_id: submission.request.user_msg_id.get().to_string(),
        };
        prepared_uploads =
            message_handler::prepare_portable_attachments(pool, identity, &submission.attachments)
                .await
                .map_err(|detail| IntakeBlockedReason::AttachmentUnavailable { detail })?;
    }
    let attachment_refs: Vec<_> = prepared_uploads
        .iter()
        .chain(&submission.preloaded_uploads)
        .filter_map(|upload| match upload {
            crate::services::cluster::attachment_transfer::uploads::Upload::Bundle(reference) => {
                Some(reference.clone())
            }
            _ => None,
        })
        .collect();
    let validated = crate::services::cluster::attachment_transfer::store::validate_refs(
        pool,
        &attachment_refs,
        submission.provider.as_str(),
        channel_id,
    )
    .await;
    validated.map_err(|error| match error {
        crate::services::cluster::attachment_transfer::store::ReadError::Rejected(detail) => {
            IntakeBlockedReason::AttachmentUnavailable { detail }
        }
        error => IntakeBlockedReason::RoutingDependencyFailed {
            detail: error.to_string(),
        },
    })?;
    Ok((prepared_uploads, attachment_refs))
}

/// The attachments a turn-mode wait queues; without Postgres they cannot be made portable.
pub(in crate::services::discord::router) async fn turn_mode_uploads(
    deps: &IntakeDeps<'_>,
    submission: &IntakeSubmission,
    channel_id: &str,
) -> Result<crate::services::cluster::attachment_transfer::uploads::PendingUploads, String> {
    if submission.attachments.is_empty() {
        return Ok(Vec::new());
    }
    let pool = deps
        .shared
        .pg_pool
        .as_ref()
        .ok_or("no Postgres for portable attachments")?;
    let prepared = portable_uploads(pool, submission, channel_id).await;
    prepared
        .map(|(uploads, _)| uploads)
        .map_err(|reason| format!("{reason:?}"))
}

pub(crate) async fn admit_text_intake(
    deps: &IntakeDeps<'_>,
    submission: &IntakeSubmission,
) -> IntakeAdmission {
    #[cfg(unix)]
    if let Some(hold) =
        super::herdr_consent::admission::check(deps, &submission.provider, &submission.request)
            .await
    {
        return IntakeAdmission::ConsentHeld(Box::new(hold));
    }
    let channel_id = submission.request.channel_id.get().to_string();
    if let Some(pool) = deps.shared.pg_pool.as_ref()
        && replay_starts_dormant(&channel_id)
    {
        if let Some(admission) = dormant_hold_admission(pool, submission, &channel_id).await {
            return admission;
        }
    } else if let Some(pool) = deps.shared.pg_pool.as_ref() {
        let request = &submission.request;
        let input = replay_admission::discord_input(
            submission.provider.as_str(),
            request.channel_id.get(),
            request.user_msg_id.get(),
            request.source_message_ids.iter().map(|id| id.get()),
            &request.user_text,
        );
        let instance =
            crate::services::cluster::node_registry::resolve_self_instance_id_without_config();
        let ctx = replay_admission::ReplayAdmissionContext::production(
            pool,
            &instance,
            replay_admission::incarnation(),
        )
        .for_channel(&channel_id);
        // Consume every absorbed source before routing or attachment effects; mixed items stay intact.
        match replay_admission::preflight_source(&ctx, &input).await {
            replay_admission::ReplayPreflight::LegacyUnprotected
            | replay_admission::ReplayPreflight::Admit(_) => {}
            replay_admission::ReplayPreflight::ConsumeProtected(_) => {
                return IntakeAdmission::ConsumedToHold;
            }
            replay_admission::ReplayPreflight::Defer(defer) => {
                return IntakeAdmission::ReplayDeferred(defer);
            }
        }
    }
    // Before any routing or Postgres-less local fallback: an O channel runs only on its gateway.
    let (provider, destination) = (submission.provider.as_str(), submission.request.channel_id);
    if let IntakeRoute::Hold(detail) =
        intake_route::for_input_placement(provider, destination.get()).await
    {
        if crate::services::tui_o::cutover::codex_turn_awaited(destination.get()).is_some() {
            return IntakeAdmission::DeferredTurnMode(
                turn_mode_uploads(deps, submission, &channel_id).await,
            );
        }
        let reason = IntakeBlockedReason::RoutingDependencyFailed { detail };
        return IntakeAdmission::Blocked { reason };
    }
    let effective_config =
        crate::services::cluster::intake_router_hook::effective_intake_routing_config();
    let mode = effective_config.mode;
    let user_msg_id = submission.request.user_msg_id.get().to_string();
    let authority_channel_opt_in = effective_config.owner_authority_channel_opt_in(&channel_id);
    let Some(pool) = deps.shared.pg_pool.as_ref() else {
        let decision = if matches!(
            mode,
            crate::services::cluster::intake_router_hook::IntakeRoutingMode::Enforce
        ) || crate::config::load_graceful().cluster.enabled
        {
            IntakeRouterDecision::Blocked {
                reason: IntakeBlockedReason::RoutingDependencyFailed {
                    detail: "Postgres pool unavailable for owner lookup".to_string(),
                },
            }
        } else {
            IntakeRouterDecision::RanLocal {
                reason: if matches!(
                    mode,
                    crate::services::cluster::intake_router_hook::IntakeRoutingMode::Disabled
                ) {
                    crate::services::cluster::intake_router_hook::RanLocalReason::HookDisabled
                } else {
                    crate::services::cluster::intake_router_hook::RanLocalReason::DbErrorFellBackToLocal {
                        detail: "Postgres pool unavailable for owner-aware planning".to_string(),
                    }
                },
            }
        };
        crate::services::cluster::intake_routing_telemetry::record_decision(
            mode,
            &channel_id,
            &user_msg_id,
            authority_channel_opt_in,
            &decision,
        );
        return match decision {
            IntakeRouterDecision::Blocked { reason } => IntakeAdmission::Blocked { reason },
            IntakeRouterDecision::RanLocal { .. } => {
                IntakeAdmission::Local(LocalAdmissionPermit::for_submission(submission))
            }
            _ => unreachable!("dependency-free admission creates only blocked or local decisions"),
        };
    };

    let policy_channel_id = match policy_channel::resolve(
        deps,
        pool,
        submission.request.channel_id,
        submission.request.dm_hint == Some(true),
    )
    .await
    {
        Ok(channel) => channel,
        Err(detail) => {
            return IntakeAdmission::Blocked {
                reason: IntakeBlockedReason::RoutingDependencyFailed { detail },
            };
        }
    };
    let (prepared_uploads, attachment_refs) =
        match portable_uploads(pool, submission, &channel_id).await {
            Ok(prepared) => prepared,
            Err(reason) => return IntakeAdmission::Blocked { reason },
        };
    let request = &submission.request;
    let self_instance_id =
        crate::services::cluster::node_registry::resolve_self_instance_id_without_config();
    let request_owner_id = request.request_owner.get().to_string();
    let node_override =
        super::super::commands::channel_node_override(deps.shared, request.channel_id);
    let turn_kind = match request.turn_kind {
        super::TurnKind::Foreground => "foreground",
        super::TurnKind::BackgroundTrigger => "background_trigger",
    };
    let replay_sources = checkpoint_waits_for_admission(request.channel_id).then(|| {
        replay_admission::discord_input(
            submission.provider.as_str(),
            request.channel_id.get(),
            request.user_msg_id.get(),
            request.source_message_ids.iter().map(|source| source.get()),
            &request.user_text,
        )
        .sources
    });
    let routed_reply_context =
        super::response_format::retry_prompt_context::RetryPromptContext::for_routing(
            request.reply_context.clone(),
            matches!(submission.origin, IntakeOrigin::QueuedDrain(_)),
        );
    let ctx = IntakeRouterContext {
        mode,
        leader_instance_id: &self_instance_id,
        provider: submission.provider.as_str(),
        channel_id: &channel_id,
        policy_channel_id: &policy_channel_id,
        user_msg_id: &user_msg_id,
        replay_source_message_ids: replay_sources.as_deref(),
        request_owner_id: &request_owner_id,
        request_owner_name: Some(&request.request_owner_name),
        user_text: &request.user_text,
        reply_context: routed_reply_context.as_deref(),
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
            || submission
                .preloaded_uploads
                .iter()
                .any(|upload| upload.is_local()),
        attachment_refs: &attachment_refs,
    };

    let decision = try_route_intake(pool, &ctx).await;
    crate::services::cluster::intake_routing_telemetry::record_decision(
        mode,
        &channel_id,
        &user_msg_id,
        authority_channel_opt_in,
        &decision,
    );
    let stale_recovery = match &decision {
        IntakeRouterDecision::DeferredOpenRoute {
            open_route_id: Some(route_id),
            open_route_status,
            open_route_age_secs: Some(age_secs),
            resolved_owner: ResolvedSessionOwner::LiveLocal,
            ..
        } if matches!(authority_channel_opt_in, OwnerAuthorityChannelOptIn::NotOptedIn)
            && *open_route_status == Some(IntakeOutboxStatus::Pending)
            && *age_secs as u64 >= effective_config.forward_pre_claim_timeout_secs =>
        {
            match crate::services::cluster::intake_router_hook::owner_record::
                retire_stale_pending_route_for_local(
                    pool,
                    submission.provider.as_str(),
                    &channel_id,
                    *route_id,
                    effective_config.forward_pre_claim_timeout_secs,
                )
                .await
            {
                Ok(retired) => retired,
                Err(error) => {
                    tracing::warn!(%error, route_id, "[intake_dispatch] stale route retirement failed; retaining fence");
                    false
                }
            }
        }
        _ => true,
    };
    let mut admission = if !stale_recovery {
        match decision {
            IntakeRouterDecision::DeferredOpenRoute {
                target_instance_id, ..
            } => IntakeAdmission::DeferredOpenRoute {
                target_instance_id,
                prepared_uploads: Vec::new(),
            },
            other => admission_for_decision(
                authority_channel_opt_in,
                effective_config.forward_pre_claim_timeout_secs,
                other,
                submission,
            ),
        }
    } else {
        admission_for_decision(
            authority_channel_opt_in,
            effective_config.forward_pre_claim_timeout_secs,
            decision,
            submission,
        )
    };

    match &mut admission {
        IntakeAdmission::Local(permit) => permit.prepared_uploads = prepared_uploads,
        IntakeAdmission::DeferredOpenRoute {
            prepared_uploads: uploads,
            ..
        } => *uploads = prepared_uploads,
        _ => {}
    }
    log_nonlocal_admission(&admission, &channel_id, &user_msg_id);
    admission
}

fn admission_for_decision(
    authority_channel_opt_in: OwnerAuthorityChannelOptIn,
    forward_pre_claim_timeout_secs: u64,
    decision: IntakeRouterDecision,
    submission: &IntakeSubmission,
) -> IntakeAdmission {
    match decision {
        IntakeRouterDecision::RanLocal { .. } | IntakeRouterDecision::Observed { .. } => {
            IntakeAdmission::Local(LocalAdmissionPermit::for_submission(submission))
        }
        IntakeRouterDecision::Forwarded {
            target_instance_id,
            outbox_id,
            ..
        } => IntakeAdmission::Forwarded {
            target_instance_id,
            outbox_id,
        },
        IntakeRouterDecision::SkippedDuplicate { .. } => IntakeAdmission::SkippedDuplicate,
        // A pending row has not crossed the worker claim boundary; claimed,
        // accepted, and spawned rows may already be executing and remain
        // fenced. Local recovery retires only a stale pending row before
        // execution begins. The retirement CAS relies on the pending status
        // predicate and PostgreSQL row lock, not advisory-lock coverage.
        IntakeRouterDecision::DeferredOpenRoute {
            target_instance_id,
            open_route_status,
            open_route_age_secs,
            resolved_owner: ResolvedSessionOwner::LiveLocal,
            ..
        } if open_route_status == Some(IntakeOutboxStatus::Pending)
            && open_route_age_secs.is_some_and(|age| age >= forward_pre_claim_timeout_secs)
            && matches!(
                authority_channel_opt_in,
                OwnerAuthorityChannelOptIn::NotOptedIn
            ) =>
        {
            tracing::warn!(
                %target_instance_id,
                ?open_route_status,
                authority_channel_opt_in = authority_channel_opt_in.as_str(),
                "[intake_dispatch] admitted local live owner despite pending open route; stale-route recovery exception"
            );
            IntakeAdmission::Local(LocalAdmissionPermit::for_submission(submission))
        }
        IntakeRouterDecision::DeferredOpenRoute {
            target_instance_id, ..
        } => IntakeAdmission::DeferredOpenRoute {
            target_instance_id,
            prepared_uploads: Vec::new(),
        },
        IntakeRouterDecision::Blocked { reason } => IntakeAdmission::Blocked { reason },
    }
}

/// Common convenience path for producers that do not already own a durable
/// queued Intervention. Queue drains call `admit_text_intake` first so they can
/// preserve the original item before any marker/card teardown.
pub(crate) async fn dispatch_text_intake(
    deps: &IntakeDeps<'_>,
    submission: IntakeSubmission,
) -> Result<(), super::super::Error> {
    let admission = admit_text_intake(deps, &submission).await;
    finish_text_intake_admission(deps, admission, submission).await
}

/// Only active replay starts defer a live input, so only they hold its checkpoint until custody.
pub(crate) fn checkpoint_waits_for_admission(channel_id: serenity::ChannelId) -> bool {
    #[cfg(unix)]
    if super::herdr_consent::endpoint(channel_id.get()).is_some() {
        return true;
    }
    !replay_starts_dormant(&channel_id.get().to_string())
}

/// A turn-selected Codex channel may queue a live input for turn mode, so its checkpoint waits too.
pub(crate) fn live_checkpoint_waits(
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
) -> bool {
    checkpoint_waits_for_admission(channel_id)
        || (*provider == ProviderKind::Codex
            && crate::services::tui_o::cutover::turn_selected(channel_id.get()))
}

/// Advances the checkpoint before any effect, except that a deferred input advances it only once
/// the queue holds it. Boxed so the caller's frame stays the size of a plain dispatch.
pub(crate) fn dispatch_live_text_intake<'a, 'b: 'a>(
    deps: &'a IntakeDeps<'b>,
    submission: IntakeSubmission,
    checkpoint: impl FnOnce() + Send + 'a,
) -> BoxedIntake<'a> {
    Box::pin(async move {
        let admission = admit_text_intake(deps, &submission).await;
        #[cfg(all(test, unix))]
        super::herdr_consent::admission::tests::after_preflight(&submission).await;
        let waits = admission_waits_for_custody(&admission);
        #[cfg(unix)]
        let waits =
            waits || super::herdr_consent::endpoint(submission.request.channel_id.get()).is_some();
        if !waits {
            checkpoint();
            return finish_text_intake_admission(deps, admission, submission).await;
        }
        finish_deferred(deps, admission, submission, checkpoint).await
    })
}

/// An attachment the gateway did not admit locally. Only a turn-mode deferral moves the checkpoint,
/// once the queue holds it; every other outcome leaves the checkpoint where it was.
pub(crate) async fn finish_nonlocal_attachment_admission(
    deps: &IntakeDeps<'_>,
    admission: IntakeAdmission,
    submission: IntakeSubmission,
) -> Result<(), super::super::Error> {
    let waits = matches!(admission, IntakeAdmission::DeferredTurnMode(_));
    #[cfg(unix)]
    let waits = waits || matches!(admission, IntakeAdmission::ConsentHeld(_));
    if !waits {
        return finish_text_intake_admission(deps, admission, submission).await;
    }
    let (provider, request) = (submission.provider.clone(), &submission.request);
    let (channel_id, message_id) = (request.channel_id, request.user_msg_id);
    let checkpoint = move || {
        super::super::advance_last_message_checkpoint(
            deps.shared,
            &provider,
            channel_id,
            message_id,
        );
    };
    finish_deferred(deps, admission, submission, checkpoint).await
}

fn admission_waits_for_custody(admission: &IntakeAdmission) -> bool {
    #[cfg(unix)]
    if matches!(admission, IntakeAdmission::ConsentHeld(_)) {
        return true;
    }
    matches!(
        admission,
        IntakeAdmission::ReplayDeferred(_) | IntakeAdmission::DeferredTurnMode(_)
    )
}

/// A deferred input advances its checkpoint only after the queue durably holds it.
async fn finish_deferred(
    deps: &IntakeDeps<'_>,
    admission: IntakeAdmission,
    submission: IntakeSubmission,
    checkpoint: impl FnOnce(),
) -> Result<(), super::super::Error> {
    finish_text_intake_admission(deps, admission, submission).await?;
    checkpoint();
    Ok(())
}

pub(crate) async fn finish_text_intake_admission(
    deps: &IntakeDeps<'_>,
    admission: IntakeAdmission,
    mut submission: IntakeSubmission,
) -> Result<(), super::super::Error> {
    match admission {
        #[cfg(unix)]
        IntakeAdmission::ConsentHeld(hold) => {
            super::herdr_consent::admission::hold_live(deps, submission, *hold).await?;
        }
        IntakeAdmission::Local(permit) => {
            finish_admitted_local(deps, permit, submission).await?;
        }
        IntakeAdmission::DeferredOpenRoute {
            prepared_uploads, ..
        } => {
            submission.preloaded_uploads.extend(prepared_uploads);
            defer_live_submission(deps, submission, false).await;
        }
        IntakeAdmission::DeferredTurnMode(prepared) => {
            let why = match prepared {
                Ok(uploads) => {
                    submission.preloaded_uploads.extend(uploads);
                    submission.attachments.clear();
                    "Codex turn mode is awaited".to_string()
                }
                Err(detail) => format!("Codex turn mode is awaited; attachments: {detail}"),
            };
            hold_in_custody(deps, submission, why).await?;
        }
        IntakeAdmission::ReplayDeferred(defer) => {
            let why = format!("replay check deferred ({defer:?})");
            hold_in_custody(deps, submission, why).await?;
        }
        IntakeAdmission::Blocked { ref reason }
            if submission.origin.should_notify_blocked(reason) =>
        {
            notify_blocked_intake(deps, &submission, reason).await;
        }
        IntakeAdmission::Forwarded { .. }
        | IntakeAdmission::SkippedDuplicate
        | IntakeAdmission::ConsumedToHold
        | IntakeAdmission::Blocked { .. } => {}
    }
    Ok(())
}

pub(crate) async fn finish_admitted_text_intake(
    deps: &IntakeDeps<'_>,
    permit: LocalAdmissionPermit,
    submission: IntakeSubmission,
) -> Result<(), super::super::Error> {
    finish_admitted_local(deps, permit, submission)
        .await
        .map(|_| ())
}

pub(crate) async fn finish_admitted_local(
    deps: &IntakeDeps<'_>,
    permit: LocalAdmissionPermit,
    submission: IntakeSubmission,
) -> Result<super::IntakeExecution, super::super::Error> {
    if !permit.permits_submission(&submission) {
        tracing::error!(
            admitted_channel_id = permit.channel_id.get(),
            submitted_channel_id = submission.request.channel_id.get(),
            admitted_request_owner = permit.request_owner.get(),
            submitted_request_owner = submission.request.request_owner.get(),
            "local admission permit does not match intake submission"
        );
        return Ok(super::IntakeExecution::ConsumedProtected);
    }
    #[cfg(unix)]
    if let Some(hold) =
        super::herdr_consent::admission::check(deps, &submission.provider, &submission.request)
            .await
    {
        if matches!(submission.origin, IntakeOrigin::QueuedDrain(_)) {
            return Err(Box::new(super::herdr_consent::admission::QueuedHold(hold)));
        }
        super::herdr_consent::admission::hold_live(deps, submission, hold).await?;
        return Ok(super::IntakeExecution::Deferred);
    }
    let preserve_on_cancel = submission.preserve_on_cancel;
    local_turn(deps, submission, preserve_on_cancel).await
}

type BoxedIntake<'a, T = ()> = std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<T, super::super::Error>> + Send + 'a>,
>;

/// Boxed inside a plain function so the turn's large future never sits in the intake frames above it.
fn local_turn<'a, 'b: 'a>(
    deps: &'a IntakeDeps<'b>,
    submission: IntakeSubmission,
    preserve_on_cancel: bool,
) -> BoxedIntake<'a, super::IntakeExecution> {
    #[cfg(all(test, unix))]
    if let Some(result) = super::herdr_consent::admission::tests::final_local(&submission) {
        return Box::pin(async move { result });
    }
    let (queued_drain, queued) = match submission.origin {
        IntakeOrigin::QueuedDrain(route) => (true, route),
        _ => (false, None),
    };
    Box::pin(message_handler::finish_admitted_local(
        deps,
        submission.request,
        preserve_on_cancel,
        queued_drain,
        queued,
        submission.preloaded_uploads,
        submission.voice_announcement,
    ))
}

/// Queues a deferred input with every absorbed source; an Err leaves its checkpoint unread.
async fn hold_in_custody(
    deps: &IntakeDeps<'_>,
    submission: IntakeSubmission,
    why: String,
) -> Result<(), super::super::Error> {
    let channel_id = submission.request.channel_id.get();
    // Unprepared Discord attachments cannot ride the queue item; refuse rather than keep text only.
    let unprepared = !submission.attachments.is_empty();
    if unprepared || !defer_with_all_sources(deps, submission).await {
        return Err(format!("{why}; channel {channel_id} refused custody").into());
    }
    Ok(())
}

/// Boxed inside a plain function: a second inline enqueue future would grow the caller's stack frame.
fn defer_with_all_sources<'a, 'b: 'a>(
    deps: &'a IntakeDeps<'b>,
    submission: IntakeSubmission,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>> {
    Box::pin(defer_live_submission(deps, submission, true))
}

/// Queues a live input behind the slow backstop, with every absorbed source when asked; true once
/// the queue durably holds it.
pub(in crate::services::discord::router) async fn defer_live_submission(
    deps: &IntakeDeps<'_>,
    submission: IntakeSubmission,
    all_sources: bool,
) -> bool {
    if matches!(submission.origin, IntakeOrigin::QueuedDrain(_)) {
        return true;
    }
    let request = submission.request;
    let channel_id = request.channel_id;
    let mut intervention = super::response_format::build_race_requeued_intervention(
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
    let absorbed = request.source_message_ids.iter().copied();
    for source in absorbed.filter(|&source| all_sources && source != request.user_msg_id) {
        let generation = intervention.queued_generation;
        let queued = match submission.preserve_on_cancel {
            true => {
                crate::services::turn_orchestrator::SourceMessageQueuedGeneration::user_instruction(
                    source, generation,
                )
            }
            false => crate::services::turn_orchestrator::SourceMessageQueuedGeneration::new(
                source, generation,
            ),
        };
        intervention.source_message_ids.push(source);
        intervention.source_message_queued_generations.push(queued);
    }
    #[cfg(unix)]
    if super::herdr_consent::endpoint(channel_id.get()).is_some() {
        let origin = if intervention.voice_announcement.is_some() {
            "Voice".into()
        } else {
            format!("{:?}", submission.origin)
        };
        for source in &mut intervention.source_message_queued_generations {
            source.origin = Some(origin.clone());
        }
    }
    let outcome = super::super::queue_io::with_post_enqueue_idle_queue_kick_suppressed(
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
    let duplicate = matches!(
        outcome.refusal_reason,
        Some(
            crate::services::turn_orchestrator::EnqueueRefusalReason::SourceIdAlreadyQueued
                | crate::services::turn_orchestrator::EnqueueRefusalReason::LastItemDedup
        )
    );
    (outcome.enqueued || outcome.merged || duplicate) && outcome.persistence_error.is_none()
}

fn log_nonlocal_admission(admission: &IntakeAdmission, channel_id: &str, user_msg_id: &str) {
    match admission {
        #[cfg(unix)]
        IntakeAdmission::ConsentHeld(_) => {
            tracing::info!(channel_id, user_msg_id, "consent keeps original custody")
        }
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
        IntakeAdmission::ReplayDeferred(defer) => tracing::warn!(
            ?defer,
            channel_id,
            user_msg_id,
            "[intake_dispatch] replay check deferred; local execution fenced"
        ),
        IntakeAdmission::SkippedDuplicate | IntakeAdmission::ConsumedToHold => tracing::info!(
            channel_id,
            user_msg_id,
            "[intake_dispatch] duplicate skipped; local execution fenced"
        ),
        IntakeAdmission::DeferredOpenRoute {
            target_instance_id, ..
        } => tracing::info!(
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
        IntakeAdmission::DeferredTurnMode(_) => tracing::info!(
            channel_id,
            user_msg_id,
            "[intake_dispatch] queued until Codex turn mode is confirmed"
        ),
        IntakeAdmission::Local(_) => {}
    }
}
