//! A channel's queue purge: the forced cancel's whole-queue purge, and the handle requests that
//! report an actor that never answered instead of an empty answer.

use super::*;

/// The `PurgeQueue` arm, which also releases an already-cancelled anchor when asked.
pub(super) fn purge_queue(
    mut state: ChannelMailboxState,
    channel_id: ChannelId,
    persistence: QueuePersistenceContext,
    clear_cancelled_active_anchor: bool,
    reply: oneshot::Sender<PurgeQueueResult>,
) -> ChannelMailboxState {
    if let Err(error) = queued_head::refuse_destructive(&mut state, channel_id, &persistence) {
        tracing::warn!(%channel_id, %error, "queued head refused mailbox purge");
        let _ = reply.send(PurgeQueueResult {
            queued_head_refused: true,
            own_files_removed: Some(0),
            queue_len_after: state.intervention_queue.len(),
            ..Default::default()
        });
        return state;
    }
    // Queue only: a turn that entered between the force-kill and this purge keeps its anchor;
    // a force purge also releases an anchor whose token the kill already cancelled.
    let cleared_active_anchor = if clear_cancelled_active_anchor
        && state
            .cancel_token
            .as_ref()
            .is_some_and(|token| token.cancelled.load(std::sync::atomic::Ordering::Relaxed))
    {
        release_active_turn_anchor(&mut state, channel_id, Some(&persistence.provider));
        true
    } else {
        false
    };
    state.last_persistence = Some(persistence.clone());
    let disk_files_removed =
        remove_channel_pending_queue_files_all_tokens(&persistence.provider, channel_id);
    let own_files_present =
        channel_queue_files_present(&persistence.provider, &persistence.token_hash, channel_id);
    let previous_queue = state.intervention_queue.clone();
    let keep = queued_head::protected_len(&mut state, channel_id, &persistence);
    let drained = state.intervention_queue.drain(keep..).count();
    let purge_persisted = persist_queue_or_restore(
        &mut state,
        channel_id,
        &persistence,
        previous_queue,
        "purge_queue",
    )
    .is_ok();
    let drained = if purge_persisted { drained } else { 0 };
    if purge_persisted {
        clear_pending_user_dispatch(&mut state);
        state.recently_valve_cleared_dispatch = None;
        delete_pending_dispatch_marker_with_persistence(&persistence, channel_id, "purge_queue");
    }
    if cleared_active_anchor {
        mark_turn_finished_signal_done(channel_id);
    }
    let _ = reply.send(PurgeQueueResult {
        drained,
        disk_files_removed,
        cleared_active_anchor,
        own_files_removed: if purge_persisted {
            own_files_present
        } else {
            Some(0)
        },
        queue_len_after: state.intervention_queue.len(),
        queued_head_refused: false,
    });
    state
}

impl ChannelMailboxHandle {
    /// Like `snapshot`, but an actor that never answered is an error, not an empty queue.
    pub(crate) async fn try_snapshot(&self) -> Result<ChannelMailboxSnapshot, MailboxUnreachable> {
        self.request(|reply| ChannelMailboxMsg::Snapshot { reply })
            .await
    }

    /// Like `purge_queue`, but an actor that never answered is an error, not zero counts.
    pub(crate) async fn try_purge_queue(
        &self,
        persistence: QueuePersistenceContext,
        clear_cancelled_active_anchor: bool,
    ) -> Result<PurgeQueueResult, MailboxUnreachable> {
        self.request(|reply| ChannelMailboxMsg::PurgeQueue {
            persistence,
            clear_cancelled_active_anchor,
            reply,
        })
        .await
    }
}

#[cfg(unix)]
pub(crate) use herdr::ExpectedQueue;
#[cfg(unix)]
pub(super) use herdr::{CheckedPurgeRequest, OwnCaptureRequest};

/// Arms a failure returned after the next queue save of `channel` is already published.
#[cfg(all(test, unix))]
pub(crate) fn fail_next_publication_for_test(channel: ChannelId) {
    super::pending_queue_persistence::post_save_fault::fail_next(channel);
}

/// The forced Herdr cancel's capture and purge: it removes only the queue the cancel captured from
/// this actor and answers with what it actually removed.
#[cfg(unix)]
mod herdr {
    use std::time::{Instant, SystemTime};

    use super::super::pending_queue_persistence::{
        PendingQueueItem, load_channel_pending_dispatch_marker_checked, pending_queue_file_path,
        pending_queue_item_from_intervention, remove_channel_pending_dispatch_marker,
    };
    use super::super::queued_head::{SnapshotMeta, meta_value, parse_snapshot, record_write};
    use super::*;

    /// The queue and dispatch reservation a cancel read from the actor before asking to purge it.
    #[derive(Clone)]
    pub(crate) struct ExpectedQueue {
        queue: Vec<Intervention>,
        reserved: Option<MessageId>,
        reserved_sources: Vec<MessageId>,
    }

    impl ExpectedQueue {
        pub(crate) fn of(snapshot: &ChannelMailboxSnapshot) -> Self {
            Self {
                queue: snapshot.intervention_queue.clone(),
                reserved: snapshot.pending_user_dispatch,
                reserved_sources: snapshot.pending_user_dispatch_source_ids.clone(),
            }
        }

        /// Why the actor no longer holds this queue: the order, every stored field of each item
        /// as held (nothing derived or filled in) and the dispatch reservation.
        fn differs(&self, state: &ChannelMailboxState) -> Option<String> {
            if (
                state.pending_user_dispatch,
                &state.pending_user_dispatch_source_ids,
            ) != (self.reserved, &self.reserved_sources)
            {
                return Some("the dispatch reservation changed since the capture".into());
            }
            let now = &state.intervention_queue;
            let same = self.queue.len() == now.len()
                && self
                    .queue
                    .iter()
                    .zip(now)
                    .all(|(held, now)| same_item(held, now));
            (!same).then(|| "the queue changed since the capture".into())
        }
    }

    fn same_item(a: &Intervention, b: &Intervention) -> bool {
        let sources = |item: &Intervention| -> Vec<_> {
            let generations = item.source_message_queued_generations.iter();
            let generation = |s: &SourceMessageQueuedGeneration| {
                let clock = s.enqueued_at_epoch_us;
                (
                    s.message_id,
                    s.queued_generation,
                    clock,
                    s.preserve_on_cancel,
                    s.route,
                )
            };
            generations.map(generation).collect()
        };
        let segments = |item: &Intervention| -> Vec<_> {
            let segments = item.source_text_segments.iter();
            segments.map(|s| (s.message_id, s.text.clone())).collect()
        };
        let head = |i: &Intervention| {
            (
                i.author_id,
                i.author_is_bot,
                i.message_id,
                i.queued_generation,
            )
        };
        let body = |i: &Intervention| {
            (
                i.text.clone(),
                i.mode,
                i.created_at,
                i.reply_context.clone(),
            )
        };
        let flags = |i: &Intervention| (i.has_reply_boundary, i.merge_consecutive);
        (head(a), &a.source_message_ids, body(a), flags(a))
            == (head(b), &b.source_message_ids, body(b), flags(b))
            && sources(a) == sources(b)
            && segments(a) == segments(b)
            && a.pending_uploads == b.pending_uploads
    }

    /// An item as its queue file records it, clocks, channel name and override left out: the file
    /// is compared only to find a writer other than this actor.
    fn filed(mut item: PendingQueueItem) -> Option<serde_json::Value> {
        item.created_at_wall_time_ms = None;
        item.channel_name = None;
        item.override_channel_id = None;
        for source in &mut item.source_message_queued_generations {
            source.enqueued_at_epoch_us = None;
        }
        serde_json::to_value(item).ok()
    }

    /// Where a checked purge stopped; no stage after it ran.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum PurgeStage {
        /// A purge-closed actor: its channel state now belongs to a successor.
        Closed,
        /// Nothing changed: a reserved head, a changed queue, or a namespace or file that differs.
        Refused,
        /// The queue save failed before it published; nothing changed.
        NotSaved,
        /// The queue save failed and its file could not be matched to either queue.
        SaveUnknown,
        /// The queue save returned an error after it published; the removal stands.
        SavedWithError,
        /// The queue was saved, but the dispatch marker could not be removed.
        Marker,
    }

    /// What a checked purge removed and, if it stopped early, where and why.
    pub(crate) struct CheckedPurge {
        /// Items this purge took out of the queue, then an unreserved dispatch marker's input.
        pub(crate) receipt: Vec<Intervention>,
        pub(crate) failed: Option<(PurgeStage, String)>,
        pub(crate) queue_len_after: usize,
    }

    impl CheckedPurge {
        fn stopped(stage: PurgeStage, reason: String, queue_len_after: usize) -> Self {
            Self {
                receipt: Vec::new(),
                failed: Some((stage, reason)),
                queue_len_after,
            }
        }

        /// Whether the actor kept its queue: true only when nothing changed, `None` when unknown.
        pub(crate) fn queue_preserved(&self) -> Option<bool> {
            match self.failed {
                None => Some(false),
                Some((PurgeStage::Closed | PurgeStage::Refused | PurgeStage::NotSaved, _)) => {
                    Some(true)
                }
                Some((PurgeStage::SaveUnknown, _)) => None,
                Some((PurgeStage::SavedWithError | PurgeStage::Marker, _)) => Some(false),
            }
        }
    }

    pub(in crate::services::turn_orchestrator) struct CheckedPurgeRequest {
        persistence: QueuePersistenceContext,
        expected: ExpectedQueue,
        reply: oneshot::Sender<CheckedPurge>,
    }

    impl CheckedPurgeRequest {
        /// A closed actor's explicit answer, never an empty success.
        pub(in crate::services::turn_orchestrator) fn refuse_closed(self) -> &'static str {
            let reason = "the mailbox actor is purge-closed".to_string();
            let _ = (self.reply).send(CheckedPurge::stopped(PurgeStage::Closed, reason, 0));
            "PurgeQueueIfUnchanged"
        }

        pub(in crate::services::turn_orchestrator) fn run(
            self,
            state: &mut ChannelMailboxState,
            channel_id: ChannelId,
        ) {
            let purge = purge_if_unchanged(state, channel_id, &self.persistence, &self.expected);
            let _ = self.reply.send(purge);
        }
    }

    /// The forced cancel's capture, taken once the actor absorbed its own queue file.
    pub(in crate::services::turn_orchestrator) struct OwnCaptureRequest {
        persistence: QueuePersistenceContext,
        reply: oneshot::Sender<Result<ChannelMailboxSnapshot, String>>,
    }

    impl OwnCaptureRequest {
        pub(in crate::services::turn_orchestrator) fn refuse_closed(self) -> &'static str {
            let _ = (self.reply).send(Err("the mailbox actor is purge-closed".into()));
            "CaptureOwnQueue"
        }

        /// Another namespace is refused before any read; a file the actor cannot read or trust
        /// absorbs nothing, and a read error answers no capture.
        pub(in crate::services::turn_orchestrator) fn run(
            self,
            state: &mut ChannelMailboxState,
            channel_id: ChannelId,
        ) {
            let absorb = super::super::dispatch_reservation::absorb_disk_queue_error;
            let captured = own_namespace(state, &self.persistence).and_then(|()| {
                match absorb(state, channel_id, &self.persistence) {
                    Some(error) => Err(error),
                    None => Ok(state.snapshot()),
                }
            });
            let _ = self.reply.send(captured);
        }
    }

    impl ChannelMailboxHandle {
        /// Absorbs this actor's queue file of `persistence`'s namespace, then answers its queue;
        /// the inner error is a refusal or a failed read, after which nothing was captured.
        pub(crate) async fn try_capture_own_queue(
            &self,
            persistence: QueuePersistenceContext,
        ) -> Result<Result<ChannelMailboxSnapshot, String>, MailboxUnreachable> {
            self.request(|reply| {
                let request = OwnCaptureRequest { persistence, reply };
                ChannelMailboxMsg::CaptureOwnQueue(request)
            })
            .await
        }

        /// Purges this actor's queue only while it is still `expected`; unreachable is an error.
        pub(crate) async fn try_purge_queue_if_unchanged(
            &self,
            persistence: QueuePersistenceContext,
            expected: ExpectedQueue,
        ) -> Result<CheckedPurge, MailboxUnreachable> {
            self.request(|reply| {
                let request = CheckedPurgeRequest {
                    persistence,
                    expected,
                    reply,
                };
                ChannelMailboxMsg::PurgeQueueIfUnchanged(request)
            })
            .await
        }
    }

    /// The namespace, the queued-head fence, the capture and the queue file are checked before
    /// anything changes; a save is judged by the file it left, and a reservation is never touched.
    fn purge_if_unchanged(
        state: &mut ChannelMailboxState,
        channel_id: ChannelId,
        persistence: &QueuePersistenceContext,
        expected: &ExpectedQueue,
    ) -> CheckedPurge {
        let refused = |state: &ChannelMailboxState, reason: String| {
            tracing::warn!(%channel_id, %reason, "checked purge refused; mailbox preserved");
            let len = state.intervention_queue.len();
            CheckedPurge::stopped(PurgeStage::Refused, reason, len)
        };
        // The namespace is compared first: the head fence adopts the namespace it is given.
        if let Err(error) = own_namespace(state, persistence) {
            return refused(state, error);
        }
        if let Err(error) = queued_head::refuse_destructive(state, channel_id, persistence) {
            return refused(state, error);
        }
        if let Some(change) = expected.differs(state) {
            return refused(state, change);
        }
        let marker = match own_files(state, channel_id, persistence) {
            Ok(marker) => marker,
            Err(error) => return refused(state, error),
        };
        let keep = queued_head::protected_len(state, channel_id, persistence);
        let previous = state.intervention_queue.clone();
        let mut receipt: Vec<Intervention> = state.intervention_queue.drain(keep..).collect();
        state.last_persistence = Some(persistence.clone());
        if let Err(error) = persist_queue(state, channel_id, persistence) {
            let saved = published(state, channel_id, persistence, &previous);
            if saved == Some(true) {
                record_write(state);
                let len = state.intervention_queue.len();
                let failed = Some((PurgeStage::SavedWithError, error));
                return CheckedPurge {
                    receipt,
                    failed,
                    queue_len_after: len,
                };
            }
            state.intervention_queue = previous;
            log_queue_persistence_rollback("checked_purge", channel_id, persistence, &error);
            let stage = saved.map_or(PurgeStage::SaveUnknown, |_| PurgeStage::NotSaved);
            return CheckedPurge::stopped(stage, error, state.intervention_queue.len());
        }
        let queue_len_after = state.intervention_queue.len();
        if let Some(marker) = marker {
            let (provider, token) = (&persistence.provider, &persistence.token_hash);
            if let Err(error) = remove_channel_pending_dispatch_marker(provider, token, channel_id)
            {
                let failed = Some((PurgeStage::Marker, error));
                return CheckedPurge {
                    receipt,
                    failed,
                    queue_len_after,
                };
            }
            state.recently_valve_cleared_dispatch = None;
            if !receipt
                .iter()
                .any(|item| item.message_id == marker.message_id)
            {
                receipt.push(marker);
            }
        }
        CheckedPurge {
            receipt,
            failed: None,
            queue_len_after,
        }
    }

    /// The capture names a token namespace, and it is the one this actor writes.
    fn own_namespace(
        state: &ChannelMailboxState,
        persistence: &QueuePersistenceContext,
    ) -> Result<(), String> {
        if persistence.token_hash.is_empty() {
            return Err("the captured session names no token namespace".into());
        }
        let namespace = super::super::remint_fence::namespace;
        match &state.last_persistence {
            Some(actor) if namespace(actor) != namespace(persistence) => {
                Err("the captured namespace is not the one this actor writes".into())
            }
            _ => Ok(()),
        }
    }

    /// The queue file of the namespace holds the actor's queue; returns the input of a dispatch
    /// marker no reservation holds, which the purge removes.
    fn own_files(
        state: &mut ChannelMailboxState,
        channel_id: ChannelId,
        persistence: &QueuePersistenceContext,
    ) -> Result<Option<Intervention>, String> {
        let namespace = super::super::remint_fence::namespace(persistence);
        if state.remint_fence.trusts_queue(&namespace) {
            let (items, _) = queue_file(channel_id, persistence)?;
            let filed_items: Vec<_> = items.into_iter().map(filed).collect();
            if filed_items != filed_queue(&state.intervention_queue, channel_id) {
                return Err("the queue file differs from the actor's queue".into());
            }
        }
        if state.pending_user_dispatch.is_some() {
            return Ok(None);
        }
        let (provider, token) = (&persistence.provider, &persistence.token_hash);
        let marker = load_channel_pending_dispatch_marker_checked(provider, token, channel_id)?;
        Ok(marker.map(|(item, _)| item))
    }

    fn filed_queue(
        queue: &[Intervention],
        channel_id: ChannelId,
    ) -> Vec<Option<serde_json::Value>> {
        let (wall, instant) = (SystemTime::now(), Instant::now());
        let item =
            |item| pending_queue_item_from_intervention(item, channel_id, None, wall, instant);
        queue.iter().map(item).map(filed).collect()
    }

    /// The channel's queue file in `persistence`'s namespace: a missing file is an empty queue,
    /// one that cannot be read or parsed is an error.
    fn queue_file(
        channel_id: ChannelId,
        persistence: &QueuePersistenceContext,
    ) -> Result<(Vec<PendingQueueItem>, Option<SnapshotMeta>), String> {
        let (provider, token) = (&persistence.provider, &persistence.token_hash);
        let path = pending_queue_file_path(provider, token, channel_id)
            .ok_or_else(|| format!("pending queue root unavailable for channel {channel_id}"))?;
        match std::fs::read_to_string(&path) {
            Ok(content) => parse_snapshot(&content),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok((Vec::new(), None)),
            Err(error) => Err(format!("read {}: {error}", path.display())),
        }
    }

    /// After a save error, whether the file holds the drained queue (`true`), still the previous
    /// one (`false`), or neither or cannot be read (`None`).
    fn published(
        state: &ChannelMailboxState,
        channel_id: ChannelId,
        persistence: &QueuePersistenceContext,
        previous: &[Intervention],
    ) -> Option<bool> {
        let (items, meta) = queue_file(channel_id, persistence).ok()?;
        let filed_items: Vec<_> = items.into_iter().map(filed).collect();
        let queue = |queue: &[Intervention]| filed_queue(queue, channel_id);
        let meta = meta_value(&meta.unwrap_or_default());
        if filed_items == queue(&state.intervention_queue)
            && meta == meta_value(&state.queued_head.meta)
        {
            return Some(true);
        }
        (filed_items == queue(previous)).then_some(false)
    }
}

#[cfg(all(test, unix))]
mod stage_tests;
