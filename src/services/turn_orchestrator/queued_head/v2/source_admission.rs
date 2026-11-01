//! A caller keeps custody until its exact candidate is confirmed, including synthetic inputs.
use super::*;
#[cfg(test)]
use std::sync::{Arc, Mutex};

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum AdmitError {
    NotCommitted(String),
    Full,
    Pending,
    /// The ticket already names another original; nothing is admitted under it.
    Conflict(String),
    /// Its sources were admitted earlier without this ticket, so the ticket names nothing.
    SourceAlreadyKnown,
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
        self.receipt_for(&canonical(original))
    }

    /// The receipt naming this ticket and one canonical original.
    fn receipt_for(&self, canonical: &serde_json::Value) -> String {
        format!("{}{}", self.prefix(), digest(canonical))
    }
}

/// The original as stored, without the fields a retry recomputes: wall-clock times and diagnostics.
/// Every other field, including one added later, changes what runs and so names another original.
fn canonical(original: &PendingQueueItem) -> serde_json::Value {
    let mut value = serde_json::to_value(original).expect("queue item serializes");
    if let Some(map) = value.as_object_mut() {
        for field in ["created_at_wall_time_ms", "channel_name"] {
            map.remove(field);
        }
        if let Some(serde_json::Value::Array(sources)) =
            map.get_mut("source_message_queued_generations")
        {
            for source in sources.iter_mut().filter_map(|s| s.as_object_mut()) {
                source.remove("enqueued_at_epoch_us");
            }
        }
    }
    value
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

/// An attempt for an admitted original and the native source it was bound to, if any.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ExactTurn {
    pub(crate) attempt: String,
    pub(crate) turn: Option<serde_json::Value>,
}

impl ExactTurn {
    /// Whether an actual bind names this attempt and this source.
    pub(crate) fn is(&self, attempt: &str, turn: Option<&serde_json::Value>) -> bool {
        self.attempt == attempt && self.turn.as_ref() == turn
    }
}

/// Where a committed ticket's original stands in the owner; each case has one meaning.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TurnBind {
    /// An attempt may have sent it; an actual bind must name this turn.
    Armed(ExactTurn),
    /// Queued, reserved, or retained as proven or opaque unsent.
    NotArmed,
    /// Delivered under this turn.
    Settled(ExactTurn),
    /// Committed, but the owner no longer keeps a record of it; never re-issue.
    Unknown,
    /// No committed admission under this ticket.
    Absent,
}

impl TurnBind {
    #[cfg(test)]
    pub(crate) fn armed(&self) -> bool {
        matches!(self, Self::Armed(_))
    }

    #[cfg(test)]
    pub(crate) fn settled(&self) -> bool {
        matches!(self, Self::Settled(_))
    }
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
        let prefix = ticket.prefix();
        let named = |receipts: &[String]| receipts.iter().find(|r| r.starts_with(&prefix)).cloned();
        // Read before any recovery: a refusal leaves the snapshot and the candidate as they were.
        let committed = named(&owner.snapshot.admission_receipts);
        let candidate =
            (owner.pending.as_ref()).and_then(|(_, next)| named(&next.admission_receipts));
        if [&committed, &candidate]
            .into_iter()
            .flatten()
            .any(|found| *found != receipt)
        {
            return Err(AdmitError::Conflict(
                "the ticket already names another original".into(),
            ));
        }
        let unsettled = committed.is_none() && owner.pending.is_some();
        if unsettled && owner.recover().is_err() && owner.pending.is_some() {
            return Err(AdmitError::Pending);
        }
        if owner.snapshot.admission_receipts.contains(&receipt) {
            self.state = State::Done;
            return Ok(owner.acknowledge(&receipt).unwrap_or(Admitted::Duplicate));
        }
        owner.unacknowledged.retain(|(r, _)| *r != receipt);
        let (next, outcome) = owner.fresh_candidate(&self.original)?;
        let Some(mut next) = next else {
            return Err(AdmitError::SourceAlreadyKnown);
        };
        next.bind_admitted(&receipt, outcome, &digest(&canonical(&self.original)));
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

impl Owner {
    /// The outcome of an admission this owner committed while its caller saw Pending; acknowledged
    /// once, after which the same ticket is a duplicate.
    fn acknowledge(&mut self, receipt: &str) -> Option<Admitted> {
        let at = self.unacknowledged.iter().position(|(r, _)| r == receipt)?;
        Some(self.unacknowledged.remove(at).1)
    }

    /// The original an admission under `ticket` holds, read from the committed snapshot and then
    /// the unconfirmed candidate, so a recovery re-enters that exact original.
    pub(crate) fn ticket_original(&self, ticket: &Ticket) -> Option<PendingQueueItem> {
        let prefix = ticket.prefix();
        let candidate = self.pending.as_ref().map(|(_, next)| next);
        for snapshot in std::iter::once(&self.snapshot).chain(candidate) {
            let receipts = snapshot.admission_receipts.iter();
            let Some(receipt) = receipts.into_iter().find(|r| r.starts_with(&prefix)) else {
                continue;
            };
            let inputs = snapshot.head_injection.iter();
            let inputs = inputs.chain(snapshot.retained_inputs.iter().map(|r| &r.input));
            let mut held = snapshot
                .items
                .iter()
                .chain(inputs.flat_map(|i| i.members()));
            if let Some(found) = held.find(|item| ticket.receipt(item) == *receipt) {
                return Some(found.clone());
            }
        }
        None
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

    /// Where the ticket's committed original stands, followed through the binds its admission left.
    pub(crate) fn exact_turn(
        &self,
        ticket: &Ticket,
        original: &PendingQueueItem,
    ) -> Result<TurnBind, String> {
        let receipt = ticket.receipt(original);
        let prefix = ticket.prefix();
        match (self.snapshot.admission_receipts.iter()).find(|r| r.starts_with(&prefix)) {
            None => return Ok(TurnBind::Absent),
            Some(found) if *found != receipt => {
                return Err("the ticket names another original".into());
            }
            Some(_) => {}
        }
        let snapshot = &self.snapshot;
        let binds: Vec<_> = (snapshot.admission_binds.iter())
            .filter(|b| b.receipt == receipt)
            .collect();
        let live = || {
            let retained = snapshot.retained_inputs.iter().map(|r| &r.input);
            snapshot.head_injection.iter().chain(retained)
        };
        for bind in &binds {
            if let BindAt::Attempt { identity, attempt } = &bind.at
                && let Some(input) = live().find(|i| {
                    i.attempt.as_ref() == Some(attempt)
                        && i.members().any(|m| sources::identity(m) == *identity)
                })
            {
                return Ok(match input.phase {
                    HeadPhase::MayHaveSent | HeadPhase::Unknown => TurnBind::Armed(ExactTurn {
                        attempt: attempt.nonce.clone(),
                        turn: input.resolved_source.clone(),
                    }),
                    HeadPhase::Reserved => TurnBind::NotArmed,
                });
            }
        }
        let held = binds.iter().any(|bind| match &bind.at {
            BindAt::Queued { index, item } => snapshot.queued_digest(*index).as_ref() == Some(item),
            BindAt::Opaque => (snapshot.retained_inputs.iter()).any(|r| {
                matches!(r.disposition, Some(RetainedDisposition::OpaqueUnsent))
                    && ticket.receipt(&r.input.original) == receipt
            }),
            _ => false,
        });
        if held {
            return Ok(TurnBind::NotArmed);
        }
        Ok(binds
            .iter()
            .find_map(|bind| match &bind.at {
                BindAt::Settled {
                    attempt, source, ..
                } => Some(TurnBind::Settled(ExactTurn {
                    attempt: attempt.clone(),
                    turn: source.clone(),
                })),
                _ => None,
            })
            .unwrap_or(TurnBind::Unknown))
    }
}

/// Binds follow their input: a queued index shifts with removals before it, an attempt moves with
/// its input between head and retained, and a delivery leaves a settled bind.
impl SnapshotV2 {
    pub(super) fn queued_digest(&self, index: usize) -> Option<String> {
        self.items.get(index).map(|item| digest(&canonical(item)))
    }

    /// Binds the admitted input only when it is the whole original: a partitioned admission queues
    /// a part, so no attempt of that part ever stands for the original.
    fn bind_admitted(&mut self, receipt: &str, outcome: Admitted, whole: &str) {
        let at = match outcome {
            Admitted::Queued => {
                let index = self.items.len() - 1;
                let item = self.queued_digest(index).expect("admitted item is queued");
                if item != whole {
                    return;
                }
                BindAt::Queued { index, item }
            }
            Admitted::RetainedOpaque => BindAt::Opaque,
            Admitted::Duplicate => return,
        };
        let receipt = receipt.to_string();
        self.admission_binds.push(AdmissionBind {
            receipt,
            at,
            context: None,
        });
    }

    /// Drops queued binds past the end, which a cut of the queue's tail left; run before a push.
    pub(super) fn prune_binds(&mut self) {
        let len = self.items.len();
        let stale =
            |b: &AdmissionBind| matches!(b.at, BindAt::Queued { index, .. } if index >= len);
        self.admission_binds.retain(|b| !stale(b));
    }

    /// Follows the removal of the queued item at `removed`.
    pub(super) fn unbind_item(&mut self, removed: usize) {
        self.admission_binds
            .retain(|b| !matches!(b.at, BindAt::Queued { index, .. } if index == removed));
        for bind in &mut self.admission_binds {
            if let BindAt::Queued { index, .. } = &mut bind.at
                && *index > removed
            {
                *index -= 1;
            }
        }
    }

    /// Carries every reserved member's queued bind to the one attempt in this same transition;
    /// attempts no input holds are dropped.
    pub(super) fn bind_reserved(&mut self) {
        let Some(head) = &self.head_injection else {
            return;
        };
        let Some(attempt) = head.attempt.clone() else {
            return;
        };
        let members: Vec<QueueIdentity> = head.members().map(sources::identity).collect();
        let mut moved = Vec::new();
        for (at, identity) in members.into_iter().enumerate() {
            let Some(item) = self.queued_digest(at) else {
                break;
            };
            let receipt = (self.admission_binds.iter()).find_map(|b| match &b.at {
                BindAt::Queued { index, item: bound } if *index == at && *bound == item => {
                    Some(b.receipt.clone())
                }
                _ => None,
            });
            moved.extend(receipt.map(|receipt| (receipt, identity)));
        }
        let live: Vec<(QueueIdentity, AttemptIdentity)> = (self.retained_inputs.iter())
            .flat_map(|r| {
                let attempt = r.input.attempt.clone();
                r.input
                    .members()
                    .filter_map(move |m| Some((sources::identity(m), attempt.clone()?)))
            })
            .collect();
        self.admission_binds.retain(|b| match &b.at {
            BindAt::Attempt { identity, attempt } => {
                live.contains(&(identity.clone(), attempt.clone()))
            }
            _ => true,
        });
        for (receipt, identity) in moved {
            let context = (self.admission_binds.iter())
                .find(|b| b.receipt == receipt)
                .and_then(|b| b.context.clone());
            let at = BindAt::Attempt {
                identity,
                attempt: attempt.clone(),
            };
            self.admission_binds.push(AdmissionBind {
                receipt,
                at,
                context,
            });
        }
    }
}

impl SnapshotV2 {
    /// After an ungrouping moved queue items, every ticket receipt whose original is queued binds
    /// to its new slot again, once each; the released unit's attempt binds go.
    pub(super) fn rebind_queued(&mut self, released: &InputAttempt) {
        let attempt = released.attempt.as_ref();
        let contexts: Vec<(String, AdmissionContext)> = (self.admission_binds.iter())
            .filter_map(|b| Some((b.receipt.clone(), b.context.clone()?)))
            .collect();
        self.admission_binds.retain(|b| match &b.at {
            BindAt::Queued { .. } => false,
            BindAt::Attempt { attempt: bound, .. } => Some(bound) != attempt,
            _ => true,
        });
        let bound: Vec<&String> = self.admission_binds.iter().map(|b| &b.receipt).collect();
        let mut free: Vec<String> = (self.admission_receipts.iter())
            .filter(|r| r.starts_with("tk:") && !bound.contains(r))
            .cloned()
            .collect();
        for index in 0..self.items.len() {
            let Some(item) = self.queued_digest(index) else {
                continue;
            };
            let suffix = format!(":{item}");
            if let Some(at) = free.iter().position(|r| r.ends_with(&suffix)) {
                let receipt = free.remove(at);
                let context = (contexts.iter())
                    .find(|(r, _)| *r == receipt)
                    .map(|(_, c)| c.clone());
                let at = BindAt::Queued { index, item };
                self.admission_binds.push(AdmissionBind {
                    receipt,
                    at,
                    context,
                });
            }
        }
    }
}

/// Whether two stored originals are the same admission, ignoring only recomputed fields.
pub(super) fn same_original(a: &PendingQueueItem, b: &PendingQueueItem) -> bool {
    canonical(a) == canonical(b)
}

/// Every member bind of the delivered attempt becomes a settled one at once, keeping its nonce
/// and bound source.
pub(super) fn settle_binds(snapshot: &mut SnapshotV2, key: &AttemptRef, now: u64) {
    let retained = snapshot.retained_inputs.iter().map(|r| &r.input);
    let input = (snapshot.head_injection.iter().chain(retained)).find(|input| key.matches(input));
    let source = input.and_then(|input| input.resolved_source.clone());
    let members: Vec<QueueIdentity> = input
        .map(|input| input.members().map(sources::identity).collect())
        .unwrap_or_default();
    for bind in &mut snapshot.admission_binds {
        if let BindAt::Attempt { identity, attempt } = &bind.at
            && members.contains(identity)
            && *attempt == key.attempt
        {
            bind.at = BindAt::Settled {
                attempt: key.attempt.nonce.clone(),
                source: source.clone(),
                settled_at_ms: now,
            };
        }
    }
}

/// Settled binds leave with the terminal identities, after the same window.
pub(super) fn expire_binds(snapshot: &mut SnapshotV2, now: u64) {
    snapshot.admission_binds.retain(|b| match b.at {
        BindAt::Settled { settled_at_ms, .. } => now.saturating_sub(settled_at_ms) < 86_400_000,
        _ => true,
    });
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
        next.prune_binds();
        next.items.push(fresh);
        Ok((Some(next), Admitted::Queued))
    }
}

impl super::super::super::ChannelMailboxHandle {
    #[cfg(test)]
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
    #[cfg(test)]
    pub(crate) async fn v2_admit_ticket(
        &self,
        original: PendingQueueItem,
        ticket: String,
    ) -> Result<Admitted, AdmitError> {
        let ticket = Ticket::new(&ticket).map_err(AdmitError::NotCommitted)?;
        // A generation of 0 is replaced by the process's own, so it names another input after a
        // restart; the caller settles it before asking.
        if original.queued_generation == 0 {
            return Err(AdmitError::NotCommitted(
                "the original's queued generation is unsettled".into(),
            ));
        }
        self.v2_admit(Arc::new(Mutex::new(Custody::resume(&original, ticket))))
            .await
    }

    /// Admits under `ticket` in one actor turn; a ticket the owner already holds resumes its stored
    /// original when `same_input` accepts the caller's copy. Returns the original admitted.
    pub(crate) async fn v2_resume_ticket(
        &self,
        original: PendingQueueItem,
        ticket: String,
        same_input: fn(&PendingQueueItem, &PendingQueueItem) -> bool,
    ) -> Result<(Admitted, PendingQueueItem), AdmitError> {
        let ticket = Ticket::new(&ticket).map_err(AdmitError::NotCommitted)?;
        self.v2_request(move |slot| {
            let Some(owner) = slot.as_mut() else {
                return Ok(Err(AdmitError::NotCommitted("V2 not adopted".into())));
            };
            let chosen = match owner.ticket_original(&ticket) {
                Some(stored) if same_input(&stored, &original) => stored,
                Some(_) => {
                    return Ok(Err(AdmitError::Conflict(
                        "another input names the ticket".into(),
                    )));
                }
                None if original.queued_generation == 0 => {
                    let unsettled = "the original's queued generation is unsettled";
                    return Ok(Err(AdmitError::NotCommitted(unsettled.into())));
                }
                None => original,
            };
            let admitted = Custody::resume(&chosen, ticket).advance(owner);
            Ok(admitted.map(|admitted| (admitted, chosen)))
        })
        .await
        .map_err(|_| AdmitError::Pending)?
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

    /// Test fixture: reserves the queue head under `nonce` and arms it, as a pump does before its
    /// effect; an already reserved head is only armed.
    #[cfg(test)]
    pub(crate) async fn v2_arm_head_for_tests(&self, nonce: &str) -> Result<(), String> {
        self.v2_reserve_head_for_tests(nonce, true).await
    }

    /// Test fixture: reserves the queue head under `nonce`, arming it when `arm` is set.
    #[cfg(test)]
    pub(crate) async fn v2_reserve_head_for_tests(
        &self,
        nonce: &str,
        arm: bool,
    ) -> Result<(), String> {
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
                followers: Vec::new(),
            };
            let key = match owner.active_attempt() {
                Ok(key) => key,
                Err(_) => owner.reserve(attempt, payload)?,
            };
            if arm {
                drop(owner.arm(&key, 0)?);
            }
            Ok(())
        })
        .await
    }

    /// Where the ticket's committed original stands; a ticket naming another original is an error.
    pub(crate) async fn v2_exact_turn(
        &self,
        original: PendingQueueItem,
        ticket: String,
    ) -> Result<TurnBind, String> {
        let ticket = Ticket::new(&ticket)?;
        self.v2_request(move |slot| {
            let owner = slot.as_ref().ok_or("V2 not adopted")?;
            owner.exact_turn(&ticket, &original)
        })
        .await
    }
}

impl Owner {
    fn context_in<'a>(
        snapshot: &'a SnapshotV2,
        receipt: &str,
    ) -> Option<&'a super::super::v2_types::AdmissionContext> {
        snapshot.admission_binds.iter().find_map(|b| {
            (b.receipt == receipt)
                .then_some(b.context.as_ref())
                .flatten()
        })
    }

    fn admit_with_context(
        &mut self,
        original: &PendingQueueItem,
        ticket: Ticket,
        execution: serde_json::Value,
    ) -> Result<Admitted, AdmitError> {
        let receipt = ticket.receipt_for(&canonical(original));
        let expected = super::super::v2_types::AdmissionContext {
            original: Some(canonical(original)),
            execution: Some(execution),
        };
        // Refuse conflicting committed or unconfirmed originals before any recovery writes.
        for snapshot in
            std::iter::once(&self.snapshot).chain(self.pending.iter().map(|(_, next)| next))
        {
            if snapshot
                .admission_receipts
                .iter()
                .any(|r| r.starts_with(&ticket.prefix()))
            {
                if !snapshot.admission_receipts.contains(&receipt)
                    || Self::context_in(snapshot, &receipt) != Some(&expected)
                {
                    return Err(AdmitError::Conflict(
                        "ticket original or execution context differs".into(),
                    ));
                }
            }
        }
        if self.pending.is_some() && self.recover().is_err() && self.pending.is_some() {
            return Err(AdmitError::Pending);
        }
        if self.snapshot.admission_receipts.contains(&receipt) {
            return Custody::with_ticket(original, ticket).advance(self);
        }
        let (next, outcome) = self.fresh_candidate(original)?;
        let Some(mut next) = next else {
            return Err(AdmitError::SourceAlreadyKnown);
        };
        next.bind_admitted(&receipt, outcome, &digest(&canonical(original)));
        let bind = next
            .admission_binds
            .iter_mut()
            .find(|b| b.receipt == receipt)
            .ok_or_else(|| AdmitError::NotCommitted("admission bind absent".into()))?;
        bind.context = Some(expected);
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
        if original.queued_generation == 0 {
            return Err(AdmitError::NotCommitted(
                "the original's queued generation is unsettled".into(),
            ));
        }
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
            let owner = slot.as_ref().ok_or("V2 not adopted")?;
            owner.exact_turn(&ticket, &original)?;
            let receipt = ticket.receipt_for(&canonical(&original));
            Ok(Owner::context_in(&owner.snapshot, &receipt).and_then(|c| c.execution.clone()))
        })
        .await
    }
}

#[cfg(test)]
mod context_tests;

// Admission fixtures require an owner backed by confirmed parent directory fsync.
#[cfg(all(test, unix))]
mod tests;
