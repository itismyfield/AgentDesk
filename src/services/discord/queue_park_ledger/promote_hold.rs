use super::{ChannelId, ChannelMailboxSnapshot, ProviderKind, QueueParkLedger};

impl QueueParkLedger {
    #[cfg(unix)]
    pub(in crate::services::discord) fn note_codex_verified_hold(
        &self,
        channel: ChannelId,
        tmux: &str,
    ) {
        let reason = crate::services::tui_prompt_dedupe::codex_verified_hold_detail(tmux);
        self.note_promote_hold(&ProviderKind::Codex, channel, "codex_verified", reason);
    }

    pub(in crate::services::discord) fn note_session_transition_hold(
        &self,
        provider: &ProviderKind,
        channel: ChannelId,
    ) {
        self.note_promote_hold(provider, channel, "session_transition", "lock_busy");
    }

    pub(super) fn note_promote_hold(
        &self,
        provider: &ProviderKind,
        channel: ChannelId,
        owner: &'static str,
        reason: &'static str,
    ) {
        let mut state = self
            .channels
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if state.promote_holds.insert((channel, owner), reason) == Some(reason) {
            return;
        }
        tracing::info!(target: "agentdesk::discord::queue_park",
            provider = provider.as_str(), channel_id = channel.get(),
            queue_park_owner = owner, hold_reason = reason, "queued promotion held");
    }

    pub(in crate::services::discord) fn clear_promote_hold(
        &self,
        channel: ChannelId,
        owner: &'static str,
    ) {
        self.channels
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .promote_holds
            .remove(&(channel, owner));
    }

    pub(super) fn promote_hold(
        &self,
        channel: ChannelId,
        snapshot: &ChannelMailboxSnapshot,
    ) -> Option<(String, &'static str)> {
        if snapshot.intervention_queue.is_empty() {
            return None;
        }
        // Project the last gate observation; re-probing source permission can mutate binding state.
        let state = self
            .channels
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        for owner in ["codex_verified", "session_transition"] {
            if let Some(reason) = state.promote_holds.get(&(channel, owner)) {
                return Some((format!("{owner}_held:{reason}"), owner));
            }
        }
        None
    }
}
