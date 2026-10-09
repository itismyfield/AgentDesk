//! Actor-side barrier and lock-held queue mutations for protected channels.
use super::*;
use crate::services::discord::input_runtime::fence::{self, Closing, Failure, Permit};
use std::io;
pub(crate) struct FreezeAck {
    channel: u64,
    provider: ProviderKind,
    closing: Arc<Closing>,
}
impl FreezeAck {
    pub(crate) fn channel(&self) -> u64 {
        self.channel
    }
    pub(crate) fn matches(&self, closing: &Closing) -> bool {
        std::ptr::eq(self.closing.as_ref(), closing)
    }
    pub(crate) fn provider(&self) -> &ProviderKind {
        &self.provider
    }
}
impl ChannelMailboxHandle {
    pub(crate) async fn freeze_input(
        &self,
        closing: Arc<Closing>,
        persistence: QueuePersistenceContext,
    ) -> Result<FreezeAck, Failure> {
        self.request(|reply| ChannelMailboxMsg::FreezeInput {
            closing,
            persistence,
            reply,
        })
        .await
        .map_err(|_| Failure::ActorUnreachable)?
    }
}
pub(super) fn freeze(
    state: &mut ChannelMailboxState,
    channel: ChannelId,
    closing: &Arc<Closing>,
    persistence: &QueuePersistenceContext,
) -> Result<FreezeAck, Failure> {
    if closing.channel() != channel.get()
        || closing.provider() != &persistence.provider
        || state.closed
        || state.cancel_token.is_some()
        || state.pending_user_dispatch.is_some()
        || state.pending_user_dispatch_lease.is_some()
    {
        return Err(Failure::Busy);
    }
    fence::require_worker()?;
    let root = fence::population_root().ok_or(Failure::Persistence)?;
    let guard = closing.population(&root)?;
    let tokens = root
        .join("discord_pending_queue")
        .join(persistence.provider.as_str());
    match std::fs::read_dir(&tokens) {
        Ok(entries) => {
            for entry in entries {
                let token = entry.map_err(|_| Failure::Persistence)?.path();
                if token.file_name().and_then(|n| n.to_str()) == Some(&persistence.token_hash) {
                    continue;
                }
                for extension in ["json", "dispatch"] {
                    match std::fs::symlink_metadata(token.join(format!(
                        "{}.{}",
                        channel.get(),
                        extension
                    ))) {
                        Ok(_) => return Err(Failure::Busy),
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                        Err(_) => return Err(Failure::Persistence),
                    }
                }
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(_) => return Err(Failure::Persistence),
    }
    let _scope = fence::PopulationScope::hold(guard);
    super::pending_queue_persistence::load_channel_pending_queue_checked(
        &persistence.provider,
        &persistence.token_hash,
        channel,
    )
    .map_err(|_| Failure::Persistence)?;
    let marker = root
        .join("discord_pending_queue")
        .join(persistence.provider.as_str())
        .join(&persistence.token_hash)
        .join(format!("{}.dispatch", channel.get()));
    match std::fs::read(marker) {
        Ok(bytes) => {
            serde_json::from_slice::<super::pending_queue_persistence::PendingQueueItem>(&bytes)
                .map_err(|_| Failure::Persistence)?;
            return Err(Failure::Busy);
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(_) => return Err(Failure::Persistence),
    }
    let result = hydrate_pending_queue_from_disk_if_present(state, channel, persistence)
        .persistence_error
        .map_or_else(
            || persist_queue(channel, &state.intervention_queue, persistence),
            Err,
        )
        .map_err(|_| Failure::Persistence);
    if state.pending_user_dispatch.is_some() || state.pending_user_dispatch_lease.is_some() {
        return Err(Failure::Busy);
    }
    result.map(|_| FreezeAck {
        channel: channel.get(),
        provider: persistence.provider.clone(),
        closing: closing.clone(),
    })
}
/// Protected and not LegacyOpen: restart drains and markers leave the channel as it is.
pub(crate) fn held(provider: &ProviderKind, channel: u64) -> bool {
    fence::lookup(provider, channel).is_some_and(|gate| gate.mode() != fence::Mode::LegacyOpen)
}
pub(super) struct StepGuard {
    _population: Option<fence::PopulationScope>,
    _effect: Option<fence::effect::WorkerScope>,
    _permit: Option<Permit>,
}
fn persistence(msg: &ChannelMailboxMsg) -> Option<&QueuePersistenceContext> {
    use ChannelMailboxMsg as M;
    match msg {
        M::TryStartTurn { persistence, .. } => persistence.as_ref(),
        M::Enqueue { persistence, .. }
        | M::HasPendingSoftQueue { persistence, .. }
        | M::TakeNextSoft { persistence, .. }
        | M::RequeueFront { persistence, .. }
        | M::AbandonPendingDispatch { persistence, .. }
        | M::CancelQueuedPrimaryMessage { persistence, .. }
        | M::FinishTurn { persistence, .. }
        | M::FinishTurnIfToken { persistence, .. }
        | M::FinishTurnIfMatches { persistence, .. }
        | M::Clear { persistence, .. }
        | M::PurgeQueue { persistence, .. }
        | M::HydratePendingQueueFromDisk { persistence, .. }
        | M::MergeRestoredQueueItems { persistence, .. }
        | M::MergeRestoredDispatchMarker { persistence, .. }
        | M::RestartDrain { persistence, .. } => Some(persistence),
        M::Injection(injection) => injection.persistence(),
        #[cfg(test)]
        M::ReplaceQueue { persistence, .. } => Some(persistence),
        _ => None,
    }
}
pub(super) fn enter(
    channel: ChannelId,
    state: &ChannelMailboxState,
    msg: &mut ChannelMailboxMsg,
) -> Result<StepGuard, Failure> {
    use ChannelMailboxMsg as M;
    let Some(gate) = persistence(msg)
        .or(state.last_persistence.as_ref())
        .map_or_else(
            || fence::channel_gate(channel.get()),
            |context| fence::lookup(&context.provider, channel.get()),
        )
    else {
        return Ok(StepGuard {
            _population: None,
            _effect: None,
            _permit: None,
        });
    };
    if matches!(
        msg,
        M::Snapshot { .. }
            | M::HasActiveTurn { .. }
            | M::HasBlockingActiveTurn { .. }
            | M::ActiveTurnKind { .. }
            | M::CancelToken { .. }
            | M::FreezeInput { .. }
            | M::CommitCapturedReadyDelivery { .. }
    ) {
        return Ok(StepGuard {
            _population: None,
            _effect: None,
            _permit: None,
        });
    }
    fence::require_worker()?;
    let permit = match msg {
        M::Enqueue { input_permit, .. } | M::RequeueFront { input_permit, .. } => {
            input_permit.take()
        }
        M::Injection(injection) => injection.take_permit(),
        _ => None,
    }
    .or_else(fence::effect::current);
    let permit = match permit {
        Some(permit) => {
            permit.validate(gate.provider(), channel.get())?;
            permit
        }
        None => gate.admit()?,
    };
    let mut population = None;
    if let Some(context) = persistence(msg).or(state.last_persistence.as_ref()) {
        let root = fence::population_root().ok_or(Failure::Persistence)?;
        population = Some(fence::PopulationScope::writer(
            &root,
            &context.provider,
            channel.get(),
            &permit,
        )?);
    }
    Ok(StepGuard {
        _population: population,
        _effect: Some(fence::effect::worker_scope(Some(permit.clone()))),
        _permit: Some(permit),
    })
}
pub(super) fn enqueue_reason(failure: Failure) -> EnqueueRefusalReason {
    match failure {
        Failure::Mode(mode) => EnqueueRefusalReason::InputModeFenced(mode),
        Failure::ActorUnreachable => EnqueueRefusalReason::ActorUnreachable,
        Failure::LockTimeout => EnqueueRefusalReason::LockTimeout,
        _ => EnqueueRefusalReason::InputPersistence,
    }
}
pub(super) fn refuse(state: &ChannelMailboxState, msg: ChannelMailboxMsg, failure: Failure) {
    use ChannelMailboxMsg as M;
    let reason = enqueue_reason(failure);
    tracing::warn!(
        ?failure,
        "input fence refused mailbox mutation; responsibility retained"
    );
    match msg {
        M::Enqueue { reply, .. } => {
            let _ = reply.send(EnqueueInterventionResult::refused(reason, Vec::new()));
        }
        M::RequeueFront { reply, .. } => {
            let _ = reply.send(RequeueInterventionResult {
                enqueued: false,
                refusal_reason: Some(reason),
                queue_exit_events: Vec::new(),
                persistence_error: None,
            });
        }
        M::RecoveryKickoff { reply, .. } => {
            let _ = reply.send(match failure {
                Failure::Mode(mode) => RecoveryKickoffResult::InputModeFenced(mode),
                _ => RecoveryKickoffResult::InputFailure(failure),
            });
        }
        M::TryStartTurn { reply, .. } => {
            let _ = reply.send(TryStartTurnResult {
                persistence_error: Some(format!("input fence: {failure:?}")),
                ..Default::default()
            });
        }
        M::ClearRecoveryMarker { reply } => {
            reply.input_refuse(failure);
        }
        M::TakeNextSoft { reply, .. } => {
            reply.input_refuse(failure);
        }
        M::Clear { reply, .. } => {
            reply.input_refuse(failure);
        }
        M::HydratePendingQueueFromDisk { reply, .. }
        | M::MergeRestoredQueueItems { reply, .. }
        | M::MergeRestoredDispatchMarker { reply, .. } => {
            reply.input_refuse(failure);
        }
        M::HasPendingSoftQueue { reply, .. } => {
            let _ = reply.send(HasPendingSoftQueueResult {
                has_pending: false,
                queue_exit_events: Vec::new(),
                persistence_error: Some(format!("input fence: {failure:?}")),
            });
        }
        M::FinishTurn { reply, .. }
        | M::FinishTurnIfMatches { reply, .. }
        | M::HardStop { reply }
        | M::FinishCancelledTurn { reply } => {
            let _ = reply.send(FinishTurnResult {
                removed_token: None,
                has_pending: false,
                mailbox_online: true,
                queue_exit_events: Vec::new(),
                persistence_error: Some(format!("input fence: {failure:?}")),
            });
        }
        M::FinishTurnIfToken { reply, .. } => {
            let _ = reply.send(TokenFinish::Unavailable);
        }
        M::CancelQueuedPrimaryMessage { reply, .. } => {
            let _ = reply.send(CancelQueuedMessageResult {
                removed: None,
                queue_exit_events: Vec::new(),
                persistence_error: Some(format!("input fence: {failure:?}")),
            });
        }
        M::RestartDrain { reply, .. } => {
            let _ = reply.send(RestartDrainResult {
                queued_count: 0,
                persistence_error: Some(format!("input fence: {failure:?}")),
            });
        }
        M::AbandonPendingDispatch { reply, .. }
        | M::CancelActiveBackgroundTurnIfCurrent { reply } => {
            let _ = reply.send(false);
        }
        M::CancelActiveTurnWithReason { reply, .. }
        | M::CancelActiveTurnIfCurrent { reply, .. }
        | M::CancelActiveTurnIfCurrentWithReason { reply, .. }
        | M::CancelActiveTurnIfUserMessageWithReason { reply, .. } => {
            let _ = reply.send(CancelActiveTurnResult {
                token: None,
                already_stopping: false,
            });
        }
        M::CancelActiveTurnIfCurrentUnlessHerdr { reply, .. } => {
            let _ = reply.send(StopCancel::NotCurrent);
        }
        M::CloseIfIdle { reply } => {
            let _ = reply.send(Err("input-mode-fenced"));
        }
        M::Injection(injection) => {
            injection.refuse(format!("input fence: {failure:?}"));
        }
        M::PurgeQueue { reply, .. } => {
            let _ = reply.send(PurgeQueueResult {
                input_refusal: Some(failure),
                queue_len_after: state.intervention_queue.len(),
                ..Default::default()
            });
        }
        M::RestoreActiveTurn { reply, .. } => {
            drop(reply);
        }
        #[cfg(test)]
        M::ReplaceQueue { reply, .. }
        | M::AgeActiveTurnForTest { reply, .. }
        | M::AgeInboundWaitsForTest { reply, .. }
        | M::AgeValveClearedDispatchForTest { reply, .. } => {
            drop(reply);
        }
        M::Snapshot { .. }
        | M::HasActiveTurn { .. }
        | M::HasBlockingActiveTurn { .. }
        | M::ActiveTurnKind { .. }
        | M::CancelToken { .. }
        | M::FreezeInput { .. }
        | M::CommitCapturedReadyDelivery { .. } => {
            unreachable!("read/barrier arms are not refused by enter")
        }
    }
}
#[cfg(test)]
#[path = "input_fence_tests.rs"]
mod tests;
