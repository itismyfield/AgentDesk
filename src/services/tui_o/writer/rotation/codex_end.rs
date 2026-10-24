//! A sealed execution end proves only its recorded old-to-fresh edge, including after restore.

use super::*;
use crate::services::codex_tui::execution_end::{identity, mutant, producer};
use crate::services::tmux_common;
use crate::services::tui_o::store::ChannelStore;
use crate::services::tui_o::store::codex_end::{CodexEnd, ContextSnapshot, EndBasis};
use crate::services::tui_prompt_dedupe::binding_context::{
    BindingContext, SpawnNonceMarker, execution_context, observe_spawn_nonce_marker,
    stable_host_identity,
};

struct Edge {
    old_key: String,
    old: SourceId,
    successor: SourceId,
    hop_seq: u64,
    seal_seq: u64,
}

impl<B: BindingEvents> Sources<B> {
    pub(super) fn settle_codex_ends<P: DiscordPort, L: DeliveryLease, A: AlarmSink>(
        &mut self,
        writer: &mut ChannelWriter<P, L, A>,
    ) -> Result<(), WriterAlarm> {
        if self.provider != ShadowProvider::Codex {
            return Ok(());
        }
        let panes: HashSet<_> = self
            .rotation
            .successors
            .values()
            .filter(|next| next.proof.is_none())
            .filter_map(|next| next.tmux_session.as_deref())
            .collect();
        for pane in panes.into_iter().filter(|_| !mutant("consumer_retry_off")) {
            tmux_common::with_tmux_source_authority(pane, |authority| {
                if let SpawnNonceMarker::Known(nonce) = observe_spawn_nonce_marker(pane) {
                    producer::retry_bound_candidate(self.channel, authority, &nonce);
                }
            });
        }
        let store = writer.store();
        // A preliminary read avoids re-publishing seals whose edges already hold durable proof.
        let relevant = store.codex_ends().is_ok_and(|records| {
            records
                .iter()
                .any(|record| self.codex_edge(store, record).is_some())
        });
        if !relevant {
            return Ok(());
        }
        let durable = if mutant("consumer_unsynced") {
            store.codex_ends()
        } else {
            store.durable_codex_ends()
        };
        let records = match durable {
            Ok(records) => records,
            Err(reason) => {
                tracing::debug!(
                    channel = self.channel,
                    reason,
                    "[tui_o] Codex end evidence pending"
                );
                return Ok(());
            }
        };
        let Some((_, events)) = self.read_log(writer, 0)? else {
            return Ok(());
        };
        super::super::historical_hops::validate(
            &events,
            self.checkpoint.unwrap_or(0),
            self.channel,
        )?;
        let proved: Vec<_> = records
            .iter()
            .filter_map(|record| self.match_codex_end(writer.store(), record, &events))
            .collect();
        if proved.is_empty() {
            return Ok(());
        }
        let mut rotation = self.rotation.clone();
        for edge in &proved {
            if let Some(next) = rotation.successors.get_mut(&edge.old_key) {
                next.proof = Some(edge.seal_seq);
            }
        }
        if !mutant("consumer_proof_unwritten") {
            writer
                .store()
                .write_rotation(&rotation)
                .map_err(halted("Codex end rotation"))?;
        }
        self.rotation = rotation;
        for edge in &proved {
            tracing::info!(
                channel = self.channel,
                hop_seq = edge.hop_seq,
                seal_seq = edge.seal_seq,
                "[tui_o] hop proved by a sealed Codex end"
            );
        }
        self.wake_proven(
            writer,
            proved.into_iter().map(|edge| edge.old_key).collect(),
        )
    }

    fn codex_edge(&self, store: &ChannelStore, record: &CodexEnd) -> Option<Edge> {
        let seal = record.successor.as_ref()?;
        let old = self.names.stored_name(store, &record.source)?;
        let successor = self.names.stored_name(store, &seal.source)?;
        if store.cursor(&old)?.retired
            || store.cursor(&successor).is_none()
            || !self.resume_selected(&old)
            || !self.resume_selected(&successor)
        {
            return None;
        }
        let old_key = source_key(&old);
        let next = self.rotation.successors.get(&old_key)?;
        let hop_seq = next.seq?;
        (next.proof.is_none()
            && next.source == successor
            && next.tmux_session.as_deref() == Some(record.tmux_session.as_str())
            && hop_seq > record.binding_seq
            && hop_seq <= seal.binding_seq
            && (!mutant("consumer_same_seq") || hop_seq == seal.binding_seq)
            && self
                .checkpoint
                .is_some_and(|checkpoint| seal.binding_seq <= checkpoint))
        .then_some(Edge {
            old_key,
            old,
            successor,
            hop_seq,
            seal_seq: seal.binding_seq,
        })
    }

    fn match_codex_end(
        &self,
        store: &ChannelStore,
        record: &CodexEnd,
        events: &[BindingEvent],
    ) -> Option<Edge> {
        let seal = record.successor.as_ref()?;
        let edge = self.codex_edge(store, record)?;
        if record.channel != self.channel
            || record.provider != "codex"
            || stable_host_identity().as_deref() != Some(record.host.as_str())
            || record.owner_runtime_root != tmux_common::current_tmux_owner_marker()
            || (mutant("consumer_explicit_clear_only") && record.basis != EndBasis::ExplicitClear)
        {
            return None;
        }
        let old_context = execution_context("codex", &record.execution_nonce).ok()?;
        let new_context = execution_context("codex", &seal.execution_nonce).ok()?;
        if ContextSnapshot::from(&old_context) != record.context
            || ContextSnapshot::from(&new_context) != seal.context
        {
            return None;
        }
        // Later executions are outside this sealed interval and do not invalidate its history.
        let rows: Vec<_> = events
            .iter()
            .filter(|event| {
                event.seq > record.binding_seq
                    && (event.seq <= edge.seal_seq || mutant("consumer_whole_tail"))
                    && event.tmux_session == record.tmux_session
            })
            .collect();
        let first = *rows.first()?;
        if first.seq != edge.hop_seq
            || rows.iter().any(|event| {
                event.channel_id != self.channel
                    || event.provider != ShadowProvider::Codex
                    || event.execution_nonce != seal.execution_nonce
            })
        {
            return None;
        }
        let BindingRecord::Bound {
            old: Some(old),
            new,
            ..
        } = &first.record
        else {
            return None;
        };
        if *old != edge.old {
            return None;
        }
        let bound = match new {
            BindingTarget::Source(source) => {
                rows.len() == 1 && edge.hop_seq == edge.seal_seq && *source == edge.successor
            }
            BindingTarget::Pending {
                payload_session_id,
                payload_transcript_path,
            } => {
                rows.len() == 2
                    && rows[1].seq == edge.seal_seq
                    && *payload_session_id == seal.source.session_id
                    && *payload_transcript_path == seal.source.path
                    && matches!(&rows[1].record, BindingRecord::Resolved { resolves_seq, source }
                        if *resolves_seq == edge.hop_seq && *source == edge.successor)
            }
        };
        if !bound
            || !native_source(store, &edge.old, &old_context, &record.provider_root)
            || !native_source(store, &edge.successor, &new_context, &record.provider_root)
        {
            return None;
        }
        Some(edge)
    }
}

fn native_source(
    store: &ChannelStore,
    stored: &SourceId,
    context: &BindingContext,
    root: &std::path::Path,
) -> bool {
    let Some(cursor) = store.cursor(stored) else {
        return false;
    };
    let Ok(capture) = reopen(cursor) else {
        return false;
    };
    let Ok(fingerprint) = capture.fingerprint() else {
        return false;
    };
    let mut source = stored.clone();
    // Reopen proves the stored prefix before a reboot-renumbered dev reaches native validation.
    (source.dev, source.ino) = (fingerprint.dev, fingerprint.ino);
    identity::verify_source(context, &source).is_ok_and(|observed| observed == root)
}
