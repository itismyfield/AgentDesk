use super::*;

impl TuiPromptDedupeState {
    pub(super) fn record_recent_observed_prompt(
        &mut self,
        provider: &str,
        tmux_session_name: &str,
        prompt: &str,
    ) {
        self.recent_observed_by_tmux
            .entry(PromptKey::new(provider, tmux_session_name))
            .or_default()
            .push_back(TimedValue {
                value: prompt.to_string(),
                recorded_at: Instant::now(),
            });
    }

    pub(super) fn purge_expired(&mut self) {
        let now = Instant::now();
        self.pending_by_tmux.retain(|_, queue| {
            while queue
                .front()
                .is_some_and(|entry| now.duration_since(entry.recorded_at) > PENDING_PROMPT_TTL)
            {
                queue.pop_front();
            }
            !queue.is_empty()
        });
        self.recent_observed_by_tmux.retain(|_, queue| {
            while queue
                .front()
                .is_some_and(|entry| now.duration_since(entry.recorded_at) > RECENT_OBSERVED_TTL)
            {
                queue.pop_front();
            }
            !queue.is_empty()
        });
        self.channel_by_tmux
            .retain(|_, entry| now.duration_since(entry.recorded_at) <= SESSION_MAPPING_TTL);
        self.runtime_by_tmux
            .retain(|_, entry| now.duration_since(entry.recorded_at) <= SESSION_MAPPING_TTL);
        // #3885 follow-up: anchors live `PROMPT_ANCHOR_SUBMIT_TTL` (4h) so a long
        // streaming turn's anchor is not purged mid-stream (see the constant). The
        // relayed-entry ledger below intentionally keeps the 30min
        // `PROMPT_ANCHOR_TTL`.
        self.prompt_anchor_by_tmux
            .retain(|_, entry| now.duration_since(entry.recorded_at) <= PROMPT_ANCHOR_SUBMIT_TTL);
        self.ssh_direct_observation_by_tmux
            .retain(|_, entry| now.duration_since(entry.recorded_at) <= SSH_DIRECT_OBSERVATION_TTL);
        self.external_input_relay_lease_by_tmux.retain(|_, entry| {
            now.duration_since(entry.recorded_at) <= EXTERNAL_INPUT_RELAY_LEASE_TTL
        });
        self.deferred_anchor_completion_by_tmux.retain(|_, entry| {
            now.duration_since(entry.recorded_at) <= DEFERRED_ANCHOR_COMPLETION_TTL
        });
        // #3540: relayed-entry-id ledger — purge ids older than PROMPT_ANCHOR_TTL
        // (30min), long enough to span a watermark-reset / jsonl-rotation +
        // self-loop window while bounding memory growth.
        self.relayed_entry_ids_by_tmux.retain(|_, queue| {
            while queue
                .front()
                .is_some_and(|entry| now.duration_since(entry.recorded_at) > PROMPT_ANCHOR_TTL)
            {
                queue.pop_front();
            }
            !queue.is_empty()
        });
    }

    pub(super) fn remove_provider_session_mappings_for_tmux(
        &mut self,
        tmux_session_name: &str,
    ) -> bool {
        let before = self.tmux_by_provider_session.len();
        self.tmux_by_provider_session
            .retain(|_, entry| entry.value != tmux_session_name);
        before != self.tmux_by_provider_session.len()
    }
}
