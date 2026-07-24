use super::*;

impl TurnViewReconciler {
    pub(super) async fn note_state(
        &self,
        shared: &SharedData,
        target: TurnViewTarget,
        owner: TurnViewOwner,
        identity: TurnViewIdentity,
        desired: TurnViewState,
        source: &'static str,
    ) -> bool {
        self.note_state_delivery(shared, target, owner, identity, desired, source)
            .await
            .delivered()
    }

    pub(super) async fn note_state_delivery(
        &self,
        shared: &SharedData,
        target: TurnViewTarget,
        owner: TurnViewOwner,
        identity: TurnViewIdentity,
        desired: TurnViewState,
        source: &'static str,
    ) -> TurnViewDelivery {
        let (delivery, _) = self
            .note_state_delivery_with_attempt(shared, target, owner, identity, desired, source)
            .await;
        delivery
    }

    pub(super) async fn note_state_delivery_with_attempt(
        &self,
        shared: &SharedData,
        target: TurnViewTarget,
        owner: TurnViewOwner,
        identity: TurnViewIdentity,
        desired: TurnViewState,
        source: &'static str,
    ) -> (TurnViewDelivery, Option<TurnStartAttempt>) {
        self.note_state_delivery_with_clear_attempt_guard(
            shared, target, owner, identity, desired, None, source,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn note_state_delivery_with_clear_attempt_guard(
        &self,
        shared: &SharedData,
        target: TurnViewTarget,
        owner: TurnViewOwner,
        identity: TurnViewIdentity,
        desired: TurnViewState,
        clear_start_attempt: Option<TurnStartAttempt>,
        source: &'static str,
    ) -> (TurnViewDelivery, Option<TurnStartAttempt>) {
        let start_attempt = self.start_attempt_for(desired);
        if !reaction_lifecycle::is_real_discord_message_id(target.message_id) {
            tracing::debug!(
                channel_id = target.channel_id.get(),
                message = target.message_id.get(),
                target_kind = ?target.kind,
                desired = ?desired,
                source,
                "turn view reaction skipped for non-Discord/synthetic message id"
            );
            return (TurnViewDelivery::Delivered, None);
        }

        let target_lock = self.target_lock(target);
        let _target_guard = target_lock.lock().await;

        if desired.is_queue_marker()
            && self.recently_finalized_blocks_queued(target, owner.generation)
            && !queue_repair::allows(shared, target, None, source).await
        {
            tracing::debug!(
                channel_id = target.channel_id.get(),
                message = target.message_id.get(),
                target_kind = ?target.kind,
                source,
                queued_generation = owner.generation,
                "turn view queued notification ignored for recently finalized generation"
            );
            return (TurnViewDelivery::Delivered, None);
        }

        let current = self
            .targets
            .get(&target)
            .map(|entry| entry.clone())
            .or_else(|| self.load_persisted_target(target, shared, source));
        if let Some(current) = current.as_ref() {
            if desired == TurnViewState::None
                && clear_start_attempt.is_none()
                && !current.legacy_queue_reactions.is_empty()
            {
                let delivery = self
                    .remove_legacy_queue_reactions(
                        shared,
                        target,
                        &current.legacy_queue_reactions,
                        &current.identity,
                        source,
                    )
                    .await;
                if delivery.delivered() || matches!(delivery, TurnViewDelivery::FailedPermanent) {
                    self.discard_target_locked(target, source, &target_lock);
                }
                return (delivery, None);
            }
            if desired == TurnViewState::None
                && let Some(clear_start_attempt) = clear_start_attempt
                && (current.owner != owner
                    || current.applied != TurnViewState::Pending
                    || current.start_attempt != Some(clear_start_attempt))
            {
                tracing::debug!(
                    channel_id = target.channel_id.get(),
                    message = target.message_id.get(),
                    target_kind = ?target.kind,
                    source,
                    current_state = ?current.applied,
                    current_generation = current.owner.generation,
                    current_turn_id = %current.owner.turn_id,
                    current_start_attempt = current.start_attempt.map(TurnStartAttempt::get),
                    clear_generation = owner.generation,
                    clear_turn_id = %owner.turn_id,
                    clear_start_attempt = clear_start_attempt.get(),
                    "turn view attempt-scoped clear ignored because current state is not the matching pending start attempt"
                );
                self.targets.insert(target, current.clone());
                return (TurnViewDelivery::Delivered, None);
            }
            if current.owner == owner
                && desired.is_queue_marker()
                && current.applied.started_or_terminal()
                && !queue_repair::allows(shared, target, Some(current.applied), source).await
            {
                tracing::debug!(
                    channel_id = target.channel_id.get(),
                    message = target.message_id.get(),
                    target_kind = ?target.kind,
                    source,
                    current_state = ?current.applied,
                    "turn view queued notification ignored after target already started"
                );
                if current.applied.terminal() {
                    self.finalize_target_locked(
                        target,
                        current.owner.generation,
                        source,
                        &target_lock,
                    );
                } else {
                    self.targets.insert(target, current.clone());
                }
                return (TurnViewDelivery::Delivered, None);
            }
            if current.owner != owner {
                if desired == TurnViewState::Pending
                    && current.applied == TurnViewState::Queued
                    && current.owner.turn_id == owner.turn_id
                {
                    // Restart generation handoff: the same queued message may
                    // be promoted by a fresh dcserver generation. This remains
                    // monotonic (`Queued` -> `Pending`) and preserves the
                    // original reaction identity for the mailbox removal.
                } else if desired == TurnViewState::Pending || desired.is_queue_marker() {
                    if current.applied == desired {
                        let transferred = Self::applied_target(
                            owner,
                            current.applied,
                            current.identity.clone(),
                            start_attempt,
                        );
                        self.targets.insert(target, transferred.clone());
                        self.persist_target(target, &transferred, shared, source);
                        return (TurnViewDelivery::Delivered, transferred.start_attempt);
                    }
                } else {
                    tracing::debug!(
                        channel_id = target.channel_id.get(),
                        message = target.message_id.get(),
                        target_kind = ?target.kind,
                        desired = ?desired,
                        source,
                        current_generation = current.owner.generation,
                        current_turn_id = %current.owner.turn_id,
                        stale_generation = owner.generation,
                        stale_turn_id = %owner.turn_id,
                        "turn view reaction notification ignored for stale owner"
                    );
                    if current.applied.terminal() {
                        self.finalize_target_locked(
                            target,
                            current.owner.generation,
                            source,
                            &target_lock,
                        );
                    }
                    return (TurnViewDelivery::Delivered, None);
                }
            }
            if current.applied == desired {
                if desired == TurnViewState::Pending {
                    let attempt = self.update_matching_pending_attempt(
                        shared,
                        target,
                        owner,
                        current,
                        start_attempt,
                        source,
                    );
                    return (TurnViewDelivery::Delivered, attempt);
                }
                self.targets.insert(target, current.clone());
                if desired.terminal() {
                    self.finalize_target_locked(
                        target,
                        current.owner.generation,
                        source,
                        &target_lock,
                    );
                }
                return (TurnViewDelivery::Delivered, None);
            }
        }

        let resolved_identity = match current.as_ref() {
            Some(current) => current.identity.clone(),
            None => match self.resolve_identity(shared, target.kind, identity, source) {
                Some(identity) => identity,
                None => return (TurnViewDelivery::Failed, None),
            },
        };

        if current.is_none() && desired == TurnViewState::None {
            let delivery = self
                .apply_unknown_clear(shared, target, &resolved_identity, source)
                .await;
            if delivery.delivered() {
                self.discard_target_locked(target, source, &target_lock);
            }
            return (delivery, None);
        }

        let applied = current
            .as_ref()
            .map(|entry| entry.applied)
            .unwrap_or_else(|| {
                if desired.terminal() {
                    TurnViewState::Pending
                } else {
                    TurnViewState::None
                }
            });

        let delivery = self
            .apply_diff_or_cold_terminal(
                shared,
                target,
                applied,
                desired,
                current.is_none(),
                current
                    .as_ref()
                    .map(|entry| entry.legacy_queue_reactions.as_slice())
                    .unwrap_or_default(),
                &resolved_identity,
                source,
            )
            .await;
        if !delivery.delivered() {
            if matches!(delivery, TurnViewDelivery::FailedPermanent) {
                self.discard_target_locked(target, source, &target_lock);
            }
            return (delivery, None);
        }
        if desired == TurnViewState::None {
            self.discard_target_locked(target, source, &target_lock);
        } else if desired.terminal() {
            let finalized_generation = owner.generation;
            let applied_target = Self::applied_target(owner, desired, resolved_identity, None);
            self.targets.insert(target, applied_target);
            self.finalize_target_locked(target, finalized_generation, source, &target_lock);
        } else {
            let applied_target =
                Self::applied_target(owner, desired, resolved_identity, start_attempt);
            self.targets.insert(target, applied_target.clone());
            self.persist_target(target, &applied_target, shared, source);
        }
        (TurnViewDelivery::Delivered, start_attempt)
    }
}
