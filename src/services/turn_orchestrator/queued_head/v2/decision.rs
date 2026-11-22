//! What the owner holds of one Decision event's original, read by the event's settlement, and
//! its removal once PG closed the event: the original never runs and no notice asks for a resend.
use super::*;

/// Where the owner holds an original, most advanced record first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Held {
    /// Queued and unattempted: it waits for its turn under its ticket.
    Waiting,
    /// An attempt carries it and has not settled.
    InFlight,
    /// Its attempt settled as submitted.
    Delivered,
    /// Kept or refused on a proof that no key was sent, with the owner's reason.
    NotSent(String),
    /// Kept after an attempt whose write was never confirmed.
    Unconfirmed,
    Gone,
}

impl Owner {
    /// `message` is the event's stable id; an original queued under another id names `origin`.
    pub(crate) fn decision_held(&self, message: u64, origin: &str) -> Held {
        let s = &self.snapshot;
        let names = |item: &PendingQueueItem| named(item, message, origin);
        if (s.head_injection.iter()).any(|head| head.members().any(names)) {
            return Held::InFlight;
        }
        let kept = (s.retained_inputs.iter()).find(|entry| entry.input.members().any(names));
        if let Some(entry) = kept {
            return match &entry.reason {
                RetainedReason::Unconfirmed => Held::Unconfirmed,
                RetainedReason::Rejected { reason, .. } => Held::NotSent(reason.clone()),
            };
        }
        if s.items.iter().any(names) {
            return Held::Waiting;
        }
        let refused = (s.refused_inputs.iter())
            .find_map(|r| r.names(message).then_some(&r.reason))
            .or_else(|| {
                (s.refused_fronts.iter()).find_map(|r| r.names(message).then_some(&r.reason))
            });
        if let Some(reason) = refused {
            return Held::NotSent(reason.clone());
        }
        let settled = |t: &TerminalIdentity| t.identity.primary == message;
        match s.terminal_identities.iter().any(settled) {
            true => Held::Delivered,
            false => Held::Gone,
        }
    }

    /// Ends the event's original unrun: a queued or proven-unsent copy leaves the queue alone,
    /// and every record kept of it stops owing its notice. An attempted head is not touched.
    pub(crate) fn discard_decision(&mut self, message: u64, origin: &str) -> Result<(), String> {
        let queued = (self.snapshot.items.iter())
            .chain((self.snapshot.retained_inputs.iter()).flat_map(|entry| entry.input.members()))
            .find(|item| named(item, message, origin))
            .map(|item| item.message_id);
        if let Some(id) = queued {
            let id = poise::serenity_prelude::MessageId::new(id);
            if let Some(error) = control::cancel(self, id, false).persistence_error {
                return Err(error);
            }
        }
        let mut next = self.snapshot.clone();
        let kept = (next.retained_inputs.iter_mut())
            .filter(|entry| {
                entry
                    .input
                    .members()
                    .any(|item| named(item, message, origin))
            })
            .map(|entry| &mut entry.notice);
        let refused = (next.refused_inputs.iter_mut())
            .filter(|r| r.names(message))
            .map(|r| &mut r.notice);
        let fronts = (next.refused_fronts.iter_mut())
            .filter(|r| r.names(message))
            .map(|r| &mut r.notice);
        let mut owed = false;
        for notice in kept.chain(refused).chain(fronts) {
            owed |= !notice.enqueued;
            notice.enqueued = true;
        }
        match owed {
            true => self.commit(next),
            false => Ok(()),
        }
    }
}

fn named(item: &PendingQueueItem, message: u64, origin: &str) -> bool {
    item.message_id == message
        || (item.source_message_queued_generations.iter())
            .any(|source| source.origin_id.as_deref() == Some(origin))
}
