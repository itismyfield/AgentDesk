//! Source selection keeps suspended cursors out of readers and preserves quiescent rebuilds.

use super::*;
use crate::services::tui_o::store::ChannelStore;

impl<B: BindingEvents> Sources<B> {
    pub(crate) fn with_suspension(
        mut self,
        suspension: super::super::suspension::Suspension,
    ) -> Self {
        self.suspension = suspension;
        self
    }

    pub(crate) fn quiescent_for_rescope(&self) -> bool {
        self.validation.is_none()
            && self.refined.is_empty()
            && !self.dirty
            && self.readers.iter().all(|reader| {
                reader.pending.is_none()
                    && reader.drain_to.is_none()
                    && reader
                        .capture
                        .as_ref()
                        .is_none_or(|capture| capture.read_through() == capture.captured_through())
            })
    }

    /// The actor checks quiescence before replacing readers; the shared selection stays monotone.
    pub(crate) fn reset_for_rescope(&mut self) {
        let config = self.reader_config;
        let suspension = self.suspension.clone();
        *self = Self::new(self.channel, self.provider, Arc::clone(&self.bindings))
            .with_suspension(suspension);
        self.reader_config = config;
    }

    pub(super) fn resume_selected(&self, source: &SourceId) -> bool {
        #[cfg(test)]
        if crate::services::claude_tui::source_verify::n2b_mutant("b1b-resume") {
            return true;
        }
        !self.suspension.contains(source)
    }

    pub(super) fn guard_selected_reference(
        &self,
        store: &ChannelStore,
        source: &SourceId,
    ) -> Result<(), WriterAlarm> {
        if self.suspension.is_empty() {
            return Ok(());
        }
        let stored = self
            .names
            .stored_name(store, source)
            .ok_or_else(|| halt("ambiguous scoped source identity"))?;
        if self.suspension.contains(&stored) {
            return Err(halt("binding refers to a suspended source"));
        }
        Ok(())
    }

    pub(super) fn check_scoped_metadata(&self, store: &ChannelStore) -> Result<(), WriterAlarm> {
        if self.suspension.is_empty() {
            return Ok(());
        }
        if store
            .binding_checkpoint()
            .map_err(halted("binding checkpoint"))?
            .is_none()
            && store.cursors().next().is_some()
        {
            return Err(halt("scoped source store has no binding checkpoint"));
        }
        for link in store.rotation().map_err(halted("rotation"))?.links.values() {
            if self.suspension.contains(&link.source) {
                if link.hot_candidate.is_some() {
                    return Err(halt("suspended source has an unresolved content boundary"));
                }
                continue;
            }
            for parent in link
                .parent
                .iter()
                .chain(link.hot_candidate.iter().map(|c| &c.parent))
            {
                self.guard_selected_reference(store, parent)?;
            }
        }
        Ok(())
    }

    /// Marks each unproven hop of the record's pane that it shows was left, durably before the
    /// checkpoint passes the record.
    pub(super) fn prove<P: DiscordPort, L: DeliveryLease, A: AlarmSink>(
        &mut self,
        writer: &mut ChannelWriter<P, L, A>,
        event: &BindingEvent,
    ) -> Result<(), WriterAlarm> {
        let Some(session) = proving_session(event) else {
            return Ok(());
        };
        let mut proved = Vec::new();
        for (key, next) in &mut self.rotation.successors {
            let made = match (next.seq, next.tmux_session.as_deref()) {
                (Some(seq), Some(tmux)) => Some((seq, tmux)),
                _ => (self.legacy_hops.get(key)).map(|(seq, tmux)| (*seq, tmux.as_str())),
            };
            let old = self.readers.iter().find(|r| source_key(&r.source) == *key);
            let left = old.is_some_and(|old| self.names.session(&old.source) != session);
            let ours =
                made.is_some_and(|(seq, tmux)| seq <= event.seq && tmux == event.tmux_session);
            if next.proof.is_none() && ours && left {
                next.proof = Some(event.seq);
                proved.push(key.clone());
            }
        }
        if !proved.is_empty() {
            let written = writer.store().write_rotation(&self.rotation);
            written.map_err(halted("rotation"))?;
            self.wake_proven(writer, proved)?;
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "scoped_tests.rs"]
mod tests;
