//! A Codex source an Unknown bind left `Pending{[0]}` is owed from 0 once its execution's own
//! fresh launch context proves it is that execution's first new source and nothing read it before.

use super::*;
use crate::services::codex_tui::execution_end::identity;
use crate::services::tmux_common::current_tmux_owner_marker;
use crate::services::tui_o::store::ChannelStore;
use crate::services::tui_prompt_dedupe::binding_context::execution_context;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Fresh {
    Proven,
    /// The native file does not prove it yet; a longer capture asks again.
    Later,
    /// Nothing this process sees can prove it.
    Refused,
}

/// The boundary an Unknown bind with no parent gives a source.
pub(super) fn held(boundary: &Boundary) -> bool {
    *boundary
        == (Boundary::Pending {
            candidates: vec![0],
        })
}

/// Whether two names may be one file: same path, and the same session or, with either session
/// empty, the same inode.
fn names_file(bound: &SourceId, source: &SourceId) -> bool {
    let unnamed = bound.session_id.is_empty() || source.session_id.is_empty();
    let session = bound.session_id == source.session_id || (unnamed && bound.ino == source.ino);
    session && bound.path == source.path
}

fn bound_targets(event: &BindingEvent, raw: &[BindingEvent]) -> Option<Vec<SourceId>> {
    match &event.record {
        BindingRecord::Bound {
            new: BindingTarget::Source(source),
            ..
        }
        | BindingRecord::Resolved { source, .. } => Some(vec![source.clone()]),
        BindingRecord::Bound {
            new: BindingTarget::Pending { .. },
            ..
        } => {
            let resolved = raw.iter().filter_map(|later| match &later.record {
                BindingRecord::Resolved {
                    resolves_seq,
                    source,
                } if *resolves_seq == event.seq => Some(source.clone()),
                _ => None,
            });
            let resolved: Vec<_> = resolved.collect();
            (!resolved.is_empty()).then_some(resolved)
        }
        BindingRecord::Rejected { .. } => Some(Vec::new()),
    }
}

impl<B: BindingEvents> Sources<B> {
    /// A stored link the Codex judgment may decide: only a held start no resolution names yet.
    fn codex_fresh_candidate(&self, store: &ChannelStore, link: &SourceLink) -> bool {
        self.provider == ShadowProvider::Codex
            && held(&link.boundary)
            && link.parent.is_none()
            && link.hot_candidate.is_none()
            && self.resume_selected(&link.source)
            && store.ledger().boundary_resolved(&link.source).is_none()
    }

    /// Judges `source`, bound at `seq`, against the raw log. `cursor` is its stored capture, whose
    /// prefix must still read as spooled; a source not yet attached has none.
    pub(super) fn codex_fresh_start(
        &self,
        store: &ChannelStore,
        source: &SourceId,
        seq: u64,
        raw: &[BindingEvent],
        cursor: Option<&Cursor>,
    ) -> Fresh {
        let stored = |bound: &SourceId| self.names.stored_name(store, bound);
        let Some(event) = raw.iter().find(|event| event.seq == seq) else {
            return Fresh::Refused;
        };
        let unknown = matches!(
            &event.record,
            BindingRecord::Bound {
                cause: BindingCause::Unknown,
                ..
            }
        );
        let names_source = bound_targets(event, raw)
            .is_some_and(|targets| targets.iter().any(|t| stored(t).as_ref() == Some(source)));
        if self.provider != ShadowProvider::Codex
            || !unknown
            || !names_source
            || event.channel_id != self.channel
            || event.provider != ShadowProvider::Codex
            || event.tmux_session.is_empty()
            || event.execution_nonce.is_empty()
        {
            return Fresh::Refused;
        }
        let Ok(context) = execution_context("codex", &event.execution_nonce) else {
            return Fresh::Refused;
        };
        if context.channel_id != Some(self.channel)
            || context.tmux_session != event.tmux_session
            || context.launch_mode != "fresh"
            || context.expected_native_session_id.is_some()
            || context.first_prompt_digest.is_none()
            || context.owner_runtime_root != current_tmux_owner_marker()
        {
            return Fresh::Refused;
        }
        // An earlier record of this execution may only name this source or one bound before it ran.
        let mut execution = raw
            .iter()
            .filter(|e| e.execution_nonce == event.execution_nonce);
        let Some(first) = execution.next() else {
            return Fresh::Refused;
        };
        let before = |other: &SourceId| match self.rotation.link(other) {
            Some(link) => link.seq < first.seq,
            None => {
                let init = store.init();
                init.at < first.committed_at && init.sources.iter().any(|s| s.source_id == *other)
            }
        };
        let earlier = std::iter::once(first).chain(execution);
        for prior in earlier.take_while(|e| e.seq < seq) {
            let Some(targets) = bound_targets(prior, raw) else {
                return Fresh::Refused;
            };
            for target in targets {
                let Some(target) = stored(&target) else {
                    return Fresh::Refused;
                };
                if target != *source && !before(&target) {
                    return Fresh::Refused;
                }
            }
        }
        // Any earlier name of this file, attached or not, means its records may be old.
        let named = raw
            .iter()
            .filter(|e| e.seq < seq)
            .filter_map(bound_source)
            .any(|bound| names_file(bound, source));
        if named {
            return Fresh::Refused;
        }
        let mut native = source.clone();
        if let Some(cursor) = cursor {
            // The stored prefix must still be this file before its current dev is trusted.
            let Ok(capture) = reopen(cursor) else {
                return Fresh::Refused;
            };
            let Ok(fingerprint) = capture.fingerprint() else {
                return Fresh::Later;
            };
            if fingerprint.ino != source.ino {
                return Fresh::Refused;
            }
            native.dev = fingerprint.dev;
        }
        match identity::verify_source(&context, &native) {
            Ok(_) => Fresh::Proven,
            Err(_) => Fresh::Later,
        }
    }

    /// Resolves each proven Codex `Pending{[0]}` link through the ledger, deriving its retained
    /// spool when `replay`; resume derives it with the rest of the spool instead.
    pub(super) fn settle_codex_fresh<P: DiscordPort, L: DeliveryLease, A: AlarmSink>(
        &mut self,
        writer: &mut ChannelWriter<P, L, A>,
        deriver: &mut UnitDeriver,
        owed: &mut impl Owe,
        replay: bool,
    ) -> Result<(), WriterAlarm> {
        if self.provider != ShadowProvider::Codex {
            return Ok(());
        }
        let store = writer.store();
        let due: Vec<(SourceLink, Cursor)> = (self.rotation.links.values())
            .filter(|link| self.codex_fresh_candidate(store, link))
            .filter_map(|link| {
                let cursor = store.cursor(&link.source)?;
                let tried = self.codex_fresh_tried.get(&source_key(&link.source));
                tried
                    .is_none_or(|through| cursor.captured_through > *through)
                    .then(|| (link.clone(), cursor.clone()))
            })
            .collect();
        if due.is_empty() {
            return Ok(());
        }
        self.flush(writer)?;
        let Some((raw, _)) = self.read_log(writer, 0)? else {
            return Ok(());
        };
        for (link, cursor) in due {
            let key = source_key(&link.source);
            let judged =
                self.codex_fresh_start(writer.store(), &link.source, link.seq, &raw, Some(&cursor));
            match judged {
                Fresh::Proven => {}
                Fresh::Later => {
                    self.codex_fresh_tried.insert(key, cursor.captured_through);
                    continue;
                }
                Fresh::Refused => {
                    self.codex_fresh_tried.insert(key, u64::MAX);
                    continue;
                }
            }
            writer
                .store()
                .resolve_codex_fresh_boundary(&link.source, link.seq)
                .map_err(halted("Codex fresh boundary"))?;
            self.rotation = writer.store().rotation().map_err(halted("rotation"))?;
            tracing::info!(
                channel = self.channel,
                seq = link.seq,
                source = %link.source.path.display(),
                "[tui_o] fresh Codex source owed from 0"
            );
            if replay {
                writer
                    .store()
                    .for_each_frame(&link.source, |frame, identity_version| {
                        if let SpoolFrame::Record(record) = frame {
                            self.owe(deriver, owed, &link.source, &record, identity_version);
                        }
                    })
                    .map_err(halted("Codex fresh boundary replay"))?;
            }
        }
        Ok(())
    }
}
