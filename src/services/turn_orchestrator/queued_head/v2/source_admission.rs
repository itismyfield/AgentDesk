//! A caller keeps custody until its exact candidate is confirmed, including synthetic inputs.
use super::*;
use std::sync::{Arc, Mutex};

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum AdmitError {
    NotCommitted(String),
    Full,
    Pending,
}

impl From<AdmitError> for String {
    fn from(error: AdmitError) -> Self {
        format!("{error:?}")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Admitted {
    Queued,
    Duplicate,
    RetainedOpaque,
}

enum State {
    New,
    Pending(String, Admitted),
    Done,
}

pub(crate) struct Custody {
    original: PendingQueueItem,
    state: State,
}

impl Custody {
    pub(crate) fn new(original: &PendingQueueItem) -> Self {
        Self {
            original: original.clone(),
            state: State::New,
        }
    }

    pub(crate) fn advance(&mut self, owner: &mut Owner) -> Result<Admitted, AdmitError> {
        match &self.state {
            State::Done => return Ok(Admitted::Duplicate),
            State::Pending(receipt, outcome) => {
                if let Err(error) = owner.recover() {
                    if owner.pending.is_none() {
                        self.state = State::New;
                        return Err(AdmitError::NotCommitted(error));
                    }
                    return Err(AdmitError::Pending);
                }
                if !owner.snapshot.admission_receipts.contains(receipt) {
                    if owner.pending.is_none() {
                        self.state = State::New;
                        return Err(AdmitError::NotCommitted("admission receipt absent".into()));
                    }
                    return Err(AdmitError::Pending);
                }
                let outcome = *outcome;
                self.state = State::Done;
                return Ok(outcome);
            }
            State::New => {}
        }
        let (next, outcome) = owner.fresh_candidate(&self.original)?;
        let Some(next) = next else {
            self.state = State::Done;
            return Ok(outcome);
        };
        let receipt = format!(
            "{}:{}",
            owner.snapshot.admission_sequence.saturating_add(1),
            uuid::Uuid::new_v4()
        );
        match owner.admit_commit(next, Some(receipt.clone())) {
            Ok(()) => {
                self.state = State::Done;
                Ok(outcome)
            }
            Err(AdmitError::Pending) => {
                self.state = State::Pending(receipt, outcome);
                Err(AdmitError::Pending)
            }
            Err(error) => Err(error),
        }
    }
}

impl Owner {
    pub(super) fn admit_commit(
        &mut self,
        mut next: SnapshotV2,
        receipt: Option<String>,
    ) -> Result<(), AdmitError> {
        next.admission_sequence = next
            .admission_sequence
            .checked_add(1)
            .ok_or_else(|| AdmitError::NotCommitted("admission sequence exhausted".into()))?;
        if let Some(receipt) = receipt {
            next.admission_receipts.push(receipt);
        }
        self.commit(next).map_err(|error| {
            if self.pending.is_some() {
                AdmitError::Pending
            } else {
                AdmitError::NotCommitted(error)
            }
        })
    }

    fn fresh_candidate(
        &self,
        original: &PendingQueueItem,
    ) -> Result<(Option<SnapshotV2>, Admitted), AdmitError> {
        if self.pending.is_some() {
            return Err(AdmitError::NotCommitted("owner recovery required".into()));
        }
        let mut fresh = original.clone();
        let ids = sources::identity(original).sources;
        let known: Vec<_> = ids
            .iter()
            .filter(|source| {
                let mut probe = original.clone();
                probe.message_id = source.message_id;
                probe.source_message_ids = vec![source.message_id];
                self.fenced(&probe)
            })
            .map(|s| s.message_id)
            .collect();
        if !known.is_empty() && known.len() == ids.len() {
            return Ok((None, Admitted::Duplicate));
        }
        let mut next = self.snapshot.clone();
        if !known.is_empty() {
            // Only explicit per-source text can be partitioned; shared assets stay opaque.
            let segments: Vec<_> = original
                .source_text_segments
                .iter()
                .map(|s| {
                    super::super::super::SourceMessageTextSegment::new(
                        poise::serenity_prelude::MessageId::new(s.message_id),
                        s.text.clone(),
                    )
                })
                .collect();
            let exact = original.reply_context.is_none()
                && original.pending_uploads.is_empty()
                && original.voice_announcement.is_none()
                && original.source_text_segments.len() == ids.len()
                && ids
                    .iter()
                    .zip(&original.source_text_segments)
                    .all(|(id, segment)| id.message_id == segment.message_id)
                && original.text == super::super::super::join_source_text_segments(&segments);
            if !exact {
                next.retained_inputs.push(RetainedInput {
                    input: InputAttempt {
                        identity: sources::identity(original),
                        original: original.clone(),
                        phase: HeadPhase::Reserved,
                        attempt: None,
                        payload: None,
                        confirmation_deadline_ms: None,
                        resolved_source: None,
                        payload_window: None,
                        resolved_target: None,
                        preparation_hold: None,
                        legacy_attempt: None,
                    },
                    reason: RetainedReason::Rejected {
                        reason: "opaque_unsent".into(),
                    },
                    disposition: Some(RetainedDisposition::OpaqueUnsent),
                    notice: NoticeIntent {
                        kind: "retained_retry_required".into(),
                        enqueued: false,
                    },
                });
                return Ok((Some(next), Admitted::RetainedOpaque));
            }
            fresh
                .source_text_segments
                .retain(|s| !known.contains(&s.message_id));
            fresh.source_message_ids = ids
                .iter()
                .filter(|s| !known.contains(&s.message_id))
                .map(|s| s.message_id)
                .collect();
            fresh.message_id = fresh.source_message_ids[0];
            fresh.queued_generation = ids
                .iter()
                .find(|s| s.message_id == fresh.message_id)
                .unwrap()
                .queued_generation;
            fresh
                .source_message_queued_generations
                .retain(|s| !known.contains(&s.message_id));
            let segments: Vec<_> = fresh
                .source_text_segments
                .iter()
                .map(|s| {
                    super::super::super::SourceMessageTextSegment::new(
                        poise::serenity_prelude::MessageId::new(s.message_id),
                        s.text.clone(),
                    )
                })
                .collect();
            fresh.text = super::super::super::join_source_text_segments(&segments);
        }
        if next.items.len() >= 30 {
            return Err(AdmitError::Full);
        }
        next.items.push(fresh);
        Ok((Some(next), Admitted::Queued))
    }
}

impl super::super::super::ChannelMailboxHandle {
    pub(crate) async fn v2_admit(
        &self,
        custody: Arc<Mutex<Custody>>,
    ) -> Result<Admitted, AdmitError> {
        self.v2_request(move |slot| {
            let mut custody = custody.lock().map_err(|_| "admission custody poisoned")?;
            Ok(match slot.as_mut() {
                Some(owner) => custody.advance(owner),
                None => Err(AdmitError::NotCommitted("V2 not adopted".into())),
            })
        })
        .await
        .map_err(|_| AdmitError::Pending)?
    }
}

#[cfg(test)]
mod tests;
