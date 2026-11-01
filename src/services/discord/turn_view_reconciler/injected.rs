use super::*;

#[derive(Default)]
pub(super) struct Pending {
    channels: std::sync::Mutex<HashMap<ChannelId, HashMap<MessageId, TurnViewIdentity>>>,
}

impl TurnViewReconciler {
    /// Uses a pending marker only when the provider can later complete it on idle.
    pub(in crate::services::discord) async fn note_confirmed_injection(
        &self,
        shared: &SharedData,
        provider: &crate::services::provider::ProviderKind,
        target: TurnViewTarget,
        identity: TurnViewIdentity,
    ) {
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
                '📥',
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
            '⏳',
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
                for (emoji, add) in [('⏳', false), ('✅', true)] {
                    let mut delivery = TurnViewDelivery::Failed;
                    for _ in 0..2 {
                        delivery = shared
                            .turn_view_reconciler
                            .note_untracked_reaction(
                                &shared,
                                target,
                                identity.clone(),
                                emoji,
                                add,
                                "busy_inject_idle",
                            )
                            .await;
                        if delivery != TurnViewDelivery::Failed {
                            break;
                        }
                    }
                    if delivery == TurnViewDelivery::FailedPermanent {
                        tracing::warn!(
                            channel_id = channel.get(),
                            message = message.get(),
                            emoji = %emoji,
                            add,
                            "discarded injected reaction after permanent failure"
                        );
                        break;
                    }
                    if delivery == TurnViewDelivery::Failed && !add {
                        shared
                            .turn_view_reconciler
                            .injected
                            .channels
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .entry(channel)
                            .or_default()
                            .entry(message)
                            .or_insert(identity);
                        break;
                    }
                }
            }
        });
    }
}
