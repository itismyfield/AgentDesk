use super::*;

#[derive(Default)]
pub(super) struct Pending {
    channels: std::sync::Mutex<HashMap<ChannelId, HashMap<MessageId, TurnViewIdentity>>>,
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
            if !self
                .note_queue_marker_removed(
                    shared,
                    target,
                    owner.clone(),
                    identity.clone(),
                    emoji,
                    "busy_inject_reaction",
                )
                .await
            {
                return;
            }
        }
        if crate::services::discord::turn_presence::supervisor::observes(
            provider,
            target.channel_id.get(),
        ) {
            self.note_busy_injected(shared, target, identity).await;
        } else {
            self.note_untracked_reaction_added(
                shared,
                target,
                identity,
                '➕',
                "busy_inject_reaction",
            )
            .await;
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
    ) {
        if !reaction_lifecycle::is_real_discord_message_id(target.message_id) {
            return;
        }
        self.note_untracked_reaction_added(
            shared,
            target,
            identity.clone(),
            '➕',
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
                for _ in 0..2 {
                    let delivery = shared
                        .turn_view_reconciler
                        .note_untracked_reaction(
                            &shared,
                            target,
                            identity.clone(),
                            '✅',
                            true,
                            "busy_inject_idle",
                        )
                        .await;
                    if delivery != TurnViewDelivery::Failed {
                        break;
                    }
                }
            }
        });
    }
}
