//! The reader count health alarms on: readers that may still be written. An old source whose
//! execution a later one replaced in its pane is left out while fully read.

use super::*;
use crate::services::tui_o::writer::actor::codex_retention::{Bind, last_bind, superseded};

/// Per old source key and hop seq: whether a later execution replaced its writer.
pub(super) type Judged = HashMap<(String, u64), bool>;

impl<B: BindingEvents> Sources<B> {
    /// Below the limit the count is published without reading the binding log.
    pub(super) fn publish_reader_count<P: DiscordPort, L: DeliveryLease, A: AlarmSink>(
        &mut self,
        writer: &mut ChannelWriter<P, L, A>,
    ) {
        let mut count = self.readers.iter().filter(|r| r.reading()).count();
        if count > MAX_READERS {
            self.judge_replaced(writer);
            let left_out = (self.readers.iter())
                .filter(|r| r.reading() && self.replaced_and_read(r))
                .count();
            count -= left_out;
        }
        if count > MAX_READERS && !self.readers_alarmed {
            writer.alarm(WriterAlarm::TooManyReaders { count });
        }
        self.readers_alarmed = count > MAX_READERS;
        writer.reconcile_reader_count(count);
    }

    /// Judges every hop again once the log tail moved or a hop is new; an unreadable log
    /// replaces none and is read again at the next tend.
    fn judge_replaced<P: DiscordPort, L: DeliveryLease, A: AlarmSink>(
        &mut self,
        writer: &mut ChannelWriter<P, L, A>,
    ) {
        let tail = *self.notice.borrow();
        let hops: Vec<_> = (self.rotation.successors.iter())
            .filter_map(|(key, next)| Some((key.clone(), next.seq?, next.tmux_session.clone())))
            .collect();
        let judged = self.replaced.as_ref().is_some_and(|(at, judged)| {
            *at == tail
                && (hops.iter()).all(|(key, seq, _)| judged.contains_key(&(key.clone(), *seq)))
        });
        if judged {
            return;
        }
        let Ok(Some((_, events))) = self.read_log(writer, 0) else {
            self.replaced = None;
            return;
        };
        let judged = (hops.into_iter())
            .map(|(key, seq, pane)| {
                let replaced = pane.is_some_and(|pane| hop_replaced(&events, &key, seq, &pane));
                ((key, seq), replaced)
            })
            .collect();
        self.replaced = Some((tail, judged));
    }

    fn replaced_and_read(&self, reader: &Reader) -> bool {
        let key = source_key(&reader.source);
        let hop = self.rotation.successors.get(&key).and_then(|next| next.seq);
        let replaced = (self.replaced.as_ref()).is_some_and(|(_, judged)| {
            hop.is_some_and(|seq| judged.get(&(key, seq)) == Some(&true))
        });
        replaced
            && reader.pending.is_none()
            && reader.drain_to.is_none()
            && (reader.capture.as_ref())
                .is_some_and(|c| c.file_len().ok() == Some(c.captured_through()))
    }
}

/// Retention's same-pane basis, with the channel's latest event standing for the execution now
/// in the pane: every bind of `key` ran in `pane` before the hop, under another nonce.
fn hop_replaced(events: &[BindingEvent], key: &str, seq: u64, pane: &str) -> bool {
    let Some(now) = events
        .last()
        .filter(|e| !e.execution_nonce.trim().is_empty())
    else {
        return false;
    };
    let last = last_bind(binds(events, key), seq, pane);
    last.is_ok_and(|bind| superseded(&bind, &now.tmux_session, &now.execution_nonce) == Some(true))
}

/// Binds of `key` in seq order; a Pending binds the source its one Resolved names, a Pending a
/// rejection closed binds nothing, and an open or doubly settled Pending makes the history unknown.
fn binds<'a>(
    events: &'a [BindingEvent],
    key: &'a str,
) -> impl Iterator<Item = Result<Bind<'a>, &'static str>> {
    events.iter().filter_map(move |event| {
        let bind = |binder: &BindingEvent| Bind {
            seq: event.seq,
            binder_seq: binder.seq,
            pane: &event.tmux_session,
            nonce: &event.execution_nonce,
        };
        let pending = match &event.record {
            BindingRecord::Bound {
                new: BindingTarget::Source(source),
                ..
            } => return (source_key(source) == key).then(|| Ok(bind(event))),
            BindingRecord::Bound {
                new: BindingTarget::Pending { .. },
                ..
            } => event.seq,
            _ => return None,
        };
        let mut settled = events.iter().filter(|later| match &later.record {
            BindingRecord::Resolved { resolves_seq, .. } => *resolves_seq == pending,
            BindingRecord::Rejected { closes, .. } => *closes == Some(pending),
            _ => false,
        });
        let later = match (settled.next(), settled.next()) {
            (Some(later), None) => later,
            (None, _) => return Some(Err("codex_retention_pending")),
            _ => return Some(Err("codex_retention_resolution_ambiguous")),
        };
        let BindingRecord::Resolved { source, .. } = &later.record else {
            return None;
        };
        if later.tmux_session != event.tmux_session
            || later.execution_nonce != event.execution_nonce
        {
            return Some(Err("codex_retention_resolution_ambiguous"));
        }
        (source_key(source) == key).then(|| Ok(bind(later)))
    })
}
