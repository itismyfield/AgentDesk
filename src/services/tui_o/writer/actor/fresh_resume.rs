//! Rescope only at a new proven root, with no work or reader obligation left in flight.

use super::*;

impl<P: DiscordPort, L: DeliveryLease, A: AlarmSink, B: BindingEvents> Actor<P, L, A, B> {
    pub(super) fn rescope(&mut self) -> bool {
        if self.sources.transition_provider()
            != crate::services::tui_o::shadow::ShadowProvider::Codex
        {
            return false;
        }
        let selection = self.writer.suspension.clone();
        let nonce = selection.nonce();
        #[cfg(test)]
        if nonce.is_some() && crate::services::claude_tui::source_verify::n2b_mutant("b1b-once") {
            return false;
        }
        let channel = self.writer.channel();
        let quiescent = self.owed.is_empty()
            && !self.deriver.has_unsealed()
            && !self.writer.has_local_retry()
            && self.writer.resume_pieces().is_empty()
            && self.writer.store().ledger().unresolved().is_none()
            && !self.transitions.has_job()
            && self.sources.quiescent_for_rescope();
        #[cfg(test)]
        let quiescent =
            quiescent || crate::services::claude_tui::source_verify::n2b_mutant("b1b-refreeze-noq");
        let sources: Vec<_> = self
            .writer
            .store()
            .cursors()
            .map(|cursor| cursor.source.clone())
            .collect();
        let result = crate::services::tui_prompt_dedupe::clear_relaunch::with_fresh_scope(
            channel,
            nonce.as_deref(),
            &sources,
            quiescent,
            |nonce, denied| {
                selection.extend(nonce, denied);
                self.sources.reset_for_rescope();
            },
        );
        match result {
            Ok(changed) => {
                self.scope_hold = None;
                if changed {
                    self.deriver = UnitDeriver::new(
                        channel,
                        crate::services::tui_o::shadow::ShadowProvider::Codex,
                    );
                }
                changed
            }
            Err(reason) => {
                if self.scope_hold != Some(reason) {
                    tracing::info!(channel, reason, "[tui_o] source resume held");
                    self.scope_hold = Some(reason);
                }
                false
            }
        }
    }
}
