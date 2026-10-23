use super::*;

#[derive(Default)]
pub(super) struct Pending {
    channels: std::sync::Mutex<HashMap<ChannelId, HashMap<MessageId, TurnViewIdentity>>>,
}

impl TurnViewReconciler {
    /// Keeps only successful immediate-injection marks, with the identity that added them.
    pub(in crate::services::discord) async fn note_busy_injected(
        &self,
        shared: &SharedData,
        target: TurnViewTarget,
        identity: TurnViewIdentity,
    ) {
        if !reaction_lifecycle::is_real_discord_message_id(target.message_id) {
            return;
        }
        if self
            .note_untracked_reaction_added(
                shared,
                target,
                identity.clone(),
                '⏳',
                "busy_inject_reaction",
            )
            .await
        {
            self.injected
                .channels
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .entry(target.channel_id)
                .or_default()
                .insert(target.message_id, identity);
        }
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
                    for _ in 0..2 {
                        if shared
                            .turn_view_reconciler
                            .note_untracked_reaction(
                                &shared,
                                target,
                                identity.clone(),
                                emoji,
                                add,
                                "busy_inject_idle",
                            )
                            .await
                            .delivered()
                        {
                            break;
                        }
                    }
                }
            }
        });
    }
}
