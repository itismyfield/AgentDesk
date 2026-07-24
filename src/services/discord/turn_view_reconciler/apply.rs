use super::*;

impl TurnViewReconciler {
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn apply_diff_or_cold_terminal(
        &self,
        shared: &SharedData,
        target: TurnViewTarget,
        applied: TurnViewState,
        desired: TurnViewState,
        cold: bool,
        legacy_queue_reactions: &[char],
        identity: &ResolvedIdentity,
        source: &'static str,
    ) -> TurnViewDelivery {
        if cold && desired.terminal() {
            let mut delivery = self
                .apply_unknown_clear(shared, target, identity, source)
                .await;
            if !matches!(delivery, TurnViewDelivery::FailedPermanent) {
                delivery = delivery.merge(
                    self.apply_diff(
                        shared,
                        target,
                        TurnViewState::None,
                        desired,
                        identity,
                        source,
                    )
                    .await,
                );
            }
            return delivery;
        }

        if !legacy_queue_reactions.is_empty() {
            let delivery = self
                .remove_legacy_queue_reactions(
                    shared,
                    target,
                    legacy_queue_reactions,
                    identity,
                    source,
                )
                .await;
            if !delivery.delivered() {
                return delivery;
            }
        }
        self.apply_diff(shared, target, applied, desired, identity, source)
            .await
    }

    pub(super) async fn remove_legacy_queue_reactions(
        &self,
        shared: &SharedData,
        target: TurnViewTarget,
        reactions: &[char],
        identity: &ResolvedIdentity,
        source: &'static str,
    ) -> TurnViewDelivery {
        let mut removed = Vec::new();
        for emoji in reactions {
            let delivery = self
                .apply_reaction(shared, target, *emoji, false, identity, source)
                .await;
            if !delivery.delivered() {
                self.compensate_reaction_ops(shared, target, identity, source, &removed)
                    .await;
                return delivery;
            }
            removed.push((*emoji, false));
        }
        TurnViewDelivery::Delivered
    }

    pub(super) async fn apply_diff(
        &self,
        shared: &SharedData,
        target: TurnViewTarget,
        applied: TurnViewState,
        desired: TurnViewState,
        identity: &ResolvedIdentity,
        source: &'static str,
    ) -> TurnViewDelivery {
        let applied_reactions = reaction_set::for_state(applied);
        let desired_reactions = reaction_set::for_state(desired);
        let mut applied_ops = Vec::new();
        for emoji in applied_reactions {
            if desired_reactions.contains(emoji) {
                continue;
            }
            let delivery = self
                .apply_reaction(shared, target, *emoji, false, identity, source)
                .await;
            if !delivery.delivered() {
                self.compensate_reaction_ops(shared, target, identity, source, &applied_ops)
                    .await;
                return delivery;
            }
            applied_ops.push((*emoji, false));
        }
        for emoji in desired_reactions {
            if applied_reactions.contains(emoji) {
                continue;
            }
            let delivery = self
                .apply_reaction(shared, target, *emoji, true, identity, source)
                .await;
            if !delivery.delivered() {
                self.compensate_reaction_ops(shared, target, identity, source, &applied_ops)
                    .await;
                return delivery;
            }
            applied_ops.push((*emoji, true));
        }
        TurnViewDelivery::Delivered
    }

    pub(super) async fn compensate_reaction_ops(
        &self,
        shared: &SharedData,
        target: TurnViewTarget,
        identity: &ResolvedIdentity,
        source: &'static str,
        applied_ops: &[(char, bool)],
    ) {
        for (emoji, add) in applied_ops.iter().rev() {
            let compensation = self
                .apply_reaction(shared, target, *emoji, !*add, identity, source)
                .await;
            if !compensation.delivered() {
                tracing::warn!(
                    channel_id = target.channel_id.get(),
                    message = target.message_id.get(),
                    emoji = %emoji,
                    source,
                    "turn view reaction compensation failed"
                );
            }
        }
    }

    pub(super) async fn apply_unknown_clear(
        &self,
        shared: &SharedData,
        target: TurnViewTarget,
        identity: &ResolvedIdentity,
        source: &'static str,
    ) -> TurnViewDelivery {
        let mut delivery = TurnViewDelivery::Delivered;
        for emoji in TURN_VIEW_REACTIONS {
            delivery = delivery.merge(
                self.apply_reaction(shared, target, emoji, false, identity, source)
                    .await,
            );
        }
        delivery
    }

    pub(super) async fn apply_reaction(
        &self,
        shared: &SharedData,
        target: TurnViewTarget,
        emoji: char,
        add: bool,
        identity: &ResolvedIdentity,
        source: &'static str,
    ) -> TurnViewDelivery {
        debug_assert!(
            TURN_VIEW_REACTIONS.contains(&emoji) || QUEUE_EXIT_FEEDBACK_REACTIONS.contains(&emoji)
        );
        #[cfg(not(test))]
        {
            let result = if add {
                reaction_lifecycle::try_add_reaction_raw_with_shared_detailed(
                    &identity.http,
                    shared,
                    target.channel_id,
                    target.message_id,
                    emoji,
                )
                .await
            } else {
                reaction_lifecycle::try_remove_reaction_raw_with_shared_detailed(
                    &identity.http,
                    shared,
                    target.channel_id,
                    target.message_id,
                    emoji,
                )
                .await
            };
            if let Err(error) = result {
                tracing::warn!(
                    channel_id = target.channel_id.get(),
                    message = target.message_id.get(),
                    target_kind = ?target.kind,
                    identity = identity.label,
                    emoji = %emoji,
                    add,
                    source,
                    error = %error,
                    "turn view reaction apply failed"
                );
                return TurnViewDelivery::from_reaction_error_status(error.status());
            }
        }
        #[cfg(test)]
        {
            let _ = (shared, source);
            tokio::task::yield_now().await;
            let delivery = self
                .test_deliveries
                .lock()
                .expect("turn view test delivery lock")
                .pop_front()
                .unwrap_or(TurnViewDelivery::Delivered);
            self.ops
                .lock()
                .expect("turn view test op lock")
                .push(TestReactionOp {
                    target,
                    emoji,
                    add,
                    identity: identity.label.clone(),
                });
            delivery
        }
        #[cfg(not(test))]
        {
            TurnViewDelivery::Delivered
        }
    }
}
