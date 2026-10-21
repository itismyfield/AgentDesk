//! Which earlier anchor a recovery delivery may reuse, read only under its captured identity.

use super::*;

impl RecoveryDeliveryContext {
    pub(in crate::services::discord) fn anchor_reuse_decision(
        &self,
    ) -> Option<RecoveryAnchorReuse> {
        if !self.reuse_recorded_anchor {
            return None;
        }
        if let Some(anchor) = self.durable_recorded_anchor().and_then(opt_message_id) {
            return Some(RecoveryAnchorReuse::DurableAlreadyDelivered(anchor));
        }
        self.inflight_recorded_anchor()
            .and_then(opt_message_id)
            .map(RecoveryAnchorReuse::InflightAnchor)
    }

    #[cfg(test)]
    pub(in crate::services::discord) fn recorded_anchor(&self) -> Option<MessageId> {
        match self.anchor_reuse_decision() {
            Some(RecoveryAnchorReuse::DurableAlreadyDelivered(anchor))
            | Some(RecoveryAnchorReuse::InflightAnchor(anchor)) => Some(anchor),
            None => None,
        }
    }

    pub(super) fn durable_recorded_anchor(&self) -> Option<u64> {
        let range = self.durable_range?;
        let tmux_session_name = self.tmux_session_name.as_deref()?;
        let anchor = delivery_frontier_probe::current_generation_delivered_anchor(
            &self.provider,
            self.record_channel_id,
            tmux_session_name,
            self.current_output_eof,
        )?;
        (anchor.panel_channel_id == self.channel_id.get() && anchor.range == range)
            .then_some(anchor.panel_msg_id)
    }

    pub(super) fn inflight_recorded_anchor(&self) -> Option<u64> {
        inflight::recovery_anchor_msg_id_if_matches_identity(
            &self.provider,
            self.channel_id.get(),
            &self.identity,
            self.expected_turn_start_offset,
        )
    }
}
