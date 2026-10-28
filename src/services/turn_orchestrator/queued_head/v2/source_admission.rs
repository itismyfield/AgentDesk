//! A caller keeps custody until its exact candidate is confirmed, including synthetic inputs.
use super::*;
use std::sync::{Arc, Mutex};

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum AdmitError {
    NotCommitted(String),
    Full,
    Pending,
    /// The ticket already names another original; nothing is admitted under it.
    Conflict(String),
}

impl AdmitError {
    /// An uncertain publish: neither accepted nor refused, so the caller keeps the same ticket.
    pub(crate) fn is_pending(&self) -> bool {
        *self == Self::Pending
    }
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
    ticket: Option<Ticket>,
}

/// A caller's stable admission name, fixed before it records its own intent; its receipt also
/// carries a digest of the original, so one ticket never names two originals.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Ticket(String);

impl Ticket {
    pub(crate) fn new(raw: &str) -> Result<Self, String> {
        let plain = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_';
        if raw.is_empty() || raw.len() > 64 || !raw.chars().all(plain) {
            return Err(format!("invalid admission ticket {raw:?}"));
        }
        Ok(Self(raw.to_string()))
    }

    /// The ticket a queue input derives from its sources, so its retries name one admission.
    pub(super) fn of_sources(original: &PendingQueueItem) -> Self {
        Self(format!("src-{}", digest(&sources::identity(original))))
    }

    fn prefix(&self) -> String {
        format!("tk:{}:", self.0)
    }

    fn receipt(&self, original: &PendingQueueItem) -> String {
        let named = (sources::identity(original), original.text.as_str());
        format!("{}{}", self.prefix(), digest(&named))
    }
}

fn digest(value: &impl serde::Serialize) -> String {
    use sha2::{Digest, Sha256};
    let bytes = serde_json::to_vec(value).expect("queue identity serializes");
    let hash = Sha256::digest(bytes);
    hash[..16].iter().map(|b| format!("{b:02x}")).collect()
}

/// What the owner holds for a ticket, read without recovery or commit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TicketStatus {
    Committed,
    /// Only an unconfirmed publish names it.
    Pending,
    Absent,
}

impl TicketStatus {
    pub(crate) fn committed(self) -> bool {
        self == Self::Committed
    }

    pub(crate) fn pending(self) -> bool {
        self == Self::Pending
    }
}

impl Custody {
    pub(crate) fn new(original: &PendingQueueItem) -> Self {
        Self {
            original: original.clone(),
            state: State::New,
            ticket: None,
        }
    }

    /// Custody whose receipt is the caller's ticket: a retry, or a restart that re-adopted the
    /// owner, finds the receipt before any second admission.
    pub(crate) fn with_ticket(original: &PendingQueueItem, ticket: Ticket) -> Self {
        Self {
            ticket: Some(ticket),
            ..Self::new(original)
        }
    }

    /// Re-entry for a ticket last reported Pending; the same lookup runs before any admission.
    pub(crate) fn resume(original: &PendingQueueItem, ticket: Ticket) -> Self {
        Self::with_ticket(original, ticket)
    }

    pub(crate) fn advance(&mut self, owner: &mut Owner) -> Result<Admitted, AdmitError> {
        if let Some(ticket) = self.ticket.clone() {
            return self.advance_ticket(&ticket, owner);
        }
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

impl Custody {
    fn advance_ticket(
        &mut self,
        ticket: &Ticket,
        owner: &mut Owner,
    ) -> Result<Admitted, AdmitError> {
        let receipt = ticket.receipt(&self.original);
        if owner.pending.is_some() && owner.recover().is_err() && owner.pending.is_some() {
            return Err(AdmitError::Pending);
        }
        let prefix = ticket.prefix();
        let named = (owner.snapshot.admission_receipts.iter())
            .find(|r| r.starts_with(&prefix))
            .cloned();
        match named {
            Some(found) if found == receipt => {
                self.state = State::Done;
                Ok(owner.acknowledge(&receipt).unwrap_or(Admitted::Duplicate))
            }
            Some(_) => Err(AdmitError::Conflict(
                "the ticket already names another original".into(),
            )),
            None => {
                owner.unacknowledged.retain(|(r, _)| *r != receipt);
                let (next, outcome) = owner.fresh_candidate(&self.original)?;
                let Some(next) = next else {
                    // Its sources were admitted earlier under another receipt.
                    self.state = State::Done;
                    return Ok(outcome);
                };
                match owner.admit_commit(next, Some(receipt.clone())) {
                    Ok(()) => {
                        self.state = State::Done;
                        Ok(outcome)
                    }
                    Err(AdmitError::Pending) => {
                        owner.unacknowledged.push((receipt, outcome));
                        Err(AdmitError::Pending)
                    }
                    Err(error) => Err(error),
                }
            }
        }
    }
}

impl Owner {
    /// The outcome of an admission this owner committed while its caller saw Pending; acknowledged
    /// once, after which the same ticket is a duplicate.
    fn acknowledge(&mut self, receipt: &str) -> Option<Admitted> {
        let at = self.unacknowledged.iter().position(|(r, _)| r == receipt)?;
        Some(self.unacknowledged.remove(at).1)
    }

    /// Where `ticket` stands, read from the committed snapshot and the unconfirmed candidate only.
    pub(crate) fn admission_status(&self, ticket: &Ticket) -> TicketStatus {
        let prefix = ticket.prefix();
        let names = |receipts: &[String]| receipts.iter().any(|r| r.starts_with(&prefix));
        if names(&self.snapshot.admission_receipts) {
            TicketStatus::Committed
        } else if (self.pending.as_ref()).is_some_and(|(_, next)| names(&next.admission_receipts)) {
            TicketStatus::Pending
        } else {
            TicketStatus::Absent
        }
    }

    /// The attempt nonce and bound source of the committed `original`, while it is the head or a
    /// retained input; an input the admission partitioned keeps another identity.
    pub(crate) fn exact_turn(
        &self,
        ticket: &Ticket,
        original: &PendingQueueItem,
    ) -> Option<(String, Option<serde_json::Value>)> {
        let receipt = ticket.receipt(original);
        if !self.snapshot.admission_receipts.contains(&receipt) {
            return None;
        }
        let identity = sources::identity(original);
        let retained = self.snapshot.retained_inputs.iter().map(|r| &r.input);
        let input = (self.snapshot.head_injection.iter().chain(retained))
            .find(|input| input.identity == identity)?;
        let nonce = input.attempt.as_ref()?.nonce.clone();
        Some((nonce, input.resolved_source.clone()))
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

    /// Admits `original` under the caller's ticket; repeat it with the same ticket after Pending
    /// or a restart. An invalid ticket refuses before the owner is asked.
    pub(crate) async fn v2_admit_ticket(
        &self,
        original: PendingQueueItem,
        ticket: String,
    ) -> Result<Admitted, AdmitError> {
        let ticket = Ticket::new(&ticket).map_err(AdmitError::NotCommitted)?;
        self.v2_admit(Arc::new(Mutex::new(Custody::resume(&original, ticket))))
            .await
    }

    /// Reads the ticket's standing; it never recovers, commits or admits.
    pub(crate) async fn v2_admission_status(&self, ticket: String) -> Result<TicketStatus, String> {
        let ticket = Ticket::new(&ticket)?;
        self.v2_request(move |slot| {
            let owner = slot.as_ref().ok_or("V2 not adopted")?;
            Ok(owner.admission_status(&ticket))
        })
        .await
    }

    /// Test fixture: reserves and arms the queue head under `nonce`, as a pump does before its
    /// effect.
    #[cfg(test)]
    pub(crate) async fn v2_arm_head_for_tests(&self, nonce: &str) -> Result<(), String> {
        use super::super::v2_types::{
            AttemptIdentity, AttemptSource, PreparedPayload, TargetIdentity,
        };
        let nonce = nonce.to_string();
        self.v2_request(move |slot| {
            let owner = slot.as_mut().ok_or("V2 not adopted")?;
            let attempt = AttemptIdentity {
                nonce,
                provider: "claude".into(),
                target: TargetIdentity {
                    host: "tmux".into(),
                    owner: "owner-pane".into(),
                    root: "/test".into(),
                    execution_nonce: "execution".into(),
                },
                source: AttemptSource::Existing {
                    session_id: "native".into(),
                    transcript: "/test/transcript".into(),
                    file: [1, 2],
                    offset: 0,
                },
            };
            let payload = PreparedPayload {
                frame: "frame".into(),
                digest: "digest".into(),
                attribution: serde_json::json!({}),
            };
            let key = owner.reserve(attempt, payload)?;
            drop(owner.arm(&key, 0)?);
            Ok(())
        })
        .await
    }

    /// The attempt and bound source the owner holds for the ticket's committed original.
    pub(crate) async fn v2_exact_turn(
        &self,
        original: PendingQueueItem,
        ticket: String,
    ) -> Result<Option<(String, Option<serde_json::Value>)>, String> {
        let ticket = Ticket::new(&ticket)?;
        self.v2_request(move |slot| {
            let owner = slot.as_ref().ok_or("V2 not adopted")?;
            Ok(owner.exact_turn(&ticket, &original))
        })
        .await
    }
}

#[cfg(test)]
mod tests;

impl Owner {
    fn admission_context(
        &self,
        original: &PendingQueueItem,
        ticket: &Ticket,
    ) -> Result<Option<serde_json::Value>, String> {
        let prefix = format!("context:{}:", ticket.receipt(original));
        let record = self
            .snapshot
            .admission_receipts
            .iter()
            .find_map(|entry| entry.strip_prefix(&prefix));
        let Some(record) = record else {
            return Ok(None);
        };
        let context: super::super::v2_types::AdmissionContext =
            serde_json::from_str(record).map_err(|e| e.to_string())?;
        if context.original.as_ref()
            != Some(&serde_json::to_value(original).map_err(|e| e.to_string())?)
        {
            return Err("admission original differs from stored context".into());
        }
        Ok(context.execution)
    }

    fn admit_with_context(
        &mut self,
        original: &PendingQueueItem,
        ticket: Ticket,
        context: serde_json::Value,
    ) -> Result<Admitted, AdmitError> {
        if self.pending.is_some() && self.recover().is_err() && self.pending.is_some() {
            return Err(AdmitError::Pending);
        }
        if self
            .snapshot
            .admission_receipts
            .iter()
            .any(|r| r.starts_with(&ticket.prefix()))
        {
            let stored = self
                .admission_context(original, &ticket)
                .map_err(AdmitError::Conflict)?;
            if stored.as_ref() != Some(&context) {
                return Err(AdmitError::Conflict(
                    "admission provenance differs or is absent".into(),
                ));
            }
            return Custody::with_ticket(original, ticket).advance(self);
        }
        let (next, outcome) = self.fresh_candidate(original)?;
        let Some(mut next) = next else {
            return Err(AdmitError::Conflict(
                "source already fenced without this context".into(),
            ));
        };
        let receipt = ticket.receipt(original);
        let stored = super::super::v2_types::AdmissionContext {
            original: Some(
                serde_json::to_value(original)
                    .map_err(|e| AdmitError::NotCommitted(e.to_string()))?,
            ),
            execution: Some(context),
        };
        let encoded =
            serde_json::to_string(&stored).map_err(|e| AdmitError::NotCommitted(e.to_string()))?;
        // The additional record shares the original's snapshot commit; the ticket bytes stay unchanged.
        next.admission_receipts
            .push(format!("context:{receipt}:{encoded}"));
        match self.admit_commit(next, Some(receipt.clone())) {
            Err(AdmitError::Pending) => {
                self.unacknowledged.push((receipt, outcome));
                Err(AdmitError::Pending)
            }
            Ok(()) => Ok(outcome),
            Err(error) => Err(error),
        }
    }
}

impl super::super::super::ChannelMailboxHandle {
    pub(crate) async fn v2_admit_ticket_with_context(
        &self,
        original: PendingQueueItem,
        ticket: String,
        context: serde_json::Value,
    ) -> Result<Admitted, AdmitError> {
        let ticket = Ticket::new(&ticket).map_err(AdmitError::NotCommitted)?;
        self.v2_request(move |slot| {
            Ok(match slot.as_mut() {
                Some(owner) => owner.admit_with_context(&original, ticket, context),
                None => Err(AdmitError::NotCommitted("V2 not adopted".into())),
            })
        })
        .await
        .map_err(|_| AdmitError::Pending)?
    }

    pub(crate) async fn v2_admission_context(
        &self,
        original: PendingQueueItem,
        ticket: String,
    ) -> Result<Option<serde_json::Value>, String> {
        let ticket = Ticket::new(&ticket)?;
        self.v2_request(move |slot| {
            slot.as_ref()
                .ok_or_else(|| "V2 not adopted".into())?
                .admission_context(&original, &ticket)
        })
        .await
    }
}

#[cfg(test)]
mod context_tests;
