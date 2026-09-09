//! Durable-intent state transitions. Runtime side effects live in the watchdog.

use super::*;

impl RecoveryRuntime {
    pub(super) fn binding_for_channel(&self, channel: &str) -> Option<ChannelRecoveryBinding> {
        if let Some(state) = self.states.get(channel).filter(|state| state.lock_held()) {
            return state.context.as_ref()?.binding(state);
        }
        self.catalog.channels.get(channel).cloned()
    }

    pub(super) fn writer_provider(
        &self,
        state: &ChannelState,
        agent: &str,
    ) -> Option<ProviderKind> {
        state
            .context
            .as_ref()
            .and_then(|context| context.provider(state, agent))
    }

    pub(super) fn checked_lease(
        &self,
        lease: &RecoveryLease,
    ) -> Result<&ChannelState, RecoveryStoreError> {
        self.states
            .get(&lease.channel_id)
            .filter(|state| {
                state.generation == lease.generation
                    && state.active_writer_agent_id == lease.active_writer_agent_id
            })
            .ok_or_else(|| RecoveryStoreError::Conflict("stale recovery launch lease".into()))
    }

    pub(super) fn acknowledge_start(
        &mut self,
        lease: &RecoveryLease,
    ) -> Result<Option<()>, RecoveryStoreError> {
        let status = self.checked_lease(lease)?.status;
        let (next, kind) = match status {
            ChannelRecoveryStatus::TakeoverPending => (
                ChannelRecoveryStatus::FallbackRunning,
                CheckpointEventKind::Stall,
            ),
            ChannelRecoveryStatus::RestorePending => (
                ChannelRecoveryStatus::Restored,
                CheckpointEventKind::Restore,
            ),
            // A very short turn may complete before start() returns.
            ChannelRecoveryStatus::FallbackDone | ChannelRecoveryStatus::Restored => {
                return Ok(None);
            }
            _ => return Err(RecoveryStoreError::Conflict("launch is not pending".into())),
        };
        self.append_event(
            &lease.channel_id,
            &lease.active_writer_agent_id,
            kind,
            CheckpointPayload::compact(
                &lease.active_writer_agent_id,
                "",
                "runtime start acknowledged",
                "",
                Vec::new(),
                "",
                "",
            ),
        )
        .map_err(|error| RecoveryStoreError::Conflict(error.message()))?;
        self.states
            .get_mut(&lease.channel_id)
            .expect("checked lease")
            .status = next;
        if next == ChannelRecoveryStatus::Restored {
            self.open_keys
                .retain(|(channel, _)| channel != &lease.channel_id);
        }
        Ok(Some(()))
    }

    pub(super) fn retry_interrupted_launch(
        &mut self,
        channel: &str,
    ) -> Result<Option<()>, RecoveryStoreError> {
        let Some(state) = self.states.get(channel) else {
            return Ok(None);
        };
        let next = match state.status {
            ChannelRecoveryStatus::FallbackRunning | ChannelRecoveryStatus::TakeoverPending => {
                ChannelRecoveryStatus::TakeoverPending
            }
            ChannelRecoveryStatus::RestorePending => ChannelRecoveryStatus::RestorePending,
            _ => return Ok(None),
        };
        let writer = state.active_writer_agent_id.clone();
        self.append_event(
            channel,
            &writer,
            CheckpointEventKind::Stall,
            CheckpointPayload::compact(
                &writer,
                "",
                "interrupted launch; reconcile before retry",
                "",
                Vec::new(),
                "reconcile runtime",
                "",
            ),
        )
        .map_err(|error| RecoveryStoreError::Conflict(error.message()))?;
        let state = self.states.get_mut(channel).expect("state checked above");
        state.status = next;
        state.generation += 1;
        Ok(Some(()))
    }
}
