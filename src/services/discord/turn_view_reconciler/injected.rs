use super::*;

#[derive(Default)]
pub(super) struct Pending {
    channels: std::sync::Mutex<HashMap<ChannelId, HashMap<MessageId, TurnViewIdentity>>>,
}

async fn retry_delivery<F, Fut>(mut attempt: F) -> TurnViewDelivery
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = TurnViewDelivery>,
{
    for _ in 0..2 {
        let delivery = attempt().await;
        if delivery != TurnViewDelivery::Failed {
            return delivery;
        }
    }
    TurnViewDelivery::Failed
}

impl TurnViewReconciler {
    /// Leaves an absorption receipt, with completion tracking only for supervised channels.
    pub(in crate::services::discord) async fn note_confirmed_injection(
        &self,
        shared: &SharedData,
        provider: &crate::services::provider::ProviderKind,
        target: TurnViewTarget,
        identity: TurnViewIdentity,
    ) {
        if !reaction_lifecycle::is_real_discord_message_id(target.message_id) {
            return;
        }
        retry_delivery(|| {
            self.confirmed_injection_attempt(shared, provider, target, identity.clone())
        })
        .await;
    }

    async fn confirmed_injection_attempt(
        &self,
        shared: &SharedData,
        provider: &crate::services::provider::ProviderKind,
        target: TurnViewTarget,
        identity: TurnViewIdentity,
    ) -> TurnViewDelivery {
        let current = self
            .targets
            .get(&target)
            .map(|entry| entry.clone())
            .or_else(|| self.load_persisted_target(target, shared, "busy_inject_reaction"));
        let owner = current.map(|entry| entry.owner).unwrap_or_else(|| {
            turn_view_owner_for_message(
                target.channel_id,
                target.message_id,
                shared.restart.current_generation,
            )
        });
        for emoji in queue_reactions::QUEUE_PENDING_REACTION_EMOJIS {
            let delivery = self
                .note_queue_marker_removed_delivery(
                    shared,
                    target,
                    owner.clone(),
                    identity.clone(),
                    emoji,
                    "busy_inject_reaction",
                )
                .await;
            if !delivery.delivered() {
                return delivery;
            }
        }
        if crate::services::discord::turn_presence::supervisor::observes(
            provider,
            target.channel_id.get(),
        ) {
            self.note_busy_injected(shared, target, identity).await
        } else {
            self.note_untracked_reaction(
                shared,
                target,
                identity,
                '➕',
                true,
                "busy_inject_reaction",
            )
            .await
        }
    }

    #[cfg(test)]
    pub(in crate::services::discord) fn pending_injected_count(&self, channel: ChannelId) -> usize {
        self.injected
            .channels
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&channel)
            .map_or(0, HashMap::len)
    }

    /// Remembers immediate-injection messages separately from turn and queue markers.
    pub(in crate::services::discord) async fn note_busy_injected(
        &self,
        shared: &SharedData,
        target: TurnViewTarget,
        identity: TurnViewIdentity,
    ) -> TurnViewDelivery {
        if !reaction_lifecycle::is_real_discord_message_id(target.message_id) {
            return TurnViewDelivery::Delivered;
        }
        let delivery = self
            .note_untracked_reaction(
                shared,
                target,
                identity.clone(),
                '➕',
                true,
                "busy_inject_reaction",
            )
            .await;
        self.injected
            .channels
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .entry(target.channel_id)
            .or_default()
            .insert(target.message_id, identity);
        delivery
    }

    /// Takes the list synchronously at the idle observation; later arrivals keep their own list.
    pub(in crate::services::discord) fn clear_busy_injected(
        &self,
        shared: Arc<SharedData>,
        channel: ChannelId,
    ) {
        let messages = self
            .injected
            .channels
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&channel)
            .unwrap_or_default();
        if messages.is_empty() {
            return;
        }
        super::super::task_supervisor::spawn_observed("busy_inject_reaction_cleanup", async move {
            for (message, identity) in messages {
                let target = TurnViewTarget::intake_user_message(channel, message);
                retry_delivery(|| {
                    shared.turn_view_reconciler.note_untracked_reaction(
                        &shared,
                        target,
                        identity.clone(),
                        '✅',
                        true,
                        "busy_inject_idle",
                    )
                })
                .await;
            }
        });
    }
}
