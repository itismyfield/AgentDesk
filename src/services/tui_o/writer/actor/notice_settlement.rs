//! Record settlement removes only proven non-notice debt, independently of current pane liveness.
use super::*;
use crate::services::tui_o::shadow::SourceId;
use crate::services::tui_o::store::ledger::LedgerEntry;
use crate::services::tui_o::store::ledger::input_notices::{
    NoticeEntry, NoticeEvidence, RecordId, Settled, Settlement,
};
use crate::services::tui_o::writer::binding::{BindingEvent, BindingRecord, BindingTarget};
use sha2::Digest;

fn bound<'a>(events: &'a [BindingEvent], event: &'a BindingEvent) -> Option<(&'a SourceId, u64)> {
    let BindingRecord::Bound { new, .. } = &event.record else {
        return None;
    };
    match new {
        BindingTarget::Source(source) => Some((source, event.seq)),
        BindingTarget::Pending {
            payload_session_id,
            payload_transcript_path,
        } => {
            let mut matching = events.iter().filter_map(|e| match &e.record {
                BindingRecord::Resolved {
                    resolves_seq,
                    source,
                } if *resolves_seq == event.seq => Some((e, source)),
                _ => None,
            });
            let (resolved, source) = matching.next()?;
            if matching.next().is_some()
                || resolved.seq <= event.seq
                || resolved.channel_id != event.channel_id
                || resolved.provider != event.provider
                || resolved.tmux_session != event.tmux_session
                || resolved.execution_nonce != event.execution_nonce
                || source.session_id != *payload_session_id
                || source.path != *payload_transcript_path
                || events.iter().any(|e| {
                    e.seq > event.seq
                        && e.seq < resolved.seq
                        && e.tmux_session == event.tmux_session
                        && matches!(e.record, BindingRecord::Bound { .. })
                })
            {
                return None;
            }
            Some((source, resolved.seq))
        }
    }
}

pub(super) fn boundary(
    events: &[BindingEvent],
    origin: &super::super::input_notices::NoticeOrigin,
) -> Option<u64> {
    let original = events.iter().find(|e| e.seq == origin.binding_seq)?;
    if original.tmux_session != origin.logical_session
        || original.provider != ShadowProvider::Claude
        || original.execution_nonce != origin.execution_nonce
        || events
            .iter()
            .any(|e| e.channel_id != original.channel_id || e.provider != original.provider)
        || bound(events, original)?.0 != &origin.record.source
    {
        return None;
    }
    for event in events
        .iter()
        .filter(|e| e.seq > origin.binding_seq && e.tmux_session == origin.logical_session)
    {
        if let BindingRecord::Bound { old: Some(old), .. } = &event.record
            && old == &origin.record.source
            && let Some((source, seq)) = bound(events, event)
            && source != old
        {
            return Some(seq);
        }
    }
    None
}

impl<P: DiscordPort, L: DeliveryLease, A: AlarmSink, B: BindingEvents> Actor<P, L, A, B> {
    pub(super) fn changed_notices(&mut self) -> HashSet<RecordId> {
        let mut cache = std::mem::take(&mut self.writer.notice_cache);
        let owners = std::mem::take(&mut self.writer.notice_owners);
        let selected = cache.select(self.writer.store(), &owners);
        self.writer.notice_owners = owners;
        self.writer.notice_cache = cache;
        let mut pending = std::mem::take(&mut self.writer.notice_pending);
        let ledger = self.writer.store().ledger();
        let state = ledger.input_notices();
        pending.retain(|id, _| {
            state
                .evidence
                .get(id)
                .is_some_and(|e| !state.terminal(ledger, id, e))
        });
        self.writer.notice_pending = pending;
        selected
    }

    pub(super) fn settle_notice(&mut self, evidence: &NoticeEvidence) -> bool {
        if evidence
            .unit_key
            .as_ref()
            .is_some_and(|key| self.writer.store().ledger().latest_piece(key, 0).is_some())
        {
            return false;
        }
        let Some(prompt) = evidence.prompt.as_deref() else {
            return false;
        };
        let digest = hex::encode(sha2::Sha256::digest(prompt.as_bytes()));
        let (proof, expired_after) =
            if crate::services::discord::input_notice_content(prompt).is_none() {
                (
                    Settled::Policy {
                        prompt_sha256: digest,
                    },
                    None,
                )
            } else {
                let (Some(origin), Some(key)) =
                    (evidence.origin.as_deref(), evidence.unit_key.as_ref())
                else {
                    return false;
                };
                let Some(owner) =
                    self.writer
                        .notice_owners
                        .owned_proof(origin, &key.native_key, prompt)
                else {
                    return false;
                };
                let expired = self
                    .bindings
                    .binding_history(self.writer.channel())
                    .ok()
                    .and_then(|events| boundary(&events, origin));
                (
                    Settled::Owned {
                        origin: Box::new(origin.clone()),
                        native_key: key.native_key.clone(),
                        prompt_sha256: digest,
                        owner,
                    },
                    expired,
                )
            };
        self.writer
            .record(LedgerEntry::InputNotice {
                entry: Box::new(NoticeEntry::Settled {
                    record: evidence.record.clone(),
                    settlement: Box::new(Settlement {
                        proof,
                        expired_after,
                    }),
                }),
            })
            .is_ok()
    }
}
