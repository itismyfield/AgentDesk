//! Hops a confirmed `!clear` proves: the pane's first bind after the clear, out of the source the
//! ended execution wrote, into the session a fresh launch was prepared for.

use super::*;
use crate::services::tui_o::store::ChannelStore;
use crate::services::tui_o::store::clear_end::{ClearEnd, mutant};
use crate::services::tui_prompt_dedupe::binding_context::{
    execution_context, stable_host_identity,
};

/// What the matching rules read for one unproven hop.
struct Hop<'a> {
    key: &'a str,
    seq: u64,
    pane: &'a str,
    new_session: &'a str,
}

impl<B: BindingEvents> Sources<B> {
    /// Applies clear ends to unproven hops once something they read may have changed: a bind, a
    /// newly published record or a resume. A log or record read that fails waits for the next poll.
    pub(super) fn settle_cleared<P: DiscordPort, L: DeliveryLease, A: AlarmSink>(
        &mut self,
        writer: &mut ChannelWriter<P, L, A>,
    ) -> Result<(), WriterAlarm> {
        if self.clear_notice.has_changed().unwrap_or(false) {
            self.clear_notice.borrow_and_update();
            self.clear_due = true;
        }
        if !std::mem::take(&mut self.clear_due) || mutant("off") {
            return Ok(());
        }
        if self.provider != ShadowProvider::Claude && !mutant("any_provider") {
            return Ok(());
        }
        let records = match writer.store().clear_ends() {
            Ok(records) if !records.is_empty() => records,
            Ok(_) => return Ok(()),
            Err(detail) => {
                tracing::warn!(
                    channel = self.channel,
                    detail,
                    "[tui_o] clear ends unreadable"
                );
                return Ok(());
            }
        };
        let open = self
            .rotation
            .successors
            .iter()
            .any(|(_, n)| n.proof.is_none());
        if !open {
            return Ok(());
        }
        let Ok(events) = self.bindings.binding_events_since(self.channel, 0) else {
            self.clear_due = true;
            return Ok(());
        };
        let host = stable_host_identity();
        let store: &ChannelStore = writer.store();
        let mut proved = Vec::new();
        for (key, next) in &self.rotation.successors {
            if next.proof.is_some() {
                continue;
            }
            let made = match (next.seq, next.tmux_session.as_deref()) {
                (Some(seq), Some(pane)) => (seq, pane),
                _ if mutant("legacy_hops") => match self.legacy_hops.get(key) {
                    Some((seq, pane)) => (*seq, pane.as_str()),
                    None => continue,
                },
                _ => continue,
            };
            let (seq, pane) = made;
            let new_session = self.names.session(&next.source);
            let hop = Hop {
                key,
                seq,
                pane,
                new_session,
            };
            if self.old_settled(store, key) && !mutant("missing_old") {
                continue;
            }
            let proven = records
                .iter()
                .rev()
                .any(|record| self.proves(store, record, &hop, &events, host.as_deref()));
            if proven {
                proved.push((key.clone(), seq));
            }
        }
        if proved.is_empty() {
            return Ok(());
        }
        for (key, seq) in &proved {
            if let Some(next) = self.rotation.successors.get_mut(key) {
                next.proof = Some(*seq);
                tracing::info!(
                    channel = self.channel,
                    seq = next.seq,
                    "[tui_o] hop proved by a confirmed clear end"
                );
            }
        }
        if mutant("proof_unwritten") {
            for (key, _) in &proved {
                if let Some(next) = self.rotation.successors.get_mut(key) {
                    next.proof = None;
                }
            }
            return Ok(());
        }
        let written = writer.store().write_rotation(&self.rotation);
        written.map_err(halted("rotation"))?;
        self.wake_proven(writer, proved.into_iter().map(|(key, _)| key).collect())
    }

    /// The old source of `key` is already retired or gone, so the missing-source path owns it.
    fn old_settled(&self, store: &ChannelStore, key: &str) -> bool {
        let Some(reader) = self.readers.iter().find(|r| source_key(&r.source) == key) else {
            return true;
        };
        let retired = store.cursor(&reader.source).is_none_or(|c| c.retired);
        let missing = matches!(
            Fingerprint::path(&reader.source),
            Err((SourceAnomalyKind::Missing, _))
        );
        retired || missing
    }

    fn proves(
        &self,
        store: &ChannelStore,
        record: &ClearEnd,
        hop: &Hop<'_>,
        events: &[BindingEvent],
        host: Option<&str>,
    ) -> bool {
        let ours = record.provider == "claude"
            && record.channel == self.channel
            && record.host.as_deref() == host;
        let same_pane = record.tmux_session == hop.pane || mutant("any_pane");
        if !ours || !same_pane {
            return false;
        }
        let old = match self.names.stored_name(store, &record.source) {
            Some(stored) => source_key(&stored),
            None => return false,
        };
        if old != hop.key && !mutant("any_source") {
            return false;
        }
        let pane_moves = events.iter().filter(|event| {
            event.seq > record.binding_seq
                && event.tmux_session == hop.pane
                && matches!(
                    event.record,
                    BindingRecord::Bound { .. } | BindingRecord::Resolved { .. }
                )
        });
        let first = if mutant("any_later_hop") {
            pane_moves.clone().find(|event| event.seq == hop.seq)
        } else {
            pane_moves.clone().next()
        };
        let Some(event) = first.filter(|event| event.seq == hop.seq) else {
            return false;
        };
        let nonce = event.execution_nonce.as_str();
        if nonce.is_empty() || (nonce == record.execution_nonce && !mutant("same_nonce")) {
            return false;
        }
        self.fresh_launch(nonce, hop)
    }

    /// The bind's execution was launched fresh in this pane for exactly the hop's new session, so
    /// it cannot be one that resumed the old source without logging a bind.
    fn fresh_launch(&self, nonce: &str, hop: &Hop<'_>) -> bool {
        if mutant("context_off") {
            return true;
        }
        let Ok(context) = execution_context("claude", nonce) else {
            return false;
        };
        let fresh = context.launch_mode == "fresh" || mutant("resume_ok");
        let expected = context.expected_native_session_id.as_deref() == Some(hop.new_session)
            || mutant("any_expected");
        fresh
            && expected
            && context.tmux_session == hop.pane
            && context.channel_id == Some(self.channel)
    }

    /// Re-arms the configured reader of each newly proven hop, as a provider proof does.
    pub(super) fn wake_proven<P: DiscordPort, L: DeliveryLease, A: AlarmSink>(
        &mut self,
        writer: &mut ChannelWriter<P, L, A>,
        keys: Vec<String>,
    ) -> Result<(), WriterAlarm> {
        if self.reader_config.is_none() {
            return Ok(());
        }
        for key in keys {
            let index = self
                .readers
                .iter()
                .position(|r| source_key(&r.source) == key);
            if let Some(index) = index {
                self.readers[index].quiet = None;
                self.wake(writer, index)?;
            }
        }
        Ok(())
    }

    /// At resume, wakes every proven old source still unretired, which boot may have put to sleep.
    pub(super) fn wake_resumed_proofs<P: DiscordPort, L: DeliveryLease, A: AlarmSink>(
        &mut self,
        writer: &mut ChannelWriter<P, L, A>,
    ) -> Result<(), WriterAlarm> {
        if mutant("resume_wake_off") {
            return Ok(());
        }
        let store: &ChannelStore = writer.store();
        let proven: Vec<String> = (self.rotation.successors.iter())
            .filter(|(_, next)| next.proof.is_some())
            .map(|(key, _)| key.clone())
            .filter(|key| !self.old_settled(store, key))
            .collect();
        self.wake_proven(writer, proven)
    }
}
