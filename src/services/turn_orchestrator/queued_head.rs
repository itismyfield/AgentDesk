//! A queued head reserved for a busy Claude TUI: its durable phase, the sources already submitted,
//! and the check every queue write runs while a head is reserved. Nothing here submits input.
use super::active_source_dedup::strip_source_message_id_from_intervention;
use super::dispatch_reservation::queue_unread;
use std::path::Path;

use super::pending_queue_persistence::{
    PendingQueueItem, PendingQueueSourceTextSegment, pending_queue_file_path,
    pending_queue_item_from_intervention,
};
use super::*;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct SourceIdentity {
    pub(crate) message_id: u64,
    pub(crate) queued_generation: u64,
}

/// The primary, its generation and every source in order; terminal checks use the sources alone.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct QueueIdentity {
    pub(crate) primary: u64,
    pub(crate) queued_generation: u64,
    pub(crate) sources: Vec<SourceIdentity>,
}

impl QueueIdentity {
    pub(crate) fn of(item: &Intervention) -> Self {
        let ids = match item.source_message_ids.as_slice() {
            [] => vec![item.message_id],
            ids => ids.to_vec(),
        };
        let generation = |id: MessageId| {
            let owners = item.source_message_queued_generations.iter();
            owners
                .filter(|owner| owner.message_id == id)
                .map(|owner| owner.queued_generation)
                .next()
                .unwrap_or(item.queued_generation)
        };
        let sources = ids.into_iter().map(|id| SourceIdentity {
            message_id: id.get(),
            queued_generation: generation(id),
        });
        Self {
            primary: item.message_id.get(),
            queued_generation: item.queued_generation,
            sources: sources.collect(),
        }
    }
}

/// Reserved: no paste has started. MayHaveSent: a paste may have reached the pane. Unknown: it may
/// have, and nothing resolves that on its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum HeadPhase {
    Reserved,
    MayHaveSent,
    Unknown,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct PersistedHeadInjection {
    pub(crate) identity: QueueIdentity,
    pub(crate) original: PendingQueueItem,
    pub(crate) phase: HeadPhase,
    /// The paste this head may have become, named before it could start; only an older binary's
    /// record lacks it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) attempt: Option<mend::HeadAttempt>,
}

/// The part of a channel snapshot beyond its items; written only while one of them is set.
#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct SnapshotMeta {
    #[serde(default)]
    pub(crate) head_injection: Option<PersistedHeadInjection>,
    #[serde(default)]
    pub(crate) terminal_identities: Vec<QueueIdentity>,
}

impl SnapshotMeta {
    pub(crate) fn is_set(&self) -> bool {
        self.head_injection.is_some() || !self.terminal_identities.is_empty()
    }
}

pub(super) mod compat;
mod snapshot;
mod v2_types;
pub(super) use snapshot::{parse_snapshot, snapshot_json};

/// The head and submitted sources a queue file holds; `None` for a missing file or a legacy array,
/// an error for a file that cannot be read or parsed, legacy or not.
fn read_snapshot_meta(path: &Path) -> Result<Option<SnapshotMeta>, String> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(queue_unread(format!("read {}: {error}", path.display()))),
    };
    let (_, meta) = parse_snapshot(&content)
        .map_err(|e| queue_unread(format!("parse {}: {e}", path.display())))?;
    Ok(meta.filter(SnapshotMeta::is_set))
}

/// Whether an actorless force purge must keep this namespace's files: its queue file holds a head
/// or submitted sources, or cannot be read or parsed.
pub(super) fn purge_keeps(queue_file: &Path) -> bool {
    let reason = match read_snapshot_meta(queue_file) {
        Ok(None) => return false,
        Ok(Some(_)) => "the snapshot holds a head or submitted sources".to_string(),
        Err(error) => error,
    };
    let path = queue_file.display();
    tracing::warn!(%path, %reason, "force purge kept this namespace's pending queue files");
    true
}

/// The payload a reserved head is compared by: no clock or override field, every segment kept.
fn canonical_item(item: &Intervention, channel_id: ChannelId) -> PendingQueueItem {
    let epoch = std::time::SystemTime::UNIX_EPOCH;
    let mut canonical =
        pending_queue_item_from_intervention(item, channel_id, None, epoch, item.created_at);
    let segments = item.source_text_segments().into_iter();
    canonical.source_text_segments = (segments)
        .map(|segment| PendingQueueSourceTextSegment {
            message_id: segment.message_id.get(),
            text: segment.text,
        })
        .collect();
    canonical
}

/// The owner's share of a reservation; one only the actor holds is orphaned.
pub(crate) struct HeadLease;

/// The actor's copy of the channel's snapshot metadata, read from disk once per namespace.
#[derive(Default)]
pub(super) struct HeadLedger {
    adopted: Option<(ProviderKind, String)>,
    pub(super) meta: SnapshotMeta,
    lease: Option<Arc<HeadLease>>,
    pub(super) persisted_meta: Option<serde_json::Value>,
}

/// Reads this namespace's head and terminal record before the first write; a snapshot the
/// reader cannot classify refuses, so no write replaces state it never read.
pub(super) fn adopt(
    state: &mut ChannelMailboxState,
    channel_id: ChannelId,
    persistence: &QueuePersistenceContext,
) -> Result<(), String> {
    let ledger = &mut state.queued_head;
    let key = (persistence.provider.clone(), persistence.token_hash.clone());
    if ledger.adopted.as_ref() == Some(&key) {
        return Ok(());
    }
    if ledger.meta.is_set() {
        return Err(format!(
            "queued head record of channel {channel_id} belongs to another token namespace"
        ));
    }
    let (provider, token_hash) = (&persistence.provider, &persistence.token_hash);
    let path = pending_queue_file_path(provider, token_hash, channel_id)
        .ok_or_else(|| format!("pending queue root unavailable for channel {channel_id}"))?;
    // A file a clear could not rewrite holds no head or record this actor may adopt.
    ledger.meta = match state.remint_fence.trusts_queue(&key) {
        true => read_snapshot_meta(&path)?.unwrap_or_default(),
        false => SnapshotMeta::default(),
    };
    ledger.persisted_meta = meta_value(&ledger.meta);
    ledger.lease = None;
    ledger.adopted = Some(key);
    reap(ledger);
    Ok(())
}

pub(super) fn meta_value(meta: &SnapshotMeta) -> Option<serde_json::Value> {
    meta.is_set()
        .then(|| serde_json::to_value(meta).expect("snapshot metadata serializes"))
}

/// After a queue write landed: its metadata is what disk holds, and the namespace is trusted again.
pub(super) fn record_write(state: &mut ChannelMailboxState) {
    state.queued_head.persisted_meta = meta_value(&state.queued_head.meta);
    if let Some(namespace) = &state.queued_head.adopted {
        state.remint_fence.trust_queue(namespace);
    }
}

/// Refuses unless the metadata on disk is what this owner last wrote; a file a clear could not
/// rewrite is no record at all.
fn validate_disk(
    state: &mut ChannelMailboxState,
    channel: ChannelId,
    persistence: &QueuePersistenceContext,
) -> Result<(), String> {
    adopt(state, channel, persistence)?;
    let namespace = super::remint_fence::namespace(persistence);
    if !state.remint_fence.trusts_queue(&namespace) {
        return Ok(());
    }
    let path = pending_queue_file_path(&persistence.provider, &persistence.token_hash, channel)
        .ok_or("pending queue root unavailable")?;
    let meta = read_snapshot_meta(&path)?.unwrap_or_default();
    if meta_value(&meta) != state.queued_head.persisted_meta {
        return Err(format!(
            "channel {channel} snapshot metadata changed outside its owner"
        ));
    }
    Ok(())
}

pub(super) fn refuse_destructive(
    state: &mut ChannelMailboxState,
    channel: ChannelId,
    persistence: &QueuePersistenceContext,
) -> Result<(), String> {
    validate_disk(state, channel, persistence)?;
    reap(&mut state.queued_head);
    if let Some(head) = &state.queued_head.meta.head_injection {
        return Err(format!(
            "channel {channel} queued head is {:?}; purge refused",
            head.phase
        ));
    }
    Ok(())
}

/// An explicit clear drops the head and its lease without reading disk; the submitted sources this
/// owner recorded stay recorded, so none of them can replay. The clear's writer sets what disk holds.
pub(super) fn release_for_clear(
    state: &mut ChannelMailboxState,
    persistence: &QueuePersistenceContext,
) {
    let key = super::remint_fence::namespace(persistence);
    let previous = std::mem::take(&mut state.queued_head);
    let owned = previous.adopted.as_ref() == Some(&key);
    let terminal_identities = match owned {
        true => previous.meta.terminal_identities,
        false => Vec::new(),
    };
    state.queued_head = HeadLedger {
        adopted: Some(key),
        meta: SnapshotMeta {
            head_injection: None,
            terminal_identities,
        },
        lease: None,
        persisted_meta: None,
    };
}

/// Adds the submitted sources a trusted queue file records to the ledger before a clear rewrites
/// it; an unreadable file adds none.
pub(super) fn keep_disk_terminal(state: &mut ChannelMailboxState, path: &Path) {
    let disk = match read_snapshot_meta(path) {
        Ok(disk) => disk.unwrap_or_default(),
        Err(error) => {
            tracing::warn!(%error, "clear kept no submitted sources from an unreadable queue file");
            return;
        }
    };
    let terminal = &mut state.queued_head.meta.terminal_identities;
    for identity in disk.terminal_identities {
        if !terminal.contains(&identity) {
            terminal.push(identity);
        }
    }
}

/// A head nobody owns, after a restart or a dropped ticket: a reservation never pasted is
/// released, one that may have pasted becomes Unknown.
fn reap(ledger: &mut HeadLedger) {
    let orphaned = (ledger.lease.as_ref()).is_none_or(|lease| Arc::strong_count(lease) == 1);
    let phase = ledger.meta.head_injection.as_ref().map(|head| head.phase);
    match phase.filter(|_| orphaned) {
        Some(HeadPhase::Reserved) => ledger.meta.head_injection = None,
        Some(HeadPhase::MayHaveSent) => set_phase(ledger, HeadPhase::Unknown),
        Some(HeadPhase::Unknown) | None => return,
    }
    ledger.lease = None;
}

fn set_phase(ledger: &mut HeadLedger, phase: HeadPhase) {
    if let Some(head) = ledger.meta.head_injection.as_mut() {
        head.phase = phase;
    }
}

fn is_head(head: &PersistedHeadInjection, item: &Intervention, channel_id: ChannelId) -> bool {
    let payload = |item: &PendingQueueItem| {
        let mut item = item.clone();
        // Restoring queue age may fill absent source clocks; clocks are not frozen payload.
        for source in &mut item.source_message_queued_generations {
            source.enqueued_at_epoch_us = None;
        }
        serde_json::to_value(item).ok()
    };
    let same = |a: &PendingQueueItem, b: &PendingQueueItem| payload(a) == payload(b);
    QueueIdentity::of(item) == head.identity
        && same(&canonical_item(item, channel_id), &head.original)
}

/// Runs before every queue write: submitted sources leave every item and a reserved head must
/// still lead the queue unchanged, or the write is refused.
pub(super) fn prepare_write(
    state: &mut ChannelMailboxState,
    channel_id: ChannelId,
    persistence: &QueuePersistenceContext,
) -> Result<(), String> {
    validate_disk(state, channel_id, persistence)?;
    reap(&mut state.queued_head);
    strip_submitted_sources(state, channel_id);
    let Some(head) = state.queued_head.meta.head_injection.as_ref() else {
        return Ok(());
    };
    match state.intervention_queue.first() {
        Some(first) if is_head(head, first, channel_id) => Ok(()),
        _ => Err(format!(
            "queued head {} of channel {channel_id} is {:?}; a write that moves or changes it is refused",
            head.identity.primary, head.phase
        )),
    }
}

/// Removes from every queued item each source the terminal record names with its generation, so
/// a restored or merged item keeps its other sources; an item left with nothing is dropped.
fn strip_submitted_sources(state: &mut ChannelMailboxState, channel_id: ChannelId) {
    let terminal = &state.queued_head.meta.terminal_identities;
    let submitted: Vec<&SourceIdentity> = terminal.iter().flat_map(|t| &t.sources).collect();
    if submitted.is_empty() {
        return;
    }
    let mut stripped = 0usize;
    state.intervention_queue.retain_mut(|item| {
        let sources = QueueIdentity::of(item).sources;
        let done = sources.iter().filter(|source| submitted.contains(source));
        let done: Vec<MessageId> = done.map(|s| MessageId::new(s.message_id)).collect();
        for id in &done {
            strip_source_message_id_from_intervention(item, *id);
        }
        stripped += done.len();
        let empty = item.text.trim().is_empty()
            && item.pending_uploads.is_empty()
            && item.voice_announcement.is_none();
        done.is_empty() || !(item.source_message_ids.is_empty() || empty)
    });
    if stripped > 0 {
        let channel_id = channel_id.get();
        tracing::warn!(
            channel_id,
            stripped,
            "queued sources already submitted were removed"
        );
    }
}

/// Whether every source of `intervention` was already submitted under its generation.
pub(super) fn all_sources_submitted(state: &ChannelMailboxState, item: &Intervention) -> bool {
    let terminal = &state.queued_head.meta.terminal_identities;
    let submitted = |s: &SourceIdentity| terminal.iter().any(|t| t.sources.contains(s));
    !terminal.is_empty() && QueueIdentity::of(item).sources.iter().all(submitted)
}

/// Whether the reserved head leads the queue, so a full drain keeps one item.
pub(super) fn protected_len(
    state: &mut ChannelMailboxState,
    channel_id: ChannelId,
    persistence: &QueuePersistenceContext,
) -> usize {
    if adopt(state, channel_id, persistence).is_err() {
        return 0;
    }
    reap(&mut state.queued_head);
    let head = state.queued_head.meta.head_injection.as_ref();
    let first = state.intervention_queue.first();
    usize::from(
        head.zip(first)
            .is_some_and(|(h, f)| is_head(h, f, channel_id)),
    )
}

/// Whether the merge candidate is the reserved head, which no later input may join.
pub(super) fn holds_front(state: &ChannelMailboxState) -> bool {
    state.queued_head.meta.head_injection.is_some()
}

/// `TakeNextSoft`'s answer while a reserved or unresolved head holds the front, or the record
/// cannot be read; `None` lets it run.
pub(super) fn withhold_take(
    state: &mut ChannelMailboxState,
    channel_id: ChannelId,
    persistence: &QueuePersistenceContext,
) -> Option<TakeNextSoftResult> {
    let error = validate_disk(state, channel_id, persistence).err();
    reap(&mut state.queued_head);
    if error.is_none() && state.queued_head.meta.head_injection.is_none() {
        return None;
    }
    let mut withheld = super::injected_inputs::head_withheld(state);
    withheld.persistence_error = error;
    Some(withheld)
}

/// A turn claim never starts ahead of a reserved or unresolved head, nor over an unreadable record.
pub(super) fn blocks_claims(
    state: &mut ChannelMailboxState,
    channel_id: ChannelId,
    persistence: Option<&QueuePersistenceContext>,
) -> bool {
    if persistence.is_some_and(|p| validate_disk(state, channel_id, p).is_err()) {
        return true;
    }
    reap(&mut state.queued_head);
    state.queued_head.meta.head_injection.is_some()
}

pub(crate) mod mend;
pub(crate) mod prefix;

pub(super) use claim::step;
pub(crate) use claim::{HeadMsg, HeadOutcome, HeadSettle, QueuedHeadClaim};
#[cfg(test)]
pub(crate) use claim::{HeadTicket, downgrade_channel_snapshot};

/// The claim and settlement driven by the busy-TUI executor.
mod claim {
    use super::*;

    pub(crate) struct HeadTicket {
        lease: Arc<HeadLease>,
        identity: QueueIdentity,
        item: Box<Intervention>,
        attempt: Option<mend::HeadAttempt>,
    }

    impl HeadTicket {
        pub(crate) fn item(&self) -> &Intervention {
            &self.item
        }

        /// Names the paste about to start in `session_id`'s transcript, so the MayHaveSent settle
        /// records it with the head; a paste that cannot be named must not be typed.
        pub(crate) fn name_paste(
            &mut self,
            nonce: &str,
            transcript: &Path,
            session_id: Option<String>,
        ) -> Result<(), mend::AttemptUnavailable> {
            let session_id = session_id.ok_or(mend::AttemptUnavailable::Session)?;
            let attempt = mend::HeadAttempt::before_paste(nonce, transcript, session_id)?;
            self.attempt = Some(attempt);
            Ok(())
        }
    }

    type HeadChannelGate = fn(ChannelId, Option<ChannelId>) -> bool;
    type HeadAdmission = (ChannelId, Option<ChannelId>, HeadChannelGate);

    pub(crate) enum QueuedHeadClaim {
        Reserved(HeadTicket),
        NotEligible,
        Contended,
        BlockedUnknown,
        PersistenceFailed(String),
    }

    #[derive(Clone, Copy)]
    pub(crate) enum HeadOutcome {
        MayHaveSent,
        Injected,
        NotSent,
        Unconfirmed,
    }

    pub(crate) enum HeadSettle {
        Committed(Option<HeadTicket>),
        /// Nothing was written and the reservation stands.
        NotCommitted(HeadTicket, String),
        /// The head stays as Unknown; nothing retries it.
        Unknown,
        /// The ticket no longer names this actor's head.
        Stale,
    }

    pub(crate) enum HeadMsg {
        Claim {
            expected: QueueIdentity,
            persistence: QueuePersistenceContext,
            admission: Option<HeadAdmission>,
            reply: oneshot::Sender<QueuedHeadClaim>,
        },
        Settle {
            ticket: HeadTicket,
            outcome: HeadOutcome,
            persistence: QueuePersistenceContext,
            reply: oneshot::Sender<HeadSettle>,
        },
        Mend(mend::MendMsg),
    }

    impl HeadMsg {
        pub(in crate::services::turn_orchestrator) fn refuse(self, error: String) {
            match self {
                Self::Claim { reply, .. } => {
                    let _ = reply.send(QueuedHeadClaim::PersistenceFailed(error));
                }
                Self::Settle { ticket, reply, .. } => {
                    let _ = reply.send(HeadSettle::NotCommitted(ticket, error));
                }
                Self::Mend(msg) => msg.refuse(error),
            }
        }
    }

    pub(in crate::services::turn_orchestrator) fn step(
        state: &mut ChannelMailboxState,
        channel_id: ChannelId,
        msg: HeadMsg,
    ) {
        match msg {
            HeadMsg::Claim {
                expected,
                persistence,
                admission,
                reply,
            } => {
                let enabled = match admission {
                    Some((channel, parent, gate)) => channel == channel_id
                        && crate::services::discord::health::InjectOrigin::queued_head_gate_enabled(
                            &persistence.provider,
                            channel,
                            parent,
                            gate,
                        ),
                    None => cfg!(test),
                };
                if !enabled {
                    let _ = reply.send(QueuedHeadClaim::NotEligible);
                    return;
                }
                state.last_persistence = Some(persistence.clone());
                let _ = reply.send(claim(state, channel_id, &expected, &persistence));
            }
            HeadMsg::Settle {
                ticket,
                outcome,
                persistence,
                reply,
            } => {
                state.last_persistence = Some(persistence.clone());
                let _ = reply.send(settle(state, channel_id, ticket, outcome, &persistence));
            }
            HeadMsg::Mend(msg) => mend::step(state, channel_id, msg),
        }
    }

    /// Reserves the first ready input when it is still `expected`, eligible and unowned. Parked
    /// inputs ahead of it move behind it in the one write that makes the reservation durable.
    fn claim(
        state: &mut ChannelMailboxState,
        channel_id: ChannelId,
        expected: &QueueIdentity,
        persistence: &QueuePersistenceContext,
    ) -> QueuedHeadClaim {
        if let Err(error) = adopt(state, channel_id, persistence) {
            return QueuedHeadClaim::PersistenceFailed(error);
        }
        reap(&mut state.queued_head);
        match state
            .queued_head
            .meta
            .head_injection
            .as_ref()
            .map(|h| h.phase)
        {
            Some(HeadPhase::Reserved) => return QueuedHeadClaim::Contended,
            Some(_) => return QueuedHeadClaim::BlockedUnknown,
            None => {}
        }
        if let Some(error) = absorb_disk_queue_error(state, channel_id, persistence) {
            return QueuedHeadClaim::PersistenceFailed(error);
        }
        let (provider, token_hash) = (&persistence.provider, persistence.token_hash.as_str());
        let namespace = super::super::remint_fence::namespace(persistence);
        let marker = state.remint_fence.trusts_marker(&namespace).then(|| {
            super::super::pending_queue_persistence::load_channel_pending_dispatch_marker_checked(
                provider, token_hash, channel_id,
            )
        });
        let marker_held = match marker {
            Some(Ok(marker)) => marker.is_some(),
            Some(Err(error)) => return QueuedHeadClaim::PersistenceFailed(error),
            None => false,
        };
        let owned = marker_held
            || state.pending_user_dispatch.is_some()
            || super::super::injected_inputs::holds_order(state);
        if state.intervention_queue.is_empty() {
            return QueuedHeadClaim::NotEligible;
        }
        // Ownership outranks retry state: an owned queue is contended whatever is parked in it.
        if owned {
            return QueuedHeadClaim::Contended;
        }
        // Read again under the actor: what the caller saw as parked or ready may have changed.
        use crate::services::discord::busy_followup_retry_store::input_class;
        let class = |item: &Intervention| input_class(provider, channel_id, item);
        let prefix::Prefix::Ready { at } = prefix::select(&state.intervention_queue, class) else {
            return QueuedHeadClaim::NotEligible;
        };
        let head = &state.intervention_queue[at];
        let identity = QueueIdentity::of(head);
        if identity != *expected {
            return QueuedHeadClaim::Contended;
        }
        // The first ready input decides alone; a later one never stands in for it.
        if !prefix::eligible(head) {
            return QueuedHeadClaim::NotEligible;
        }
        let item = Box::new(head.clone());
        let original = canonical_item(head, channel_id);
        let lease = Arc::new(HeadLease);
        let previous_queue = state.intervention_queue.clone();
        let previous_meta = state.queued_head.meta.clone();
        state.intervention_queue[..=at].rotate_right(1);
        let ledger = &mut state.queued_head;
        let phase = HeadPhase::Reserved;
        ledger.meta.head_injection = Some(PersistedHeadInjection {
            identity: identity.clone(),
            original,
            phase,
            attempt: None,
        });
        ledger.lease = Some(lease.clone());
        if let Err(error) = persist_queue(state, channel_id, persistence) {
            state.intervention_queue = previous_queue;
            state.queued_head.meta = previous_meta;
            state.queued_head.lease = None;
            return QueuedHeadClaim::PersistenceFailed(error);
        }
        QueuedHeadClaim::Reserved(HeadTicket {
            lease,
            identity,
            item,
            attempt: None,
        })
    }

    /// Moves the ticket's head on. A write that may have landed after a paste could have started
    /// leaves the head Unknown, which holds the queue until an explicit clear discards it.
    fn settle(
        state: &mut ChannelMailboxState,
        channel_id: ChannelId,
        ticket: HeadTicket,
        outcome: HeadOutcome,
        persistence: &QueuePersistenceContext,
    ) -> HeadSettle {
        let ledger = &state.queued_head;
        let own = ledger
            .lease
            .as_ref()
            .is_some_and(|l| Arc::ptr_eq(l, &ticket.lease));
        let head = ledger.meta.head_injection.as_ref();
        let Some(phase) = head
            .filter(|h| own && h.identity == ticket.identity)
            .map(|h| h.phase)
        else {
            return HeadSettle::Stale;
        };
        let previous_queue = state.intervention_queue.clone();
        let previous_meta = state.queued_head.meta.clone();
        match (outcome, phase) {
            (HeadOutcome::MayHaveSent, HeadPhase::Reserved) => {
                set_phase(&mut state.queued_head, HeadPhase::MayHaveSent);
                if let Some(head) = state.queued_head.meta.head_injection.as_mut() {
                    head.attempt = ticket.attempt.clone();
                }
            }
            (HeadOutcome::Injected, HeadPhase::MayHaveSent) => {
                state.intervention_queue.remove(0);
                state.queued_head.meta.head_injection = None;
                let terminal = &mut state.queued_head.meta.terminal_identities;
                terminal.push(ticket.identity.clone());
            }
            (HeadOutcome::NotSent, _) => state.queued_head.meta.head_injection = None,
            (HeadOutcome::Unconfirmed, _) => return unknown(state, channel_id, persistence),
            _ => return HeadSettle::NotCommitted(ticket, "phase out of order".to_string()),
        }
        let written = persist_queue(state, channel_id, persistence);
        state.queued_head.lease = None;
        match written {
            Ok(()) if state.queued_head.meta.head_injection.is_some() => {
                state.queued_head.lease = Some(ticket.lease.clone());
                HeadSettle::Committed(Some(ticket))
            }
            Ok(()) => HeadSettle::Committed(None),
            Err(error) => {
                state.intervention_queue = previous_queue;
                state.queued_head.meta = previous_meta;
                log_queue_persistence_rollback("queued_head", channel_id, persistence, &error);
                if phase == HeadPhase::Reserved && !matches!(outcome, HeadOutcome::MayHaveSent) {
                    state.queued_head.lease = Some(ticket.lease.clone());
                    return HeadSettle::NotCommitted(ticket, error);
                }
                unknown(state, channel_id, persistence)
            }
        }
    }

    fn unknown(
        state: &mut ChannelMailboxState,
        channel_id: ChannelId,
        persistence: &QueuePersistenceContext,
    ) -> HeadSettle {
        set_phase(&mut state.queued_head, HeadPhase::Unknown);
        state.queued_head.lease = None;
        if let Err(error) = persist_queue(state, channel_id, persistence) {
            tracing::error!(channel_id = channel_id.get(), %error, "queued head Unknown not written");
        }
        HeadSettle::Unknown
    }

    impl ChannelMailboxHandle {
        #[cfg(test)]
        pub(crate) async fn claim_queued_head(
            &self,
            expected: QueueIdentity,
            persistence: QueuePersistenceContext,
        ) -> QueuedHeadClaim {
            self.request_head_claim(expected, persistence, None).await
        }

        pub(crate) async fn claim_queued_head_if_enabled(
            &self,
            expected: QueueIdentity,
            persistence: QueuePersistenceContext,
            channel: ChannelId,
            parent: Option<ChannelId>,
            gate: HeadChannelGate,
        ) -> QueuedHeadClaim {
            self.request_head_claim(expected, persistence, Some((channel, parent, gate)))
                .await
        }

        async fn request_head_claim(
            &self,
            expected: QueueIdentity,
            persistence: QueuePersistenceContext,
            admission: Option<HeadAdmission>,
        ) -> QueuedHeadClaim {
            self.request(|reply| {
                head_msg(HeadMsg::Claim {
                    expected,
                    persistence,
                    admission,
                    reply,
                })
            })
            .await
            .unwrap_or(QueuedHeadClaim::PersistenceFailed("unreachable".into()))
        }

        pub(crate) async fn settle_queued_head(
            &self,
            ticket: HeadTicket,
            outcome: HeadOutcome,
            persistence: QueuePersistenceContext,
        ) -> HeadSettle {
            self.request(|reply| {
                head_msg(HeadMsg::Settle {
                    ticket,
                    outcome,
                    persistence,
                    reply,
                })
            })
            .await
            .unwrap_or(HeadSettle::Unknown)
        }
    }

    /// Offline, gate off: rewrites a V1 snapshot as the legacy array an older binary reads, refused
    /// while a head is held, a marker stands or a submitted source is not `replay_safe`.
    #[cfg(test)]
    pub(crate) fn downgrade_channel_snapshot(
        provider: &ProviderKind,
        token_hash: &str,
        channel_id: ChannelId,
        replay_safe: impl Fn(&QueueIdentity) -> bool,
    ) -> Result<bool, String> {
        let path = pending_queue_file_path(provider, token_hash, channel_id)
            .ok_or_else(|| "pending queue root unavailable".to_string())?;
        let content = match std::fs::read_to_string(&path) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.to_string()),
        };
        let (items, Some(meta)) = parse_snapshot(&content)? else {
            return Ok(false);
        };
        if let Some(head) = meta.head_injection {
            return Err(format!(
                "queued head {} is {:?}",
                head.identity.primary, head.phase
            ));
        }
        let marker = load_channel_pending_dispatch_marker_checked(provider, token_hash, channel_id);
        if marker?.is_some() {
            return Err("a dispatch marker stands".to_string());
        }
        if let Some(unsafe_terminal) = meta.terminal_identities.iter().find(|t| !replay_safe(t)) {
            return Err(format!(
                "submitted source {} may replay",
                unsafe_terminal.primary
            ));
        }
        if items.is_empty() {
            return std::fs::remove_file(&path)
                .map(|()| true)
                .map_err(|e| e.to_string());
        }
        let json = serde_json::to_string_pretty(&items).map_err(|e| e.to_string())?;
        let context = crate::services::discord::runtime_store::AtomicWriteContext::new("downgrade");
        crate::services::discord::runtime_store::critical_atomic_write(&path, &json, context)?;
        Ok(true)
    }

    fn head_msg(msg: HeadMsg) -> ChannelMailboxMsg {
        ChannelMailboxMsg::Injection(super::super::injected_inputs::InjectionMsg::Head(msg))
    }
}

#[cfg(test)]
#[path = "queued_head_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "queued_head_s2a_tests.rs"]
mod s2a_tests;

#[cfg(test)]
#[path = "queued_head/compat_tests.rs"]
mod compat_tests;
