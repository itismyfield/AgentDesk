use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use poise::serenity_prelude as serenity;
use serenity::{ChannelId, MessageId, UserId};
use tokio::sync::{Notify, mpsc, oneshot};

use crate::services::provider::{CancelToken, ProviderKind};

// #3293: non-creating registry lookup + operator-gated idle-entry purge.
mod pending_queue_persistence;
pub(crate) mod registry_purge;
pub(crate) use pending_queue_persistence::{
    PendingQueueItem, cleanup_stale_pending_queue_tmp_files_all_tokens, load_pending_queues,
    remove_channel_pending_queue_files_all_tokens, save_channel_queue,
    warn_legacy_pending_queue_files,
};
#[cfg(test)]
use pending_queue_persistence::{
    cleanup_stale_pending_queue_tmp_files_in_dir, cleanup_stale_pending_queue_tmp_files_under_root,
    load_channel_pending_queue,
};
use pending_queue_persistence::{
    load_channel_pending_queue_with_marker_recovery, remove_channel_pending_dispatch_marker,
    save_channel_pending_dispatch_marker,
};

pub(crate) const MAX_INTERVENTIONS_PER_CHANNEL: usize = 30;
pub(crate) const INTERVENTION_DEDUP_WINDOW: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InterventionMode {
    Soft,
}

#[derive(Clone, Debug)]
pub(crate) struct Intervention {
    pub(crate) author_id: UserId,
    pub(crate) author_is_bot: bool,
    pub(crate) message_id: MessageId,
    pub(crate) source_message_ids: Vec<MessageId>,
    pub(crate) text: String,
    pub(crate) mode: InterventionMode,
    pub(crate) created_at: Instant,
    pub(crate) reply_context: Option<String>,
    pub(crate) has_reply_boundary: bool,
    pub(crate) merge_consecutive: bool,
    pub(crate) pending_uploads: Vec<String>,
    /// #2266: when a voice-transcript announcement loses the
    /// `mailbox_try_start_turn` race and is enqueued for later dispatch, the
    /// per-process `voice::announce_meta` store entry is consumed by the
    /// original `handle_text_message` call before the race-loss branch runs.
    /// Embedding the full announcement here keeps the queued payload
    /// self-contained so the dispatch path (which reinserts the entry into
    /// the store before re-entering `handle_text_message`) can reconstruct
    /// the voice-transcript framing instead of falling back to plain text.
    /// `None` for non-voice paths.
    pub(crate) voice_announcement: Option<crate::voice::prompt::VoiceTranscriptAnnouncement>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum QueueExitKind {
    Cancelled,
    // #3177: age-eviction was removed (queued user input must never expire), so
    // nothing constructs this arm anymore; the display contract in
    // `discord::queue_exit_feedback_emoji`/`queue_exit_card_body` still handles
    // it, so it is kept as a stable feedback-surface variant.
    #[allow(dead_code)]
    Expired,
    Superseded,
}

#[derive(Clone, Debug)]
pub(crate) struct QueueExitEvent {
    pub(crate) intervention: Intervention,
    pub(crate) kind: QueueExitKind,
}

impl QueueExitEvent {
    fn new(intervention: Intervention, kind: QueueExitKind) -> Self {
        Self { intervention, kind }
    }
}

fn prune_interventions(queue: &mut Vec<Intervention>) -> Vec<QueueExitEvent> {
    prune_interventions_at(queue, Instant::now())
}

fn prune_interventions_at(queue: &mut Vec<Intervention>, now: Instant) -> Vec<QueueExitEvent> {
    // #3177: queued user messages are never age-evicted. A busy turn can hold a
    // reply in the queue well past the old 10-minute TTL, and silently dropping
    // it (the previous `Expired` retain) lost real user input. Only the
    // MAX_INTERVENTIONS_PER_CHANNEL overflow cap still bounds the queue.
    let _ = now;
    let mut queue_exit_events = Vec::new();
    if queue.len() > MAX_INTERVENTIONS_PER_CHANNEL {
        let overflow = queue.len() - MAX_INTERVENTIONS_PER_CHANNEL;
        queue_exit_events.extend(
            queue
                .drain(0..overflow)
                .map(|intervention| QueueExitEvent::new(intervention, QueueExitKind::Superseded)),
        );
    }
    queue_exit_events
}

fn intervention_age_since(last: &Intervention, current: &Intervention) -> Duration {
    current
        .created_at
        .checked_duration_since(last.created_at)
        .unwrap_or_default()
}

fn ensure_source_message_ids(intervention: &mut Intervention) {
    if intervention.source_message_ids.is_empty() {
        intervention
            .source_message_ids
            .push(intervention.message_id);
    }
}

fn push_unique_message_ids(
    existing: &mut Vec<MessageId>,
    incoming: impl IntoIterator<Item = MessageId>,
) {
    for message_id in incoming {
        if !existing.contains(&message_id) {
            existing.push(message_id);
        }
    }
}

fn should_merge_intervention(last: &Intervention, incoming: &Intervention) -> bool {
    last.mode == InterventionMode::Soft
        && incoming.mode == InterventionMode::Soft
        && last.merge_consecutive
        && incoming.merge_consecutive
        && last.author_id == incoming.author_id
        && !last.has_reply_boundary
        && !incoming.has_reply_boundary
}

pub(crate) fn enqueue_intervention(
    queue: &mut Vec<Intervention>,
    mut intervention: Intervention,
) -> EnqueueInterventionResult {
    let mut queue_exit_events = prune_interventions(queue);
    ensure_source_message_ids(&mut intervention);

    if queue
        .iter()
        .any(|item| item.source_message_ids.contains(&intervention.message_id))
    {
        return EnqueueInterventionResult {
            enqueued: false,
            merged: false,
            refusal_reason: Some(EnqueueRefusalReason::SourceIdAlreadyQueued),
            queue_exit_events,
            persistence_error: None,
        };
    }

    if let Some(last) = queue.last() {
        if last.author_id == intervention.author_id
            && last.text == intervention.text
            && last.reply_context == intervention.reply_context
            && last.has_reply_boundary == intervention.has_reply_boundary
            && last.pending_uploads == intervention.pending_uploads
            && intervention_age_since(last, &intervention) <= INTERVENTION_DEDUP_WINDOW
        {
            return EnqueueInterventionResult {
                enqueued: false,
                merged: false,
                refusal_reason: Some(EnqueueRefusalReason::LastItemDedup),
                queue_exit_events,
                persistence_error: None,
            };
        }
    }

    if let Some(last) = queue.last_mut() {
        if should_merge_intervention(last, &intervention) {
            if !last.text.is_empty() && !intervention.text.is_empty() {
                last.text.push('\n');
            }
            last.text.push_str(&intervention.text);
            last.message_id = intervention.message_id;
            push_unique_message_ids(
                &mut last.source_message_ids,
                intervention.source_message_ids.into_iter(),
            );
            last.created_at = intervention.created_at;
            // #2266: on merge, the incoming voice announcement (if any)
            // matches the new HEAD `message_id`; the dispatch path reinserts
            // by the HEAD id, so the latest metadata is what we keep.
            if intervention.voice_announcement.is_some() {
                last.voice_announcement = intervention.voice_announcement;
            }
            last.pending_uploads.extend(intervention.pending_uploads);
            return EnqueueInterventionResult {
                enqueued: true,
                merged: true,
                refusal_reason: None,
                queue_exit_events,
                persistence_error: None,
            };
        }
    }

    queue.push(intervention);
    if queue.len() > MAX_INTERVENTIONS_PER_CHANNEL {
        let overflow = queue.len() - MAX_INTERVENTIONS_PER_CHANNEL;
        queue_exit_events.extend(
            queue
                .drain(0..overflow)
                .map(|intervention| QueueExitEvent::new(intervention, QueueExitKind::Superseded)),
        );
    }
    EnqueueInterventionResult {
        enqueued: true,
        merged: false,
        refusal_reason: None,
        queue_exit_events,
        persistence_error: None,
    }
}

pub(crate) fn has_soft_intervention_at(queue: &mut Vec<Intervention>, now: Instant) -> bool {
    // #3177: no age-based eviction — only the overflow cap bounds the queue.
    let _ = now;
    if queue.len() > MAX_INTERVENTIONS_PER_CHANNEL {
        let overflow = queue.len() - MAX_INTERVENTIONS_PER_CHANNEL;
        queue.drain(0..overflow);
    }
    queue.iter().any(|item| item.mode == InterventionMode::Soft)
}

pub(crate) fn has_soft_intervention(queue: &mut Vec<Intervention>) -> HasPendingSoftQueueResult {
    let queue_exit_events = prune_interventions(queue);
    HasPendingSoftQueueResult {
        has_pending: queue.iter().any(|item| item.mode == InterventionMode::Soft),
        queue_exit_events,
        persistence_error: None,
    }
}

pub(crate) fn dequeue_next_soft_intervention(queue: &mut Vec<Intervention>) -> TakeNextSoftResult {
    let queue_exit_events = prune_interventions(queue);
    let intervention = queue
        .iter()
        .position(|item| item.mode == InterventionMode::Soft)
        .map(|index| queue.remove(index));
    let has_more = queue.iter().any(|item| item.mode == InterventionMode::Soft);
    TakeNextSoftResult {
        intervention,
        has_more,
        queue_len_after: queue.len(),
        queue_exit_events,
        persistence_error: None,
    }
}

pub(crate) fn cancel_soft_intervention_by_message_id(
    queue: &mut Vec<Intervention>,
    message_id: MessageId,
) -> CancelQueuedMessageResult {
    let mut queue_exit_events = prune_interventions(queue);
    let removed = queue
        .iter()
        .position(|item| {
            item.mode == InterventionMode::Soft
                && (item.message_id == message_id || item.source_message_ids.contains(&message_id))
        })
        .map(|index| queue.remove(index));
    if let Some(ref intervention) = removed {
        queue_exit_events.push(QueueExitEvent::new(
            intervention.clone(),
            QueueExitKind::Cancelled,
        ));
    }
    CancelQueuedMessageResult {
        removed,
        queue_exit_events,
        persistence_error: None,
    }
}

pub(crate) fn requeue_intervention_front(
    queue: &mut Vec<Intervention>,
    intervention: Intervention,
) -> Vec<QueueExitEvent> {
    let mut queue_exit_events = prune_interventions(queue);
    queue.insert(0, intervention);
    if queue.len() > MAX_INTERVENTIONS_PER_CHANNEL {
        queue_exit_events.extend(
            queue
                .drain(MAX_INTERVENTIONS_PER_CHANNEL..)
                .map(|intervention| QueueExitEvent::new(intervention, QueueExitKind::Superseded)),
        );
    }
    queue_exit_events
}

#[derive(Clone, Debug)]
pub(crate) struct QueuePersistenceContext {
    pub(crate) provider: ProviderKind,
    pub(crate) token_hash: String,
    pub(crate) dispatch_role_override: Option<u64>,
}

impl QueuePersistenceContext {
    pub(crate) fn new(
        provider: &ProviderKind,
        token_hash: &str,
        dispatch_role_override: Option<u64>,
    ) -> Self {
        Self {
            provider: provider.clone(),
            token_hash: token_hash.to_string(),
            dispatch_role_override,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct HydratePendingQueueResult {
    pub(crate) absorbed: usize,
    pub(crate) queue_len_after: usize,
    pub(crate) restored_override: Option<ChannelId>,
    pub(crate) persistence_error: Option<String>,
}

#[derive(Clone, Default)]
pub(crate) struct ChannelMailboxSnapshot {
    pub(crate) cancel_token: Option<Arc<CancelToken>>,
    pub(crate) active_request_owner: Option<UserId>,
    pub(crate) active_user_message_id: Option<MessageId>,
    /// #3167 — priority class of the active-turn slot. `UserOrAgent` (default)
    /// when idle or carrying a real user/agent turn; background variants cover
    /// monitor relay / self-paced TUI loop ownership. Lets the kickoff snapshot
    /// gate treat a background turn as non-blocking while preserving a distinct
    /// monitor marker for reclaim policy.
    pub(crate) active_turn_kind: ActiveTurnKind,
    pub(crate) intervention_queue: Vec<Intervention>,
    pub(crate) recovery_started_at: Option<Instant>,
    /// #1031: wall-clock instant the current active turn began (UTC). Set by
    /// the mailbox actor whenever `cancel_token` transitions from `None` to
    /// `Some`; cleared on finalize / clear. Idle detector uses this as a
    /// freshness anchor so the banner doesn't fire within the first poll of
    /// a brand-new turn.
    pub(crate) turn_started_at: Option<DateTime<Utc>>,
}

pub(crate) struct FinishTurnResult {
    pub(crate) removed_token: Option<Arc<CancelToken>>,
    pub(crate) has_pending: bool,
    pub(crate) mailbox_online: bool,
    pub(crate) queue_exit_events: Vec<QueueExitEvent>,
    // Carries a real `persist_queue_or_restore` failure on the finish-turn path;
    // part of the uniform queue-mutation result contract. No caller consumes it
    // yet, but it is written with genuine error info so it is kept rather than
    // silently dropped.
    #[allow(dead_code)]
    pub(crate) persistence_error: Option<String>,
}

pub(crate) struct ClearChannelResult {
    pub(crate) removed_token: Option<Arc<CancelToken>>,
    pub(crate) queue_exit_events: Vec<QueueExitEvent>,
    // Uniform queue-mutation persistence-result surface; written on the
    // clear-channel path, no consumer yet. See `FinishTurnResult`.
    #[allow(dead_code)]
    pub(crate) persistence_error: Option<String>,
}

pub(crate) struct CancelActiveTurnResult {
    pub(crate) token: Option<Arc<CancelToken>>,
    pub(crate) already_stopping: bool,
}

/// #3029(D): outcome of a `PurgeQueue` request.
#[derive(Debug, Default, Clone, Eq, PartialEq)]
pub(crate) struct PurgeQueueResult {
    /// Number of intervention-queue entries drained.
    pub(crate) drained: usize,
    /// Whether the request also released a *cancelled* active-turn anchor
    /// (only possible when `clear_cancelled_active_anchor` was requested and
    /// the anchored token was already cancelled).
    pub(crate) cleared_active_anchor: bool,
}

pub(crate) struct HasPendingSoftQueueResult {
    pub(crate) has_pending: bool,
    pub(crate) queue_exit_events: Vec<QueueExitEvent>,
    // Uniform queue-mutation persistence-result surface; no consumer yet.
    // See `FinishTurnResult`.
    #[allow(dead_code)]
    pub(crate) persistence_error: Option<String>,
}

pub(crate) struct RecoveryKickoffResult {
    pub(crate) activated_turn: bool,
    /// #3297 r3 — kickoff refused by a purge tombstone (`state.closed`).
    pub(crate) refused_closed: bool,
}

pub(crate) struct RestartDrainResult {
    pub(crate) queued_count: usize,
    pub(crate) persistence_error: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct QueuePersistenceFailure {
    pub(crate) channel_id: ChannelId,
    pub(crate) error: String,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct RestartDrainAllResult {
    pub(crate) queued_count: usize,
    pub(crate) persistence_errors: Vec<QueuePersistenceFailure>,
}

/// #2728: identifies which guard in `enqueue_intervention` produced an
/// `enqueued = false` outcome. Callers surface this through the producer-exit
/// diagnostic JSON so the next adk-cc-style incident is one log line away from
/// path A / B / C classification instead of code-only inference.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EnqueueRefusalReason {
    /// The incoming `message_id` is already present in some queued entry's
    /// `source_message_ids` — duplicate insert from a re-entry or rehydrated
    /// queue.
    SourceIdAlreadyQueued,
    /// The queue's last entry matches the incoming intervention on
    /// `(author_id, text, reply_context, has_reply_boundary)` within
    /// `INTERVENTION_DEDUP_WINDOW` — rapid-resend dedup.
    LastItemDedup,
    /// The `ChannelMailboxHandle` could not reach the mailbox actor (mpsc
    /// closed or oneshot dropped). Surfaced only at the handle layer.
    ActorUnreachable,
    /// #3297 r3 — the resolved actor is purge-tombstoned (`closed`). The
    /// registry's `enqueue_with_closed_retry` re-resolves a fresh actor.
    MailboxClosed,
}

impl EnqueueRefusalReason {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            EnqueueRefusalReason::SourceIdAlreadyQueued => "source_id_already_queued",
            EnqueueRefusalReason::LastItemDedup => "last_item_dedup",
            EnqueueRefusalReason::ActorUnreachable => "actor_unreachable",
            EnqueueRefusalReason::MailboxClosed => "mailbox_closed",
        }
    }
}

pub(crate) struct EnqueueInterventionResult {
    pub(crate) enqueued: bool,
    /// True when the incoming intervention was folded into the previous queue
    /// entry via `should_merge_intervention` (text concatenated, source IDs
    /// accumulated). Callers use this to surface a different reaction emoji
    /// for merged messages so users can tell merged from standalone entries.
    pub(crate) merged: bool,
    /// #2728: identifies which guard in `enqueue_intervention` (or the
    /// handle-layer actor fallback) produced the refusal. Persistence failures
    /// are reported in `persistence_error` instead so adding that path does not
    /// expand the externally matched refusal enum.
    pub(crate) refusal_reason: Option<EnqueueRefusalReason>,
    pub(crate) queue_exit_events: Vec<QueueExitEvent>,
    pub(crate) persistence_error: Option<String>,
}

pub(crate) struct CancelQueuedMessageResult {
    pub(crate) removed: Option<Intervention>,
    pub(crate) queue_exit_events: Vec<QueueExitEvent>,
    // Uniform queue-mutation persistence-result surface; no consumer yet.
    // See `FinishTurnResult`.
    #[allow(dead_code)]
    pub(crate) persistence_error: Option<String>,
}

pub(crate) struct TakeNextSoftResult {
    pub(crate) intervention: Option<Intervention>,
    pub(crate) has_more: bool,
    pub(crate) queue_len_after: usize,
    pub(crate) queue_exit_events: Vec<QueueExitEvent>,
    pub(crate) persistence_error: Option<String>,
}

pub(crate) struct RequeueInterventionResult {
    pub(crate) queue_exit_events: Vec<QueueExitEvent>,
    // Uniform queue-mutation persistence-result surface; no consumer yet.
    // See `FinishTurnResult`.
    #[allow(dead_code)]
    pub(crate) persistence_error: Option<String>,
}

static GLOBAL_CHANNEL_MAILBOXES: LazyLock<dashmap::DashMap<ChannelId, ChannelMailboxHandle>> =
    LazyLock::new(dashmap::DashMap::new);

#[derive(Clone)]
pub(crate) struct ChannelMailboxHandle {
    sender: mpsc::UnboundedSender<ChannelMailboxMsg>,
}

impl ChannelMailboxHandle {
    async fn request<T>(
        &self,
        build: impl FnOnce(oneshot::Sender<T>) -> ChannelMailboxMsg,
        fallback: T,
    ) -> T {
        let (reply_tx, reply_rx) = oneshot::channel();
        if self.sender.send(build(reply_tx)).is_err() {
            return fallback;
        }
        reply_rx.await.unwrap_or(fallback)
    }

    pub(crate) async fn snapshot(&self) -> ChannelMailboxSnapshot {
        self.request(
            |reply| ChannelMailboxMsg::Snapshot { reply },
            ChannelMailboxSnapshot::default(),
        )
        .await
    }

    pub(crate) async fn has_active_turn(&self) -> bool {
        self.request(|reply| ChannelMailboxMsg::HasActiveTurn { reply }, false)
            .await
    }

    pub(crate) async fn cancel_token(&self) -> Option<Arc<CancelToken>> {
        self.request(|reply| ChannelMailboxMsg::CancelToken { reply }, None)
            .await
    }

    /// #2374 — atomic "set cancel reason + flip cancelled" performed by
    /// the mailbox actor. PR #2373 (#2335) set `cancel_source` from the
    /// caller task before sending the actor a `CancelActiveTurn`; that
    /// kept the writes ordered for the common path but left a small
    /// reorder window where two concurrent cancellers could both fetch
    /// the same `cancel_token`, race to call `set_cancel_source`, then
    /// have the actor flip `cancelled` based on whichever message it
    /// dequeued first. Moving the reason write INTO the actor makes the
    /// reason-then-flip sequence genuinely sequential per channel and
    /// eliminates the small ordering window the previous design left.
    ///
    /// Semantics:
    ///  - If the active token is already cancelled (`already_stopping`),
    ///    the reason is NOT overwritten — earlier attribution wins, the
    ///    same protection PR #2373 added to the caller-side write.
    ///  - If no active token exists, this is a no-op (returns
    ///    `token: None`).
    pub(crate) async fn cancel_active_turn_with_reason(
        &self,
        reason: String,
    ) -> CancelActiveTurnResult {
        self.request(
            |reply| ChannelMailboxMsg::CancelActiveTurnWithReason { reason, reply },
            CancelActiveTurnResult {
                token: None,
                already_stopping: false,
            },
        )
        .await
    }

    // Unguarded `if_current` cancel; production uses the
    // `_with_reason` variant. Exercised only by `#[cfg(test)]` tests.
    #[allow(dead_code)]
    pub(crate) async fn cancel_active_turn_if_current(
        &self,
        expected_token: Arc<CancelToken>,
    ) -> CancelActiveTurnResult {
        self.request(
            |reply| ChannelMailboxMsg::CancelActiveTurnIfCurrent {
                expected_token,
                reply,
            },
            CancelActiveTurnResult {
                token: None,
                already_stopping: false,
            },
        )
        .await
    }

    /// #2374 — see [`Self::cancel_active_turn_with_reason`]. This variant
    /// preserves the `if_current` guard so a stale caller cannot cancel
    /// a freshly-restarted turn that happens to live on the same channel.
    pub(crate) async fn cancel_active_turn_if_current_with_reason(
        &self,
        expected_token: Arc<CancelToken>,
        reason: String,
    ) -> CancelActiveTurnResult {
        self.request(
            |reply| ChannelMailboxMsg::CancelActiveTurnIfCurrentWithReason {
                expected_token,
                reason,
                reply,
            },
            CancelActiveTurnResult {
                token: None,
                already_stopping: false,
            },
        )
        .await
    }

    /// #2374 Codex round-1 fix (HIGH-1) — actor-owned guarded cancel
    /// keyed by `user_message_id`. The handoff cancel-tombstone retry
    /// path must only cancel the target-channel turn that was actually
    /// started by the original handoff prompt; an unguarded cancel
    /// would also kill an unrelated turn that happened to start on the
    /// same target channel after the original handoff turn finalized.
    /// The actor performs the identity check inline so the read of
    /// `active_user_message_id` and the cancel flip are observed as a
    /// single per-channel transition.
    ///
    /// Returns `token: None` when the active turn's `user_message_id`
    /// does not match `expected_user_message_id` (or no active turn
    /// exists at all).
    pub(crate) async fn cancel_active_turn_if_user_message_with_reason(
        &self,
        expected_user_message_id: MessageId,
        reason: String,
    ) -> CancelActiveTurnResult {
        self.request(
            |reply| ChannelMailboxMsg::CancelActiveTurnIfUserMessageWithReason {
                expected_user_message_id,
                reason,
                reply,
            },
            CancelActiveTurnResult {
                token: None,
                already_stopping: false,
            },
        )
        .await
    }

    /// #3167 — atomically cancel the active turn IFF it is a *background* turn
    /// (monitor relay / self-paced TUI loop). Returns `true` ONLY when this call
    /// performs a NEW cancel (a background turn held the slot and was not already
    /// cancelling); returns `false` when the slot is idle, holds a real
    /// user/agent turn (left untouched), OR already holds an already-cancelling
    /// background turn (no-op). #3167 BLOCKER-1: the already-cancelling `false`
    /// is what stops the caller's immediate re-kick from hot-looping while the
    /// background finalizer drains the slot. Replaces the racy
    /// `active_turn_kind()`-read-then-`cancel_active_turn_with_reason()`
    /// sequence in the idle-queue dequeue gate: the actor observes the kind
    /// check and the cancel flip as one serialized step, so a real user turn
    /// that starts after the background turn finalizes is never aborted by a
    /// stale supersede.
    pub(crate) async fn cancel_active_background_turn_if_current(&self) -> bool {
        self.request(
            |reply| ChannelMailboxMsg::CancelActiveBackgroundTurnIfCurrent { reply },
            false,
        )
        .await
    }

    pub(crate) async fn try_start_turn(
        &self,
        cancel_token: Arc<CancelToken>,
        request_owner: UserId,
        user_message_id: MessageId,
    ) -> bool {
        // #3167 — default callers claim the slot as a real user/agent turn.
        self.try_start_turn_kinded(
            cancel_token,
            request_owner,
            user_message_id,
            ActiveTurnKind::UserOrAgent,
        )
        .await
    }

    /// #3167 — kinded variant of [`Self::try_start_turn`]. The monitor
    /// auto-turn and the self-paced TUI loop pass background kinds so a queued
    /// external USER intervention is not perpetually deferred behind the
    /// continuously-cycling background turn.
    pub(crate) async fn try_start_turn_kinded(
        &self,
        cancel_token: Arc<CancelToken>,
        request_owner: UserId,
        user_message_id: MessageId,
        turn_kind: ActiveTurnKind,
    ) -> bool {
        self.request(
            |reply| ChannelMailboxMsg::TryStartTurn {
                cancel_token,
                request_owner,
                user_message_id,
                turn_kind,
                reply,
            },
            false,
        )
        .await
    }

    // Default-kind restore wrapper; the production restore path lives in the
    // (currently dormant) `mailbox_restore_active_turn`. Exercised only by
    // `#[cfg(test)]` tests.
    #[allow(dead_code)]
    pub(crate) async fn restore_active_turn(
        &self,
        cancel_token: Arc<CancelToken>,
        request_owner: UserId,
        user_message_id: MessageId,
    ) {
        // #3167 — default restore re-binds a real user/agent turn.
        self.restore_active_turn_kinded(
            cancel_token,
            request_owner,
            user_message_id,
            ActiveTurnKind::UserOrAgent,
        )
        .await;
    }

    /// #3167 — kinded variant of [`Self::restore_active_turn`]. Preserves the
    /// background classification across a restore so the dequeue gates stay
    /// background-aware after a re-bind.
    // Reached only via the dormant restore wrapper / `#[cfg(test)]` tests.
    #[allow(dead_code)]
    pub(crate) async fn restore_active_turn_kinded(
        &self,
        cancel_token: Arc<CancelToken>,
        request_owner: UserId,
        user_message_id: MessageId,
        turn_kind: ActiveTurnKind,
    ) {
        let _ = self
            .request(
                |reply| ChannelMailboxMsg::RestoreActiveTurn {
                    cancel_token,
                    request_owner,
                    user_message_id,
                    turn_kind,
                    reply,
                },
                (),
            )
            .await;
    }

    /// #3167 — current active-turn kind, or `None` when the channel is idle
    /// (no `cancel_token`). Lets the dequeue path detect a background turn
    /// holding the slot so it can cancel-then-redispatch instead of starving
    /// a queued user intervention.
    // Production gates read `active_turn_kind` off the snapshot
    // (`snapshot.active_turn_kind`); this async accessor is exercised only by
    // `#[cfg(test)]` tests.
    #[allow(dead_code)]
    pub(crate) async fn active_turn_kind(&self) -> Option<ActiveTurnKind> {
        self.request(|reply| ChannelMailboxMsg::ActiveTurnKind { reply }, None)
            .await
    }

    /// #3167 — true only when a *real* (non-background) active turn holds the
    /// slot. Distinct from [`Self::has_active_turn`], which reports any active
    /// turn (background included) and whose semantics 30+ callers rely on.
    pub(crate) async fn has_blocking_active_turn(&self) -> bool {
        self.request(
            |reply| ChannelMailboxMsg::HasBlockingActiveTurn { reply },
            false,
        )
        .await
    }

    pub(crate) async fn recovery_kickoff(
        &self,
        cancel_token: Arc<CancelToken>,
        request_owner: UserId,
        // `None` for a recovery turn that carries no user message
        // (user_msg_id == 0, e.g. a TUI-direct turn) — there is then no
        // `active_user_message_id` to bind. `MessageId::new(0)` would panic.
        user_message_id: Option<MessageId>,
    ) -> RecoveryKickoffResult {
        self.request(
            |reply| ChannelMailboxMsg::RecoveryKickoff {
                cancel_token,
                request_owner,
                user_message_id,
                reply,
            },
            RecoveryKickoffResult {
                activated_turn: false,
                refused_closed: false,
            },
        )
        .await
    }

    pub(crate) async fn clear_recovery_marker(&self) {
        let _ = self
            .request(|reply| ChannelMailboxMsg::ClearRecoveryMarker { reply }, ())
            .await;
    }

    pub(crate) async fn enqueue(
        &self,
        intervention: Intervention,
        persistence: QueuePersistenceContext,
    ) -> EnqueueInterventionResult {
        self.request(
            |reply| ChannelMailboxMsg::Enqueue {
                intervention,
                persistence,
                reply,
            },
            EnqueueInterventionResult {
                enqueued: false,
                merged: false,
                refusal_reason: Some(EnqueueRefusalReason::ActorUnreachable),
                queue_exit_events: Vec::new(),
                persistence_error: None,
            },
        )
        .await
    }

    pub(crate) async fn has_pending_soft_queue(
        &self,
        persistence: QueuePersistenceContext,
    ) -> HasPendingSoftQueueResult {
        self.request(
            |reply| ChannelMailboxMsg::HasPendingSoftQueue { persistence, reply },
            HasPendingSoftQueueResult {
                has_pending: false,
                queue_exit_events: Vec::new(),
                persistence_error: None,
            },
        )
        .await
    }

    pub(crate) async fn take_next_soft(
        &self,
        persistence: QueuePersistenceContext,
    ) -> TakeNextSoftResult {
        self.request(
            |reply| ChannelMailboxMsg::TakeNextSoft { persistence, reply },
            TakeNextSoftResult {
                intervention: None,
                has_more: false,
                queue_len_after: 0,
                queue_exit_events: Vec::new(),
                persistence_error: None,
            },
        )
        .await
    }

    pub(crate) async fn requeue_front(
        &self,
        intervention: Intervention,
        persistence: QueuePersistenceContext,
    ) -> RequeueInterventionResult {
        self.request(
            |reply| ChannelMailboxMsg::RequeueFront {
                intervention,
                persistence,
                reply,
            },
            RequeueInterventionResult {
                queue_exit_events: Vec::new(),
                persistence_error: None,
            },
        )
        .await
    }

    pub(crate) async fn cancel_queued_message(
        &self,
        message_id: MessageId,
        persistence: QueuePersistenceContext,
    ) -> CancelQueuedMessageResult {
        self.request(
            |reply| ChannelMailboxMsg::CancelQueuedMessage {
                message_id,
                persistence,
                reply,
            },
            CancelQueuedMessageResult {
                removed: None,
                queue_exit_events: Vec::new(),
                persistence_error: None,
            },
        )
        .await
    }

    pub(crate) async fn finish_turn(
        &self,
        persistence: QueuePersistenceContext,
    ) -> FinishTurnResult {
        self.request(
            |reply| ChannelMailboxMsg::FinishTurn { persistence, reply },
            FinishTurnResult {
                removed_token: None,
                has_pending: false,
                mailbox_online: false,
                queue_exit_events: Vec::new(),
                persistence_error: None,
            },
        )
        .await
    }

    /// #3016 — identity-guarded finish. Finalizes the active turn ONLY when
    /// the mailbox's current `active_user_message_id` matches
    /// `expected_user_message_id`; otherwise it is a no-op that returns
    /// `removed_token = None` (so the caller's counter decrement is skipped)
    /// and leaves the possibly-newer live turn untouched.
    pub(crate) async fn finish_turn_if_matches(
        &self,
        expected_user_message_id: MessageId,
        persistence: QueuePersistenceContext,
    ) -> FinishTurnResult {
        self.request(
            |reply| ChannelMailboxMsg::FinishTurnIfMatches {
                expected_user_message_id,
                persistence,
                reply,
            },
            FinishTurnResult {
                removed_token: None,
                has_pending: false,
                mailbox_online: false,
                queue_exit_events: Vec::new(),
                persistence_error: None,
            },
        )
        .await
    }

    pub(crate) async fn hard_stop(&self) -> FinishTurnResult {
        self.request(
            |reply| ChannelMailboxMsg::HardStop { reply },
            FinishTurnResult {
                removed_token: None,
                has_pending: false,
                mailbox_online: false,
                queue_exit_events: Vec::new(),
                persistence_error: None,
            },
        )
        .await
    }

    pub(crate) async fn finish_cancelled_turn(&self) -> FinishTurnResult {
        self.request(
            |reply| ChannelMailboxMsg::FinishCancelledTurn { reply },
            FinishTurnResult {
                removed_token: None,
                has_pending: false,
                mailbox_online: false,
                queue_exit_events: Vec::new(),
                persistence_error: None,
            },
        )
        .await
    }

    pub(crate) async fn clear(&self, persistence: QueuePersistenceContext) -> ClearChannelResult {
        self.request(
            |reply| ChannelMailboxMsg::Clear { persistence, reply },
            ClearChannelResult {
                removed_token: None,
                queue_exit_events: Vec::new(),
                persistence_error: None,
            },
        )
        .await
    }

    /// #2706: queue-only purge. Drains the intervention queue without
    /// touching the active `cancel_token`, so a turn that entered the
    /// mailbox between a sibling force-kill and this call is not
    /// collaterally cancelled.
    ///
    /// #3029(D): `clear_cancelled_active_anchor=true` additionally releases the
    /// active-turn anchor when its token is already `cancelled` (force purge),
    /// so a force cancel does not leave a stale anchor that blocks the next
    /// dispatch. Pass `false` for a pure queue drain.
    pub(crate) async fn purge_queue(
        &self,
        persistence: QueuePersistenceContext,
        clear_cancelled_active_anchor: bool,
    ) -> PurgeQueueResult {
        self.request(
            |reply| ChannelMailboxMsg::PurgeQueue {
                persistence,
                clear_cancelled_active_anchor,
                reply,
            },
            PurgeQueueResult::default(),
        )
        .await
    }

    // #3864: test-only queue-seeding helper. Production startup restore moved
    // to `merge_restored_queue_items` (the in-actor merge), so `ReplaceQueue`'s
    // blind overwrite — the source of the lost-enqueue race — has NO production
    // caller anymore and is gated to test builds. Test modules still use it to
    // seed a channel queue directly.
    #[cfg(test)]
    pub(crate) async fn replace_queue(
        &self,
        queue: Vec<Intervention>,
        persistence: QueuePersistenceContext,
    ) {
        let _ = self
            .request(
                |reply| ChannelMailboxMsg::ReplaceQueue {
                    queue,
                    persistence,
                    reply,
                },
                (),
            )
            .await;
    }

    pub(crate) async fn hydrate_pending_queue_from_disk(
        &self,
        persistence: QueuePersistenceContext,
    ) -> HydratePendingQueueResult {
        self.request(
            |reply| ChannelMailboxMsg::HydratePendingQueueFromDisk { persistence, reply },
            HydratePendingQueueResult::default(),
        )
        .await
    }

    /// #3864: in-actor merge of SIGTERM-restored disk items into the live
    /// queue. Mirrors `hydrate_pending_queue_from_disk`, but the caller
    /// supplies the items it already loaded and sender-filtered (the
    /// sender check is stateless, so it stays out-of-actor); the actor then
    /// dedups, front-inserts and persists in one serialized step. Replaces
    /// the out-of-actor snapshot→build→`replace_queue` RMW that silently
    /// dropped any live `Enqueue` landing between its two round-trips.
    pub(crate) async fn merge_restored_queue_items(
        &self,
        items: Vec<Intervention>,
        persistence: QueuePersistenceContext,
    ) -> HydratePendingQueueResult {
        self.request(
            |reply| ChannelMailboxMsg::MergeRestoredQueueItems {
                items,
                persistence,
                reply,
            },
            HydratePendingQueueResult::default(),
        )
        .await
    }

    pub(crate) async fn restart_drain(
        &self,
        persistence: QueuePersistenceContext,
    ) -> RestartDrainResult {
        self.request(
            |reply| ChannelMailboxMsg::RestartDrain { persistence, reply },
            RestartDrainResult {
                queued_count: 0,
                persistence_error: None,
            },
        )
        .await
    }

    pub(crate) async fn extend_timeout(
        &self,
        extend_by_secs: u64,
    ) -> Result<WatchdogDeadlineExtension, WatchdogDeadlineExtensionError> {
        self.request(
            |reply| ChannelMailboxMsg::ExtendTimeout {
                extend_by_secs,
                reply,
            },
            Err(WatchdogDeadlineExtensionError::MailboxUnavailable),
        )
        .await
    }

    pub(crate) async fn take_timeout_override(&self) -> Option<WatchdogDeadlineExtension> {
        self.request(
            |reply| ChannelMailboxMsg::TakeTimeoutOverride { reply },
            None,
        )
        .await
    }

    pub(crate) async fn clear_timeout_override(&self) {
        let _ = self
            .request(
                |reply| ChannelMailboxMsg::ClearTimeoutOverride { reply },
                (),
            )
            .await;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct WatchdogDeadlineExtension {
    pub(crate) requested_deadline_ms: i64,
    pub(crate) new_deadline_ms: i64,
    pub(crate) max_deadline_ms: i64,
    pub(crate) applied_extend_secs: u64,
    pub(crate) requested_extend_secs: u64,
    pub(crate) extension_count: u32,
    pub(crate) extension_count_limit: u32,
    pub(crate) extension_total_secs: u64,
    pub(crate) extension_total_secs_limit: u64,
    pub(crate) clamped: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WatchdogDeadlineExtensionError {
    MailboxUnavailable,
    NoActiveTurn,
}

/// #2443 — deterministic "recovery finished" signal per channel.
///
/// Pairs a `tokio::sync::Notify` with a one-shot `latched` flag so a
/// `recovery_done` event raised before a watcher subscribes is still
/// observable. Without the latch, `Notify::notify_waiters` would lose the
/// signal whenever recovery completes BEFORE the watcher reaches its
/// `notified()` await, re-introducing exactly the race the 60s timeout was
/// papering over. The latch flips on the first `mark_done` call and
/// `wait()` short-circuits on subsequent observers — recovery sessions are
/// monotonic per channel within the lifetime of this signal.
///
/// Callers reset the latch when a *new* recovery begins (so the next watcher
/// wave doesn't see a stale "already done"). `reset()` is idempotent.
pub(crate) struct RecoveryDoneSignal {
    notify: Notify,
    latched: std::sync::atomic::AtomicBool,
}

impl RecoveryDoneSignal {
    fn new() -> Self {
        Self {
            notify: Notify::new(),
            latched: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Mark recovery as finished. Wakes all current waiters and latches the
    /// signal so subsequent `wait()` calls return immediately until `reset()`.
    pub(crate) fn mark_done(&self) {
        self.latched.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    /// Reset the latch so the next recovery cycle starts clean. Should be
    /// called at recovery kickoff so an old "done" flag does not satisfy a
    /// watcher waiting for the new run.
    pub(crate) fn reset(&self) {
        self.latched.store(false, Ordering::Release);
    }

    /// Wait until `mark_done` is observed. Returns immediately if the latch
    /// is already set (race-free for observers that subscribe after the
    /// notification fires).
    pub(crate) async fn wait(&self) {
        if self.latched.load(Ordering::Acquire) {
            return;
        }
        // Subscribe BEFORE the second check to close the
        // observe-then-subscribe window. `Notify::notified()` returns a
        // future that registers a waiter on first poll; recheck the flag
        // afterwards in case `mark_done` ran between the load and the
        // subscribe.
        let notified = self.notify.notified();
        tokio::pin!(notified);
        if self.latched.load(Ordering::Acquire) {
            return;
        }
        notified.await;
    }
}

/// #2424 — generic "active turn finished" signal per channel.
///
/// Same latch shape as `RecoveryDoneSignal`: a terminal mailbox transition
/// can happen before a deferred monitor auto-turn subscribes, so late
/// subscribers must observe the already-finished state without falling back
/// to mailbox-state polling.
pub(crate) struct TurnFinishedSignal {
    notify: Notify,
    latched: std::sync::atomic::AtomicBool,
}

impl TurnFinishedSignal {
    fn new() -> Self {
        Self {
            notify: Notify::new(),
            latched: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub(crate) fn mark_done(&self) {
        self.latched.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    pub(crate) fn reset(&self) {
        self.latched.store(false, Ordering::Release);
    }

    pub(crate) async fn wait(&self) {
        if self.latched.load(Ordering::Acquire) {
            return;
        }
        let notified = self.notify.notified();
        tokio::pin!(notified);
        if self.latched.load(Ordering::Acquire) {
            return;
        }
        notified.await;
    }
}

static GLOBAL_RECOVERY_DONE_SIGNALS: LazyLock<
    dashmap::DashMap<ChannelId, Arc<RecoveryDoneSignal>>,
> = LazyLock::new(dashmap::DashMap::new);
static GLOBAL_TURN_FINISHED_SIGNALS: LazyLock<
    dashmap::DashMap<ChannelId, Arc<TurnFinishedSignal>>,
> = LazyLock::new(dashmap::DashMap::new);

fn turn_finished_signal(channel_id: ChannelId) -> Arc<TurnFinishedSignal> {
    if let Some(existing) = GLOBAL_TURN_FINISHED_SIGNALS.get(&channel_id) {
        return existing.value().clone();
    }
    let signal = Arc::new(TurnFinishedSignal::new());
    match GLOBAL_TURN_FINISHED_SIGNALS.entry(channel_id) {
        dashmap::mapref::entry::Entry::Occupied(entry) => entry.get().clone(),
        dashmap::mapref::entry::Entry::Vacant(entry) => {
            entry.insert(signal.clone());
            signal
        }
    }
}

fn reset_turn_finished_signal(channel_id: ChannelId) {
    turn_finished_signal(channel_id).reset();
}

fn mark_turn_finished_signal_done(channel_id: ChannelId) {
    turn_finished_signal(channel_id).mark_done();
}

#[derive(Clone, Default)]
pub(crate) struct ChannelMailboxRegistry {
    handles: Arc<dashmap::DashMap<ChannelId, ChannelMailboxHandle>>,
    /// #2443 — per-channel "recovery finished" signals consumed by
    /// `watchers/lifecycle.rs` to graduate the 60s `recovery_started_at < 60s`
    /// skip heuristic. Stored in a separate map (rather than fields on the
    /// mailbox actor state) so both the recovery_engine producer and the
    /// watchers/lifecycle consumer can take a clone without round-tripping
    /// through the actor's message channel.
    recovery_done: Arc<dashmap::DashMap<ChannelId, Arc<RecoveryDoneSignal>>>,
    /// #2424 — per-channel generic "turn finished" signals consumed by
    /// deferred monitor auto-turn. Stored beside `recovery_done` so callers
    /// can clone the signal without actor round-trips.
    turn_finished: Arc<dashmap::DashMap<ChannelId, Arc<TurnFinishedSignal>>>,
}

impl ChannelMailboxRegistry {
    pub(crate) fn handle(&self, channel_id: ChannelId) -> ChannelMailboxHandle {
        if let Some(existing) = self.handles.get(&channel_id) {
            return existing.clone();
        }

        let handle = spawn_channel_mailbox(channel_id);
        let resolved = match self.handles.entry(channel_id) {
            dashmap::mapref::entry::Entry::Occupied(entry) => entry.get().clone(),
            dashmap::mapref::entry::Entry::Vacant(entry) => {
                entry.insert(handle.clone());
                handle
            }
        };
        GLOBAL_CHANNEL_MAILBOXES.insert(channel_id, resolved.clone());
        resolved
    }

    pub(crate) fn global_handle(channel_id: ChannelId) -> Option<ChannelMailboxHandle> {
        GLOBAL_CHANNEL_MAILBOXES
            .get(&channel_id)
            .map(|entry| entry.value().clone())
    }

    /// #2443 — fetch or create the recovery-done signal for this channel.
    /// Cloning the `Arc` is cheap; the signal lives for the lifetime of the
    /// registry. The same `Arc` is mirrored into `GLOBAL_RECOVERY_DONE_SIGNALS`
    /// so callers that only have a `ChannelId` (no registry handle, e.g.
    /// helper free functions outside `SharedData`) can resolve via
    /// `global_recovery_done`.
    pub(crate) fn recovery_done(&self, channel_id: ChannelId) -> Arc<RecoveryDoneSignal> {
        if let Some(existing) = self.recovery_done.get(&channel_id) {
            return existing.clone();
        }
        let signal = Arc::new(RecoveryDoneSignal::new());
        let resolved = match self.recovery_done.entry(channel_id) {
            dashmap::mapref::entry::Entry::Occupied(entry) => entry.get().clone(),
            dashmap::mapref::entry::Entry::Vacant(entry) => {
                entry.insert(signal.clone());
                signal
            }
        };
        GLOBAL_RECOVERY_DONE_SIGNALS.insert(channel_id, resolved.clone());
        resolved
    }

    /// #2443 — globally resolvable variant. Returns `None` only when no
    /// `recovery_done()` call has happened yet for this channel; callers
    /// that need a signal regardless should use the per-instance accessor.
    pub(crate) fn global_recovery_done(channel_id: ChannelId) -> Option<Arc<RecoveryDoneSignal>> {
        GLOBAL_RECOVERY_DONE_SIGNALS
            .get(&channel_id)
            .map(|entry| entry.value().clone())
    }

    pub(crate) fn turn_finished(&self, channel_id: ChannelId) -> Arc<TurnFinishedSignal> {
        if let Some(existing) = self.turn_finished.get(&channel_id) {
            return existing.clone();
        }
        let signal = turn_finished_signal(channel_id);
        let resolved = match self.turn_finished.entry(channel_id) {
            dashmap::mapref::entry::Entry::Occupied(entry) => entry.get().clone(),
            dashmap::mapref::entry::Entry::Vacant(entry) => {
                entry.insert(signal.clone());
                signal
            }
        };
        GLOBAL_TURN_FINISHED_SIGNALS.insert(channel_id, resolved.clone());
        resolved
    }

    // Global-registry accessor for the latched turn-finished signal; exercised
    // only by `#[cfg(test)]` late-subscriber tests.
    #[allow(dead_code)]
    pub(crate) fn global_turn_finished(channel_id: ChannelId) -> Option<Arc<TurnFinishedSignal>> {
        GLOBAL_TURN_FINISHED_SIGNALS
            .get(&channel_id)
            .map(|entry| entry.value().clone())
    }

    pub(crate) async fn snapshot_all(&self) -> HashMap<ChannelId, ChannelMailboxSnapshot> {
        let handles: Vec<_> = self
            .handles
            .iter()
            .map(|entry| (*entry.key(), entry.value().clone()))
            .collect();
        let mut snapshots = HashMap::new();
        for (channel_id, handle) in handles {
            snapshots.insert(channel_id, handle.snapshot().await);
        }
        snapshots
    }

    pub(crate) async fn restart_drain_all(
        &self,
        provider: &ProviderKind,
        token_hash: &str,
        dispatch_role_overrides: &dashmap::DashMap<ChannelId, ChannelId>,
    ) -> RestartDrainAllResult {
        let handles: Vec<_> = self
            .handles
            .iter()
            .map(|entry| (*entry.key(), entry.value().clone()))
            .collect();
        let mut queued_total = 0usize;
        let mut persistence_errors = Vec::new();
        for (channel_id, handle) in handles {
            let persistence = QueuePersistenceContext::new(
                provider,
                token_hash,
                dispatch_role_overrides
                    .get(&channel_id)
                    .map(|override_id| override_id.value().get()),
            );
            let result = handle.restart_drain(persistence).await;
            queued_total += result.queued_count;
            if let Some(error) = result.persistence_error {
                persistence_errors.push(QueuePersistenceFailure { channel_id, error });
            }
        }
        RestartDrainAllResult {
            queued_count: queued_total,
            persistence_errors,
        }
    }
}

// #3297 r3 (codex) — tombstone classification, enforced for EVERY arm by
// `registry_purge::gate_closed_arm` ahead of the actor's match. Once
// `CloseIfIdle` sets `state.closed` (actor about to be unlinked):
//  (a) START-LIKE arms — anything that binds an active turn / recovery marker
//      or accepts NEW work (`TryStartTurn`, `RestoreActiveTurn`,
//      `RecoveryKickoff`, `Enqueue`) — are REFUSED with that arm's existing
//      "cannot start" reply (`TryStartTurn` ⇒ `false`); callers re-resolve a
//      fresh actor via the registry `*_with_closed_retry` helpers and replay.
//  (b) everything else stays ALLOWED — reads, cancels, finishes, drains, and
//      queue RESTITUTION (`RequeueFront`/`ReplaceQueue`/hydrate, which
//      re-persist already-accepted work to disk for a successor actor to
//      hydrate — refusing those would drop user messages).
// New arms must be classified here and (if start-like) gated there.
enum ChannelMailboxMsg {
    Snapshot {
        reply: oneshot::Sender<ChannelMailboxSnapshot>,
    },
    HasActiveTurn {
        reply: oneshot::Sender<bool>,
    },
    /// #3167 — true only when a non-background active turn holds the slot.
    HasBlockingActiveTurn {
        reply: oneshot::Sender<bool>,
    },
    /// #3167 — current active-turn kind, or `None` when the channel is idle.
    // Constructed only by the test-only `active_turn_kind` accessor.
    #[allow(dead_code)]
    ActiveTurnKind {
        reply: oneshot::Sender<Option<ActiveTurnKind>>,
    },
    CancelToken {
        reply: oneshot::Sender<Option<Arc<CancelToken>>>,
    },
    /// #2374 — atomic reason-write + cancel flip performed by the actor.
    CancelActiveTurnWithReason {
        reason: String,
        reply: oneshot::Sender<CancelActiveTurnResult>,
    },
    // Constructed only by the test-only `cancel_active_turn_if_current`.
    #[allow(dead_code)]
    CancelActiveTurnIfCurrent {
        expected_token: Arc<CancelToken>,
        reply: oneshot::Sender<CancelActiveTurnResult>,
    },
    /// #2374 — see `CancelActiveTurnWithReason`. Variant that also matches
    /// `expected_token` so a stale caller cannot cancel a restarted turn.
    CancelActiveTurnIfCurrentWithReason {
        expected_token: Arc<CancelToken>,
        reason: String,
        reply: oneshot::Sender<CancelActiveTurnResult>,
    },
    /// #2374 Codex round-1 fix (HIGH-1) — identity-guarded cancel by
    /// active `user_message_id`. See
    /// `ChannelMailboxHandle::cancel_active_turn_if_user_message_with_reason`.
    CancelActiveTurnIfUserMessageWithReason {
        expected_user_message_id: MessageId,
        reason: String,
        reply: oneshot::Sender<CancelActiveTurnResult>,
    },
    /// #3167 — atomic, kind-guarded cancel of a *background* active turn. The
    /// idle-queue dequeue gate uses this to supersede a background relay/loop
    /// turn without the TOCTOU window of a separate `active_turn_kind()` read
    /// followed by an unguarded cancel: between that read and the cancel the
    /// background turn could finalize and a real user turn start, and the
    /// unguarded cancel would then abort the real turn. The actor performs the
    /// `is_background` check and the cancel flip as a single serialized step.
    /// Replies `true` when a background turn was cancelled, `false` otherwise
    /// (idle slot, or a real user/agent turn holds the slot — left untouched).
    CancelActiveBackgroundTurnIfCurrent {
        reply: oneshot::Sender<bool>,
    },
    TryStartTurn {
        cancel_token: Arc<CancelToken>,
        request_owner: UserId,
        user_message_id: MessageId,
        /// #3167 — priority class to record on the success branch.
        turn_kind: ActiveTurnKind,
        reply: oneshot::Sender<bool>,
    },
    // Constructed only via the dormant restore wrapper / `#[cfg(test)]` tests.
    #[allow(dead_code)]
    RestoreActiveTurn {
        cancel_token: Arc<CancelToken>,
        request_owner: UserId,
        user_message_id: MessageId,
        /// #3167 — priority class to record on the restored slot.
        turn_kind: ActiveTurnKind,
        reply: oneshot::Sender<()>,
    },
    RecoveryKickoff {
        cancel_token: Arc<CancelToken>,
        request_owner: UserId,
        user_message_id: Option<MessageId>,
        reply: oneshot::Sender<RecoveryKickoffResult>,
    },
    ClearRecoveryMarker {
        reply: oneshot::Sender<()>,
    },
    Enqueue {
        intervention: Intervention,
        persistence: QueuePersistenceContext,
        reply: oneshot::Sender<EnqueueInterventionResult>,
    },
    HasPendingSoftQueue {
        persistence: QueuePersistenceContext,
        reply: oneshot::Sender<HasPendingSoftQueueResult>,
    },
    TakeNextSoft {
        persistence: QueuePersistenceContext,
        reply: oneshot::Sender<TakeNextSoftResult>,
    },
    RequeueFront {
        intervention: Intervention,
        persistence: QueuePersistenceContext,
        reply: oneshot::Sender<RequeueInterventionResult>,
    },
    CancelQueuedMessage {
        message_id: MessageId,
        persistence: QueuePersistenceContext,
        reply: oneshot::Sender<CancelQueuedMessageResult>,
    },
    FinishTurn {
        persistence: QueuePersistenceContext,
        reply: oneshot::Sender<FinishTurnResult>,
    },
    /// #3016 — identity-guarded finish. Only finalizes the active turn IF the
    /// mailbox's CURRENT `active_user_message_id` matches
    /// `expected_user_message_id`. Closes the wrong-turn race: a stale /
    /// channel-only terminal arriving after a turn finalized but before the
    /// next turn's `try_start_turn` (or after ledger GC) must NOT release the
    /// NEWER turn's token or decrement `global_active`. On mismatch this is a
    /// no-op that returns `removed_token = None`, leaving the live turn intact.
    FinishTurnIfMatches {
        expected_user_message_id: MessageId,
        persistence: QueuePersistenceContext,
        reply: oneshot::Sender<FinishTurnResult>,
    },
    HardStop {
        reply: oneshot::Sender<FinishTurnResult>,
    },
    FinishCancelledTurn {
        reply: oneshot::Sender<FinishTurnResult>,
    },
    Clear {
        persistence: QueuePersistenceContext,
        reply: oneshot::Sender<ClearChannelResult>,
    },
    /// #2706: drain the intervention queue without touching the active
    /// `cancel_token`. Used by `cancel_turn(force=true)` so the in-memory
    /// channel mailbox is emptied even if a fresh turn entered the actor
    /// between `force_kill_turn_without_cancel_event` and this purge.
    ///
    /// #3029(D): when `clear_cancelled_active_anchor` is set (force purge),
    /// also release the active-turn anchor (`cancel_token` /
    /// `active_request_owner` / `active_user_message_id` / `turn_started_at`)
    /// — but ONLY if that anchor's token is already `cancelled`. The force
    /// path cancels the token via `cancel_active_token` before purging, so the
    /// just-killed turn's anchor is cleared, while a fresh *uncancelled* turn
    /// that entered the actor between force-kill and purge keeps its anchor
    /// (preserving the #2706 no-collateral-cancel guarantee).
    PurgeQueue {
        persistence: QueuePersistenceContext,
        clear_cancelled_active_anchor: bool,
        reply: oneshot::Sender<PurgeQueueResult>,
    },
    /// #3864: blind queue overwrite. Production restore now uses
    /// `MergeRestoredQueueItems` (in-actor, race-immune); `ReplaceQueue` has no
    /// production caller and survives ONLY as a `#[cfg(test)]` queue-seeding
    /// primitive used across the queue / turn_finalizer / turn_orchestrator
    /// test modules.
    #[cfg(test)]
    ReplaceQueue {
        queue: Vec<Intervention>,
        persistence: QueuePersistenceContext,
        reply: oneshot::Sender<()>,
    },
    HydratePendingQueueFromDisk {
        persistence: QueuePersistenceContext,
        reply: oneshot::Sender<HydratePendingQueueResult>,
    },
    /// #3864: merge SIGTERM-restored disk queue items into the LIVE queue
    /// inside the actor, in one serialized step. Unlike `ReplaceQueue` — a
    /// blind overwrite that loses any `Enqueue` landing between an
    /// out-of-actor snapshot and the replace — this reads, dedups,
    /// front-inserts and persists atomically, so a live reconcile-window
    /// enqueue can never be dropped (same race-immunity as
    /// `HydratePendingQueueFromDisk`, #1683).
    MergeRestoredQueueItems {
        items: Vec<Intervention>,
        persistence: QueuePersistenceContext,
        reply: oneshot::Sender<HydratePendingQueueResult>,
    },
    RestartDrain {
        persistence: QueuePersistenceContext,
        reply: oneshot::Sender<RestartDrainResult>,
    },
    ExtendTimeout {
        extend_by_secs: u64,
        reply: oneshot::Sender<Result<WatchdogDeadlineExtension, WatchdogDeadlineExtensionError>>,
    },
    TakeTimeoutOverride {
        reply: oneshot::Sender<Option<WatchdogDeadlineExtension>>,
    },
    ClearTimeoutOverride {
        reply: oneshot::Sender<()>,
    },
    /// #3297 r2 (codex) — registry purge: verify idleness and set the `closed`
    /// tombstone in ONE serialized actor step, closing the snapshot→unlink
    /// TOCTOU race. Full rationale + verdict logic live in `registry_purge.rs`.
    CloseIfIdle {
        reply: oneshot::Sender<Result<(), &'static str>>,
    },
}

/// #3167 — priority class of the mailbox active-turn slot. Lets the external-input
/// dequeue distinguish a low-priority background relay (monitor terminal-output
/// relay, self-paced TUI loop) from a real user/agent turn, so a queued external
/// USER intervention is not perpetually deferred behind a continuously-cycling
/// background turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum ActiveTurnKind {
    #[default]
    UserOrAgent,
    Background,
    MonitorAutoTurn,
}
impl ActiveTurnKind {
    pub(crate) fn is_background(self) -> bool {
        matches!(
            self,
            ActiveTurnKind::Background | ActiveTurnKind::MonitorAutoTurn
        )
    }

    pub(crate) fn is_monitor_auto_turn(self) -> bool {
        matches!(self, ActiveTurnKind::MonitorAutoTurn)
    }
}

/// #3167 BLOCKER-2 safety valve — max consecutive `Background` starts refused
/// SOLELY because a dequeued-but-not-yet-claimed user dispatch holds the
/// `pending_user_dispatch` reservation (the queue is already empty). After this
/// many refusals with no intervening user claim/requeue, the reservation is
/// force-cleared so a lost/never-claimed dequeue cannot permanently lock out
/// `Background` turns. Bounded + reset on every (re)set/claim/requeue ⇒
/// provably non-permanent.
const PENDING_USER_DISPATCH_MAX_YIELDS: u32 = 5;

#[derive(Default)]
struct ChannelMailboxState {
    cancel_token: Option<Arc<CancelToken>>,
    active_request_owner: Option<UserId>,
    active_user_message_id: Option<MessageId>,
    /// #3167 — priority class of the active-turn slot. `UserOrAgent` (default)
    /// for a real user/agent turn; background variants cover monitor
    /// terminal-output relay or self-paced TUI loop turns. Reset to default
    /// wherever the active-turn anchor is cleared.
    active_turn_kind: ActiveTurnKind,
    intervention_queue: Vec<Intervention>,
    /// #3167 BLOCKER-2 — reservation that closes the dequeue→claim starvation
    /// window. `TakeNextSoft` REMOVES the queued head before the dequeued
    /// UserOrAgent turn actually claims the slot (the claim happens later, in
    /// `intake_turn`, after async kickoff cleanup). During that window the
    /// `intervention_queue` is empty, so a `Background` `TryStartTurn` would
    /// otherwise acquire the freed slot AHEAD of the in-flight user turn. While
    /// `Some`, a `Background` start yields exactly as it does for a non-empty
    /// queue. Set when `TakeNextSoft` hands out a head for dispatch; cleared
    /// when a `UserOrAgent` turn claims the slot, when the reserved id is
    /// re-enqueued/requeued (dispatch failed → queue-non-empty then covers it),
    /// or by the bounded safety valve below.
    pending_user_dispatch: Option<MessageId>,
    /// #3167 BLOCKER-2 SAFETY VALVE — consecutive `Background` starts refused
    /// SOLELY because of `pending_user_dispatch` (the queue is already empty).
    /// If a dequeued user turn is lost and never claims nor requeues, the
    /// reservation would otherwise lock `Background` out forever. After
    /// `PENDING_USER_DISPATCH_MAX_YIELDS` such refusals with no intervening
    /// user claim/requeue, the reservation is force-cleared. Reset to 0 whenever
    /// the reservation is (re)set or a user claim/requeue clears it ⇒ the valve
    /// is bounded and provably non-permanent.
    pending_user_dispatch_yield_count: u32,
    last_persistence: Option<QueuePersistenceContext>,
    recovery_started_at: Option<Instant>,
    /// #3297 r2 — purge tombstone set by `CloseIfIdle`; see `registry_purge.rs`.
    closed: bool,
    /// #1031: see `ChannelMailboxSnapshot::turn_started_at`. Mirrors the
    /// `cancel_token.is_some()` lifetime so the idle-detector freshness
    /// anchor is always source-of-truth from the mailbox actor itself.
    turn_started_at: Option<DateTime<Utc>>,
    watchdog_deadline_override: Option<WatchdogDeadlineExtension>,
    watchdog_extension_count: u32,
    watchdog_extension_total_secs: u64,
}

fn persist_queue(
    channel_id: ChannelId,
    queue: &[Intervention],
    persistence: &QueuePersistenceContext,
) -> Result<(), String> {
    save_channel_queue(
        &persistence.provider,
        &persistence.token_hash,
        channel_id,
        queue,
        persistence.dispatch_role_override,
    )
}

fn log_queue_persistence_rollback(
    operation: &str,
    channel_id: ChannelId,
    persistence: &QueuePersistenceContext,
    error: &str,
) {
    tracing::error!(
        operation,
        provider = persistence.provider.as_str(),
        token_hash = %persistence.token_hash,
        channel_id = channel_id.get(),
        error = %error,
        "rolled back in-memory pending queue mutation after durable persistence failed"
    );
}

fn persist_queue_or_restore(
    state: &mut ChannelMailboxState,
    channel_id: ChannelId,
    persistence: &QueuePersistenceContext,
    previous_queue: Vec<Intervention>,
    operation: &str,
) -> Result<(), String> {
    match persist_queue(channel_id, &state.intervention_queue, persistence) {
        Ok(()) => Ok(()),
        Err(error) => {
            state.intervention_queue = previous_queue;
            log_queue_persistence_rollback(operation, channel_id, persistence, &error);
            Err(error)
        }
    }
}

fn delete_pending_dispatch_marker_for_state(
    state: &ChannelMailboxState,
    channel_id: ChannelId,
    operation: &str,
) {
    let Some(persistence) = state.last_persistence.as_ref() else {
        tracing::warn!(
            operation,
            channel_id = channel_id.get(),
            "pending dispatch marker could not be cleared because mailbox has no persistence context"
        );
        return;
    };
    if let Err(error) = remove_channel_pending_dispatch_marker(
        &persistence.provider,
        &persistence.token_hash,
        channel_id,
    ) {
        tracing::warn!(
            operation,
            provider = persistence.provider.as_str(),
            token_hash = %persistence.token_hash,
            channel_id = channel_id.get(),
            error = %error,
            "failed to remove pending dispatch marker"
        );
    }
}

/// #3864: the full set of message ids an intervention represents — every
/// `source_message_ids` entry (a merged queue item may carry several), plus its
/// own `message_id` when not already among them. Dedup must treat an incoming
/// item as a duplicate ONLY when EVERY one of these ids is already queued;
/// otherwise a merged item whose text is no longer separable would silently
/// drop the source messages startup catch-up has not yet recovered.
fn intervention_dedup_ids(item: &Intervention) -> Vec<MessageId> {
    let mut ids: Vec<MessageId> = item.source_message_ids.clone();
    if !ids.contains(&item.message_id) {
        ids.push(item.message_id);
    }
    ids
}

fn hydrate_pending_queue_into_state(
    state: &mut ChannelMailboxState,
    channel_id: ChannelId,
    disk_items: Vec<Intervention>,
    persistence: QueuePersistenceContext,
    restored_override: Option<ChannelId>,
) -> HydratePendingQueueResult {
    state.last_persistence = Some(persistence.clone());
    let previous_queue = state.intervention_queue.clone();
    // #3864: seed the seen-set with the FULL id set of every live queue item
    // (message_id + source_message_ids), not just message_id, so a restored
    // merged item that overlaps a live item on any source id is recognized as
    // a duplicate. Strictly strengthens the prior message_id-only dedup; the
    // existing disk-hydrate callers only ever pass single-source items, for
    // which this is identical behavior.
    let mut existing_ids: HashSet<MessageId> = state
        .intervention_queue
        .iter()
        .flat_map(intervention_dedup_ids)
        .collect();
    let mut absorbed = 0usize;
    // Walk in reverse so repeated `insert(0, …)` ends up with disk
    // items in their original order.
    for item in disk_items.into_iter().rev() {
        let item_ids = intervention_dedup_ids(&item);
        // Skip ONLY when every id the item represents is already queued. A
        // merged item is dropped whole solely if all of its source messages
        // are present; otherwise the unseen ones would be lost.
        if item_ids.iter().all(|id| existing_ids.contains(id)) {
            continue;
        }
        existing_ids.extend(item_ids);
        state.intervention_queue.insert(0, item);
        absorbed += 1;
    }
    if absorbed > 0 {
        if let Err(error) = persist_queue_or_restore(
            state,
            channel_id,
            &persistence,
            previous_queue,
            "hydrate_pending_queue_from_disk",
        ) {
            return HydratePendingQueueResult {
                absorbed: 0,
                queue_len_after: state.intervention_queue.len(),
                restored_override,
                persistence_error: Some(error),
            };
        }
    }
    HydratePendingQueueResult {
        absorbed,
        queue_len_after: state.intervention_queue.len(),
        restored_override,
        persistence_error: None,
    }
}

fn hydrate_pending_queue_from_disk_if_present(
    state: &mut ChannelMailboxState,
    channel_id: ChannelId,
    persistence: &QueuePersistenceContext,
) -> HydratePendingQueueResult {
    // Boot/recovery loads recover the durable pending-dispatch marker into the
    // queue. A live actor that already owns the in-memory reservation must not
    // re-import its own marker during actor-local disk hydrate (#1683).
    let recover_dispatch_marker = state.pending_user_dispatch.is_none();
    let (disk_items, restored_override) = load_channel_pending_queue_with_marker_recovery(
        &persistence.provider,
        &persistence.token_hash,
        channel_id,
        recover_dispatch_marker,
    );
    if disk_items.is_empty() {
        return HydratePendingQueueResult {
            absorbed: 0,
            queue_len_after: state.intervention_queue.len(),
            restored_override,
            persistence_error: None,
        };
    }

    let mut effective_persistence = persistence.clone();
    if effective_persistence.dispatch_role_override.is_none() {
        effective_persistence.dispatch_role_override =
            restored_override.map(|channel| channel.get());
    }
    hydrate_pending_queue_into_state(
        state,
        channel_id,
        disk_items,
        effective_persistence,
        restored_override,
    )
}

fn finalize_turn_state(
    state: &mut ChannelMailboxState,
    channel_id: ChannelId,
    persistence: Option<&QueuePersistenceContext>,
) -> FinishTurnResult {
    let removed_token = state.cancel_token.take();
    state.active_request_owner = None;
    state.active_user_message_id = None;
    // #3167 — clear the priority class with the rest of the active-turn anchor.
    state.active_turn_kind = ActiveTurnKind::default();
    state.recovery_started_at = None;
    state.turn_started_at = None;
    reset_watchdog_extension_state(state);
    let previous_len = state.intervention_queue.len();
    let previous_queue = state.intervention_queue.clone();
    let pending_result = has_soft_intervention(&mut state.intervention_queue);
    if let Some(persistence) = persistence {
        if state.intervention_queue.len() != previous_len || !state.intervention_queue.is_empty() {
            if let Err(error) = persist_queue_or_restore(
                state,
                channel_id,
                persistence,
                previous_queue,
                "finish_turn",
            ) {
                return FinishTurnResult {
                    removed_token,
                    has_pending: state
                        .intervention_queue
                        .iter()
                        .any(|item| item.mode == InterventionMode::Soft),
                    mailbox_online: true,
                    queue_exit_events: Vec::new(),
                    persistence_error: Some(error),
                };
            }
        }
    }
    FinishTurnResult {
        removed_token,
        has_pending: pending_result.has_pending,
        mailbox_online: true,
        queue_exit_events: pending_result.queue_exit_events,
        persistence_error: None,
    }
}

fn reset_watchdog_extension_state(state: &mut ChannelMailboxState) {
    state.watchdog_deadline_override = None;
    state.watchdog_extension_count = 0;
    state.watchdog_extension_total_secs = 0;
}

fn extend_active_watchdog_deadline(
    state: &mut ChannelMailboxState,
    requested_extend_secs: u64,
) -> Result<WatchdogDeadlineExtension, WatchdogDeadlineExtensionError> {
    let Some(cancel_token) = state.cancel_token.as_ref() else {
        return Err(WatchdogDeadlineExtensionError::NoActiveTurn);
    };

    let count_limit = u32::MAX;
    let total_secs_limit = u64::MAX;
    let applied_extend_secs = requested_extend_secs;

    let now_ms = Utc::now().timestamp_millis();
    let current_deadline = cancel_token.watchdog_deadline_ms.load(Ordering::Relaxed);
    let current_deadline = if current_deadline > 0 {
        current_deadline
    } else {
        now_ms
    };
    let current_max_deadline = cancel_token
        .watchdog_max_deadline_ms
        .load(Ordering::Relaxed);
    let current_max_deadline = if current_max_deadline > 0 {
        current_max_deadline
    } else {
        current_deadline
    };
    let requested_deadline_ms =
        std::cmp::max(current_deadline, now_ms) + requested_extend_secs as i64 * 1000;
    let new_deadline_ms =
        std::cmp::max(current_deadline, now_ms) + applied_extend_secs as i64 * 1000;
    let max_deadline_ms = std::cmp::max(current_max_deadline, new_deadline_ms);

    cancel_token
        .watchdog_deadline_ms
        .store(new_deadline_ms, Ordering::Relaxed);
    cancel_token
        .watchdog_max_deadline_ms
        .store(max_deadline_ms, Ordering::Relaxed);

    state.watchdog_extension_count = state.watchdog_extension_count.saturating_add(1);
    state.watchdog_extension_total_secs = state
        .watchdog_extension_total_secs
        .saturating_add(applied_extend_secs);

    let extension = WatchdogDeadlineExtension {
        requested_deadline_ms,
        new_deadline_ms,
        max_deadline_ms,
        applied_extend_secs,
        requested_extend_secs,
        extension_count: state.watchdog_extension_count,
        extension_count_limit: count_limit,
        extension_total_secs: state.watchdog_extension_total_secs,
        extension_total_secs_limit: total_secs_limit,
        clamped: false,
    };
    state.watchdog_deadline_override = Some(extension);
    Ok(extension)
}

#[cfg(test)]
mod turn_finished_signal_tests {
    use super::*;

    #[tokio::test]
    async fn turn_finished_latch_short_circuits_late_subscribers() {
        let registry = ChannelMailboxRegistry::default();
        let channel_id = ChannelId::new(242_411);
        let handle = registry.handle(channel_id);

        assert!(
            handle
                .try_start_turn(
                    Arc::new(CancelToken::new()),
                    UserId::new(24),
                    MessageId::new(2411),
                )
                .await
        );
        let finished = handle.hard_stop().await;
        assert!(finished.removed_token.is_some());

        let signal =
            ChannelMailboxRegistry::global_turn_finished(channel_id).expect("global signal");
        tokio::time::timeout(std::time::Duration::from_millis(25), signal.wait())
            .await
            .expect("late subscriber should observe latched turn-finished signal");
    }

    #[tokio::test]
    async fn turn_finished_reset_unlatches_on_new_turn() {
        let registry = ChannelMailboxRegistry::default();
        let channel_id = ChannelId::new(242_412);
        let handle = registry.handle(channel_id);

        assert!(
            handle
                .try_start_turn(
                    Arc::new(CancelToken::new()),
                    UserId::new(24),
                    MessageId::new(2412),
                )
                .await
        );
        let _ = handle.hard_stop().await;
        let signal = registry.turn_finished(channel_id);
        tokio::time::timeout(std::time::Duration::from_millis(25), signal.wait())
            .await
            .expect("finished turn should latch signal");

        assert!(
            handle
                .try_start_turn(
                    Arc::new(CancelToken::new()),
                    UserId::new(24),
                    MessageId::new(2413),
                )
                .await
        );
        let still_waiting =
            tokio::time::timeout(std::time::Duration::from_millis(25), signal.wait()).await;
        assert!(
            still_waiting.is_err(),
            "new active turn should reset the previous finished latch"
        );

        let _ = handle.hard_stop().await;
        tokio::time::timeout(std::time::Duration::from_millis(250), signal.wait())
            .await
            .expect("fresh finish should wake reset waiter");
    }
}

fn spawn_channel_mailbox(channel_id: ChannelId) -> ChannelMailboxHandle {
    let (tx, mut rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut state = ChannelMailboxState::default();
        while let Some(msg) = rx.recv().await {
            // #3297 r3 — tombstoned actor refuses start-like arms (enum docs).
            let Some(msg) = registry_purge::gate_closed_arm(&state, msg) else {
                continue;
            };
            match msg {
                ChannelMailboxMsg::Snapshot { reply } => {
                    let _ = reply.send(ChannelMailboxSnapshot {
                        cancel_token: state.cancel_token.clone(),
                        active_request_owner: state.active_request_owner,
                        active_user_message_id: state.active_user_message_id,
                        active_turn_kind: state.active_turn_kind,
                        intervention_queue: state.intervention_queue.clone(),
                        recovery_started_at: state.recovery_started_at,
                        turn_started_at: state.turn_started_at,
                    });
                }
                ChannelMailboxMsg::HasActiveTurn { reply } => {
                    let _ = reply.send(state.cancel_token.is_some());
                }
                ChannelMailboxMsg::HasBlockingActiveTurn { reply } => {
                    // #3167 — a background turn (monitor relay / TUI loop)
                    // does not block dequeuing a queued user intervention.
                    let _ = reply.send(
                        state.cancel_token.is_some() && !state.active_turn_kind.is_background(),
                    );
                }
                ChannelMailboxMsg::ActiveTurnKind { reply } => {
                    // #3167 — `None` when idle; otherwise the slot's kind.
                    let kind = state.cancel_token.as_ref().map(|_| state.active_turn_kind);
                    let _ = reply.send(kind);
                }
                ChannelMailboxMsg::CancelToken { reply } => {
                    let _ = reply.send(state.cancel_token.clone());
                }
                ChannelMailboxMsg::CancelActiveTurnWithReason { reason, reply } => {
                    // #2374 — atomic, actor-serialized "reason then flip"
                    // (full race rationale on the
                    // `cancel_active_turn_with_reason` handle doc). Guard
                    // mirrors #2373: never overwrite a reason once
                    // `cancelled` is set — earlier attribution wins.
                    let token = state.cancel_token.clone();
                    let already_stopping = token.as_ref().is_some_and(|token| {
                        token.cancelled.load(std::sync::atomic::Ordering::Relaxed)
                    });
                    if let Some(token) = token.as_ref()
                        && !already_stopping
                    {
                        token.set_cancel_source(reason.clone());
                        token
                            .cancelled
                            .store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                    let _ = reply.send(CancelActiveTurnResult {
                        token,
                        already_stopping,
                    });
                }
                ChannelMailboxMsg::CancelActiveTurnIfCurrent {
                    expected_token,
                    reply,
                } => {
                    let token = state
                        .cancel_token
                        .clone()
                        .filter(|token| Arc::ptr_eq(token, &expected_token));
                    let already_stopping = token.as_ref().is_some_and(|token| {
                        token.cancelled.load(std::sync::atomic::Ordering::Relaxed)
                    });
                    if let Some(token) = token.as_ref()
                        && !already_stopping
                    {
                        token
                            .cancelled
                            .store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                    let _ = reply.send(CancelActiveTurnResult {
                        token,
                        already_stopping,
                    });
                }
                ChannelMailboxMsg::CancelActiveTurnIfCurrentWithReason {
                    expected_token,
                    reason,
                    reply,
                } => {
                    // #2374 — atomic reason-then-flip with the
                    // `if_current` guard preserved. See the unguarded
                    // variant above for the broader rationale.
                    let token = state
                        .cancel_token
                        .clone()
                        .filter(|token| Arc::ptr_eq(token, &expected_token));
                    let already_stopping = token.as_ref().is_some_and(|token| {
                        token.cancelled.load(std::sync::atomic::Ordering::Relaxed)
                    });
                    if let Some(token) = token.as_ref()
                        && !already_stopping
                    {
                        token.set_cancel_source(reason.clone());
                        token
                            .cancelled
                            .store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                    let _ = reply.send(CancelActiveTurnResult {
                        token,
                        already_stopping,
                    });
                }
                ChannelMailboxMsg::CancelActiveTurnIfUserMessageWithReason {
                    expected_user_message_id,
                    reason,
                    reply,
                } => {
                    // #2374 Codex round-1 fix (HIGH-1): identity check +
                    // cancel as one serialized step, keyed by
                    // `user_message_id` (full rationale on the
                    // `cancel_active_turn_if_user_message_with_reason`
                    // handle doc).
                    let identity_matches = state
                        .active_user_message_id
                        .is_some_and(|id| id == expected_user_message_id);
                    let token = if identity_matches {
                        state.cancel_token.clone()
                    } else {
                        None
                    };
                    let already_stopping = token.as_ref().is_some_and(|token| {
                        token.cancelled.load(std::sync::atomic::Ordering::Relaxed)
                    });
                    if let Some(token) = token.as_ref()
                        && !already_stopping
                    {
                        token.set_cancel_source(reason.clone());
                        token
                            .cancelled
                            .store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                    let _ = reply.send(CancelActiveTurnResult {
                        token,
                        already_stopping,
                    });
                }
                ChannelMailboxMsg::CancelActiveBackgroundTurnIfCurrent { reply } => {
                    // #3167 — atomic kind-guarded supersede: cancel ONLY a
                    // background-held slot (reason+flip mirror
                    // `CancelActiveTurnWithReason`; slot release stays with the
                    // turn's own finalizer). #3167 BLOCKER-1: reply `true` only
                    // for a NEW cancel — `true` on an already-cancelling slot
                    // would hot-loop the caller's immediate re-kick. Full
                    // rationale on the handle + enum variant docs.
                    let is_background_active =
                        state.cancel_token.is_some() && state.active_turn_kind.is_background();
                    let newly_cancelled = if is_background_active {
                        match state.cancel_token.as_ref() {
                            Some(token)
                                if !token.cancelled.load(std::sync::atomic::Ordering::Relaxed) =>
                            {
                                token.set_cancel_source(
                                    "idle_queue_user_supersede_background".to_string(),
                                );
                                token
                                    .cancelled
                                    .store(true, std::sync::atomic::Ordering::Relaxed);
                                true
                            }
                            // Already cancelling (or, defensively, no token): no-op.
                            _ => false,
                        }
                    } else {
                        false
                    };
                    let _ = reply.send(newly_cancelled);
                }
                ChannelMailboxMsg::TryStartTurn {
                    cancel_token,
                    request_owner,
                    user_message_id,
                    turn_kind,
                    reply,
                } => {
                    // #3167 BLOCKER-2 — background yields to a queued backlog AND
                    // to a reserved dequeue→claim window. The start rule used to
                    // only check `cancel_token.is_some()`. After a background
                    // finalizer releases the slot, another background cycle
                    // (monitor relay / self-paced TUI loop) could win the race
                    // for the freed slot AHEAD of the deferred kickoff that
                    // drains a queued user intervention — starving the user
                    // indefinitely. Refuse a Background start whenever a backlog
                    // is already queued, OR while a `pending_user_dispatch`
                    // reservation is live: `TakeNextSoft` REMOVES the queued
                    // head before the dequeued user turn actually claims the
                    // slot, leaving an EMPTY queue during that window — without
                    // the reservation a Background start would slip in and
                    // race-win ahead of the user. A `false` return is the
                    // background callers' normal lost-race path (they do not
                    // error or hot-spin; the watcher relays terminal output
                    // independently of the mailbox slot, so no output is
                    // dropped). UserOrAgent starts are UNCHANGED.
                    let queue_non_empty = !state.intervention_queue.is_empty();
                    let reservation_held = state.pending_user_dispatch.is_some();
                    let background_yields =
                        turn_kind.is_background() && (queue_non_empty || reservation_held);
                    // SAFETY VALVE: only the dequeue→claim window (queue empty,
                    // reservation held) can deadlock if the dequeued user turn is
                    // lost. Count those refusals; a queue-backed refusal is a real
                    // backlog and is never counted. After N consecutive
                    // reservation-only refusals, drop the (possibly stale)
                    // reservation so Background can proceed next time.
                    if background_yields && !queue_non_empty && reservation_held {
                        state.pending_user_dispatch_yield_count += 1;
                        if state.pending_user_dispatch_yield_count
                            >= PENDING_USER_DISPATCH_MAX_YIELDS
                        {
                            state.pending_user_dispatch = None;
                            state.pending_user_dispatch_yield_count = 0;
                        }
                    }
                    let started = if state.cancel_token.is_some() || background_yields {
                        false
                    } else {
                        reset_turn_finished_signal(channel_id);
                        state.cancel_token = Some(cancel_token);
                        state.active_request_owner = Some(request_owner);
                        state.active_user_message_id = Some(user_message_id);
                        // #3167 — record the slot's priority class so the
                        // dequeue gates can treat a background turn as
                        // non-blocking.
                        state.active_turn_kind = turn_kind;
                        // #3167 BLOCKER-2 — a real (UserOrAgent) turn claiming the
                        // slot satisfies any reserved dequeue→claim window: clear
                        // the reservation and reset the valve counter.
                        if turn_kind == ActiveTurnKind::UserOrAgent {
                            if state.pending_user_dispatch.is_some() {
                                delete_pending_dispatch_marker_for_state(
                                    &state,
                                    channel_id,
                                    "try_start_turn",
                                );
                            }
                            state.pending_user_dispatch = None;
                            state.pending_user_dispatch_yield_count = 0;
                        }
                        state.recovery_started_at = None;
                        state.turn_started_at = Some(Utc::now());
                        reset_watchdog_extension_state(&mut state);
                        true
                    };
                    let _ = reply.send(started);
                }
                ChannelMailboxMsg::RestoreActiveTurn {
                    cancel_token,
                    request_owner,
                    user_message_id,
                    turn_kind,
                    reply,
                } => {
                    reset_turn_finished_signal(channel_id);
                    let was_idle = state.cancel_token.is_none();
                    state.cancel_token = Some(cancel_token);
                    state.active_request_owner = Some(request_owner);
                    state.active_user_message_id = Some(user_message_id);
                    // #3167 — preserve the priority class across the re-bind.
                    state.active_turn_kind = turn_kind;
                    if was_idle || state.turn_started_at.is_none() {
                        state.turn_started_at = Some(Utc::now());
                    }
                    reset_watchdog_extension_state(&mut state);
                    let _ = reply.send(());
                }
                ChannelMailboxMsg::RecoveryKickoff {
                    cancel_token,
                    request_owner,
                    user_message_id,
                    reply,
                } => {
                    reset_turn_finished_signal(channel_id);
                    let activated_turn = state.cancel_token.is_none();
                    state.cancel_token = Some(cancel_token);
                    state.active_request_owner = Some(request_owner);
                    state.active_user_message_id = user_message_id;
                    // #3167 — a recovery turn is a real (non-background) turn.
                    state.active_turn_kind = ActiveTurnKind::default();
                    state.recovery_started_at = Some(Instant::now());
                    if activated_turn || state.turn_started_at.is_none() {
                        state.turn_started_at = Some(Utc::now());
                    }
                    reset_watchdog_extension_state(&mut state);
                    let _ = reply.send(RecoveryKickoffResult {
                        activated_turn,
                        refused_closed: false,
                    });
                }
                ChannelMailboxMsg::ClearRecoveryMarker { reply } => {
                    state.recovery_started_at = None;
                    let _ = reply.send(());
                }
                ChannelMailboxMsg::Enqueue {
                    intervention,
                    persistence,
                    reply,
                } => {
                    state.last_persistence = Some(persistence.clone());
                    let hydrate_result = hydrate_pending_queue_from_disk_if_present(
                        &mut state,
                        channel_id,
                        &persistence,
                    );
                    if let Some(error) = hydrate_result.persistence_error {
                        let _ = reply.send(EnqueueInterventionResult {
                            enqueued: false,
                            merged: false,
                            refusal_reason: None,
                            queue_exit_events: Vec::new(),
                            persistence_error: Some(error),
                        });
                        continue;
                    }
                    let previous_queue = state.intervention_queue.clone();
                    let mut enqueue_result =
                        enqueue_intervention(&mut state.intervention_queue, intervention);
                    if enqueue_result.enqueued
                        && let Err(error) = persist_queue_or_restore(
                            &mut state,
                            channel_id,
                            &persistence,
                            previous_queue,
                            "enqueue",
                        )
                    {
                        enqueue_result = EnqueueInterventionResult {
                            enqueued: false,
                            merged: false,
                            refusal_reason: None,
                            queue_exit_events: Vec::new(),
                            persistence_error: Some(error),
                        };
                    }
                    let _ = reply.send(enqueue_result);
                }
                ChannelMailboxMsg::HasPendingSoftQueue { persistence, reply } => {
                    state.last_persistence = Some(persistence.clone());
                    let previous_len = state.intervention_queue.len();
                    let previous_queue = state.intervention_queue.clone();
                    let mut pending_result = has_soft_intervention(&mut state.intervention_queue);
                    if state.intervention_queue.len() != previous_len
                        && let Err(error) = persist_queue_or_restore(
                            &mut state,
                            channel_id,
                            &persistence,
                            previous_queue,
                            "has_pending_soft_queue",
                        )
                    {
                        pending_result = HasPendingSoftQueueResult {
                            has_pending: state
                                .intervention_queue
                                .iter()
                                .any(|item| item.mode == InterventionMode::Soft),
                            queue_exit_events: Vec::new(),
                            persistence_error: Some(error),
                        };
                    }
                    let _ = reply.send(pending_result);
                }
                ChannelMailboxMsg::TakeNextSoft { persistence, reply } => {
                    state.last_persistence = Some(persistence.clone());
                    let previous_queue = state.intervention_queue.clone();
                    let next_result = dequeue_next_soft_intervention(&mut state.intervention_queue);
                    let queue_len_after = state.intervention_queue.len();
                    // #3167 BLOCKER-2 — capture the dispatched head id BEFORE the
                    // intervention is moved into the reply, so we can reserve the
                    // dequeue→claim window against a racing Background start.
                    let dispatched_head = next_result.intervention.as_ref().map(|i| i.message_id);
                    let marker_error = if let Some(intervention) = next_result.intervention.as_ref()
                    {
                        save_channel_pending_dispatch_marker(
                            &persistence.provider,
                            &persistence.token_hash,
                            channel_id,
                            intervention,
                            persistence.dispatch_role_override,
                        )
                        .err()
                    } else {
                        None
                    };
                    let result = if let Some(error) = marker_error {
                        state.intervention_queue = previous_queue;
                        log_queue_persistence_rollback(
                            "take_next_soft_marker",
                            channel_id,
                            &persistence,
                            &error,
                        );
                        TakeNextSoftResult {
                            intervention: None,
                            has_more: state
                                .intervention_queue
                                .iter()
                                .any(|item| item.mode == InterventionMode::Soft),
                            queue_len_after: state.intervention_queue.len(),
                            queue_exit_events: Vec::new(),
                            persistence_error: Some(error),
                        }
                    } else if let Err(error) = persist_queue_or_restore(
                        &mut state,
                        channel_id,
                        &persistence,
                        previous_queue,
                        "take_next_soft",
                    ) {
                        // Persistence failed → `persist_queue_or_restore` rolled
                        // the dequeue back (head re-inserted); no dispatch happens,
                        // so do NOT set the reservation.
                        if let Err(marker_error) = remove_channel_pending_dispatch_marker(
                            &persistence.provider,
                            &persistence.token_hash,
                            channel_id,
                        ) {
                            tracing::warn!(
                                provider = persistence.provider.as_str(),
                                token_hash = %persistence.token_hash,
                                channel_id = channel_id.get(),
                                error = %marker_error,
                                "failed to remove pending dispatch marker after take_next_soft persistence failure"
                            );
                        }
                        TakeNextSoftResult {
                            intervention: None,
                            has_more: state
                                .intervention_queue
                                .iter()
                                .any(|item| item.mode == InterventionMode::Soft),
                            queue_len_after: state.intervention_queue.len(),
                            queue_exit_events: Vec::new(),
                            persistence_error: Some(error),
                        }
                    } else {
                        // #3167 BLOCKER-2 — a head was handed out for dispatch but
                        // the slot is not claimed until `intake_turn` runs. Reserve
                        // the window so a Background start cannot slip in ahead.
                        if let Some(head) = dispatched_head {
                            state.pending_user_dispatch = Some(head);
                            state.pending_user_dispatch_yield_count = 0;
                        }
                        TakeNextSoftResult {
                            intervention: next_result.intervention,
                            has_more: next_result.has_more,
                            queue_len_after,
                            queue_exit_events: next_result.queue_exit_events,
                            persistence_error: None,
                        }
                    };
                    let _ = reply.send(result);
                }
                ChannelMailboxMsg::RequeueFront {
                    intervention,
                    persistence,
                    reply,
                } => {
                    state.last_persistence = Some(persistence.clone());
                    // #3167 BLOCKER-2 — a failed dispatch requeues the reserved
                    // head: clear the dequeue→claim reservation so the now
                    // non-empty queue (not the stale reservation) governs the
                    // Background gate, and reset the safety-valve counter.
                    let requeued_id = intervention.message_id;
                    let requeued_reserved = state.pending_user_dispatch == Some(requeued_id);
                    let previous_queue = state.intervention_queue.clone();
                    let requeue_result =
                        requeue_intervention_front(&mut state.intervention_queue, intervention);
                    let result = if let Err(error) = persist_queue_or_restore(
                        &mut state,
                        channel_id,
                        &persistence,
                        previous_queue,
                        "requeue_front",
                    ) {
                        RequeueInterventionResult {
                            queue_exit_events: Vec::new(),
                            persistence_error: Some(error),
                        }
                    } else {
                        if requeued_reserved {
                            delete_pending_dispatch_marker_for_state(
                                &state,
                                channel_id,
                                "requeue_front",
                            );
                            state.pending_user_dispatch = None;
                            state.pending_user_dispatch_yield_count = 0;
                        }
                        RequeueInterventionResult {
                            queue_exit_events: requeue_result,
                            persistence_error: None,
                        }
                    };
                    let _ = reply.send(result);
                }
                ChannelMailboxMsg::CancelQueuedMessage {
                    message_id,
                    persistence,
                    reply,
                } => {
                    state.last_persistence = Some(persistence.clone());
                    let previous_queue = state.intervention_queue.clone();
                    let mut cancel_result = cancel_soft_intervention_by_message_id(
                        &mut state.intervention_queue,
                        message_id,
                    );
                    if cancel_result.removed.is_some()
                        || !cancel_result.queue_exit_events.is_empty()
                    {
                        if let Err(error) = persist_queue_or_restore(
                            &mut state,
                            channel_id,
                            &persistence,
                            previous_queue,
                            "cancel_queued_message",
                        ) {
                            cancel_result = CancelQueuedMessageResult {
                                removed: None,
                                queue_exit_events: Vec::new(),
                                persistence_error: Some(error),
                            };
                        }
                    }
                    let _ = reply.send(cancel_result);
                }
                ChannelMailboxMsg::FinishTurn { persistence, reply } => {
                    state.last_persistence = Some(persistence.clone());
                    let _ = reply.send(finalize_turn_state(
                        &mut state,
                        channel_id,
                        Some(&persistence),
                    ));
                    mark_turn_finished_signal_done(channel_id);
                }
                ChannelMailboxMsg::FinishTurnIfMatches {
                    expected_user_message_id,
                    persistence,
                    reply,
                } => {
                    // #3016 — identity guard. Finalize ONLY when the active
                    // turn's user_message_id still matches the terminal's
                    // identity. A mismatch (or no active turn) means the turn
                    // this terminal belonged to already finalized and a newer
                    // turn may now own the mailbox — so we must NOT take its
                    // token. Return a no-op result (removed_token = None) that
                    // mirrors `mailbox_finish_turn`'s idempotent second-call
                    // shape, so the finalizer's `removed_token.is_some()` gate
                    // skips the counter decrement and trailing release.
                    let matches = state
                        .active_user_message_id
                        .is_some_and(|active| active == expected_user_message_id);
                    if matches {
                        state.last_persistence = Some(persistence.clone());
                        let _ = reply.send(finalize_turn_state(
                            &mut state,
                            channel_id,
                            Some(&persistence),
                        ));
                        mark_turn_finished_signal_done(channel_id);
                    } else {
                        // No-op: do not touch the active token. Surface the
                        // current pending state so a caller that schedules a
                        // queue kickoff still sees an accurate backlog flag,
                        // but never release the (possibly newer) live turn.
                        let _ = reply.send(FinishTurnResult {
                            removed_token: None,
                            has_pending: state
                                .intervention_queue
                                .iter()
                                .any(|item| item.mode == InterventionMode::Soft),
                            mailbox_online: true,
                            queue_exit_events: Vec::new(),
                            persistence_error: None,
                        });
                    }
                }
                ChannelMailboxMsg::HardStop { reply } => {
                    let persistence = state.last_persistence.clone();
                    let _ = reply.send(finalize_turn_state(
                        &mut state,
                        channel_id,
                        persistence.as_ref(),
                    ));
                    mark_turn_finished_signal_done(channel_id);
                }
                ChannelMailboxMsg::FinishCancelledTurn { reply } => {
                    let should_finish = state.cancel_token.as_ref().is_some_and(|token| {
                        token.cancelled.load(std::sync::atomic::Ordering::Relaxed)
                    });
                    if should_finish {
                        let persistence = state.last_persistence.clone();
                        let _ = reply.send(finalize_turn_state(
                            &mut state,
                            channel_id,
                            persistence.as_ref(),
                        ));
                        mark_turn_finished_signal_done(channel_id);
                    } else {
                        let _ = reply.send(FinishTurnResult {
                            removed_token: None,
                            has_pending: state
                                .intervention_queue
                                .iter()
                                .any(|item| item.mode == InterventionMode::Soft),
                            mailbox_online: true,
                            queue_exit_events: Vec::new(),
                            persistence_error: None,
                        });
                    }
                }
                ChannelMailboxMsg::Clear { persistence, reply } => {
                    state.last_persistence = Some(persistence.clone());
                    let removed_token = state.cancel_token.take();
                    state.active_request_owner = None;
                    state.active_user_message_id = None;
                    // #3167 — clear the priority class with the anchor.
                    state.active_turn_kind = ActiveTurnKind::default();
                    state.recovery_started_at = None;
                    state.turn_started_at = None;
                    reset_watchdog_extension_state(&mut state);
                    let previous_queue = state.intervention_queue.clone();
                    let queue_exit_events = state
                        .intervention_queue
                        .drain(..)
                        .map(|intervention| {
                            QueueExitEvent::new(intervention, QueueExitKind::Superseded)
                        })
                        .collect();
                    let result = if let Err(error) = persist_queue_or_restore(
                        &mut state,
                        channel_id,
                        &persistence,
                        previous_queue,
                        "clear",
                    ) {
                        ClearChannelResult {
                            removed_token,
                            queue_exit_events: Vec::new(),
                            persistence_error: Some(error),
                        }
                    } else {
                        ClearChannelResult {
                            removed_token,
                            queue_exit_events,
                            persistence_error: None,
                        }
                    };
                    let _ = reply.send(result);
                    mark_turn_finished_signal_done(channel_id);
                }
                ChannelMailboxMsg::PurgeQueue {
                    persistence,
                    clear_cancelled_active_anchor,
                    reply,
                } => {
                    // #2706: queue-only purge. Leaves `cancel_token`,
                    // `active_request_owner`, `active_user_message_id`
                    // untouched so a turn that entered the actor in
                    // between force-kill and purge is not collaterally
                    // cancelled.
                    //
                    // #3029(D): a force purge additionally releases the
                    // active-turn anchor, but ONLY when the anchored token is
                    // already `cancelled`. The force path flips that flag via
                    // `cancel_active_token` before purging, so this clears the
                    // just-killed turn's anchor while still leaving a fresh,
                    // uncancelled turn (which raced in after the force-kill)
                    // fully intact — keeping the #2706 guarantee.
                    let cleared_active_anchor = if clear_cancelled_active_anchor
                        && state.cancel_token.as_ref().is_some_and(|token| {
                            token.cancelled.load(std::sync::atomic::Ordering::Relaxed)
                        }) {
                        state.cancel_token = None;
                        state.active_request_owner = None;
                        state.active_user_message_id = None;
                        // #3167 — clear the priority class with the anchor.
                        state.active_turn_kind = ActiveTurnKind::default();
                        state.recovery_started_at = None;
                        state.turn_started_at = None;
                        reset_watchdog_extension_state(&mut state);
                        true
                    } else {
                        false
                    };
                    state.last_persistence = Some(persistence.clone());
                    let previous_queue = state.intervention_queue.clone();
                    let drained = state.intervention_queue.drain(..).count();
                    let drained = if persist_queue_or_restore(
                        &mut state,
                        channel_id,
                        &persistence,
                        previous_queue,
                        "purge_queue",
                    )
                    .is_err()
                    {
                        0
                    } else {
                        drained
                    };
                    if cleared_active_anchor {
                        mark_turn_finished_signal_done(channel_id);
                    }
                    let _ = reply.send(PurgeQueueResult {
                        drained,
                        cleared_active_anchor,
                    });
                }
                #[cfg(test)]
                ChannelMailboxMsg::ReplaceQueue {
                    queue,
                    persistence,
                    reply,
                } => {
                    state.last_persistence = Some(persistence.clone());
                    let previous_queue = state.intervention_queue.clone();
                    state.intervention_queue = queue;
                    let _ = persist_queue_or_restore(
                        &mut state,
                        channel_id,
                        &persistence,
                        previous_queue,
                        "replace_queue",
                    );
                    let _ = reply.send(());
                }
                ChannelMailboxMsg::HydratePendingQueueFromDisk { persistence, reply } => {
                    // #1683: read the disk queue inside the mailbox actor so
                    // a dequeue that removes the file cannot race with a stale
                    // out-of-actor disk snapshot and reinsert an already
                    // processed item.
                    let result = hydrate_pending_queue_from_disk_if_present(
                        &mut state,
                        channel_id,
                        &persistence,
                    );
                    let _ = reply.send(result);
                }
                ChannelMailboxMsg::MergeRestoredQueueItems {
                    items,
                    persistence,
                    reply,
                } => {
                    // #3864: merge SIGTERM-restored disk items into the live
                    // queue in ONE serialized actor step (read + dedup-merge +
                    // persist). Immune to the lost-enqueue race the old
                    // out-of-actor snapshot→build→`ReplaceQueue` RMW suffered:
                    // a live reconcile-window `Enqueue` is serialized before
                    // or after this merge, never overwritten by it. override =
                    // None — dispatch_role_overrides are restored separately,
                    // before the restore loop (see recovery_flush).
                    let result = hydrate_pending_queue_into_state(
                        &mut state,
                        channel_id,
                        items,
                        persistence,
                        None,
                    );
                    let _ = reply.send(result);
                }
                ChannelMailboxMsg::RestartDrain { persistence, reply } => {
                    state.last_persistence = Some(persistence.clone());
                    let persistence_error =
                        persist_queue(channel_id, &state.intervention_queue, &persistence).err();
                    let _ = reply.send(RestartDrainResult {
                        queued_count: if persistence_error.is_some() {
                            0
                        } else {
                            state.intervention_queue.len()
                        },
                        persistence_error,
                    });
                }
                ChannelMailboxMsg::ExtendTimeout {
                    extend_by_secs,
                    reply,
                } => {
                    let _ = reply.send(extend_active_watchdog_deadline(&mut state, extend_by_secs));
                }
                ChannelMailboxMsg::TakeTimeoutOverride { reply } => {
                    let _ = reply.send(state.watchdog_deadline_override.take());
                }
                ChannelMailboxMsg::ClearTimeoutOverride { reply } => {
                    state.watchdog_deadline_override = None;
                    let _ = reply.send(());
                }
                ChannelMailboxMsg::CloseIfIdle { reply } => {
                    let _ = reply.send(registry_purge::close_if_idle_verdict(&mut state));
                }
            }
        }
    });
    ChannelMailboxHandle { sender: tx }
}

// #3167 BLOCKER-3 — a SINGLE process-wide lock shared by EVERY test in this
// file that mutates (or depends on) the process-global `AGENTDESK_ROOT_DIR`
// env (the durable-queue persistence root). Previously each test module
// declared its OWN `static TEST_ENV_LOCK`; separate Mutex instances do NOT
// serialize across modules, so under the default parallel `cargo test --lib`
// an env-mutating test in module A could clobber the root that an env-reading
// test in module B (e.g. `purge_queue_tests`) relied on → spurious failures.
// All env-touching test modules below now share THIS one lock, so any two such
// tests are mutually exclusive regardless of which module they live in.
#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::{MutexGuard, PoisonError};

    pub(crate) const AGENTDESK_ROOT_DIR_ENV: &str = "AGENTDESK_ROOT_DIR";

    /// The SINGLE crate-wide env lock. `.lock()` delegates to
    /// `crate::config::shared_test_env_lock()` so EVERY turn_orchestrator env
    /// test serializes against every OTHER env-mutating test in the crate
    /// (config / tmux_watcher / turn_finalizer / standby_relay). A module-local
    /// `Mutex` (the previous impl) only serialized within turn_orchestrator and
    /// let a concurrent root-mutating test on the config lock (e.g. tmux_watcher)
    /// stomp the tempdir `AGENTDESK_ROOT_DIR` env mid-test. This zero-sized type
    /// keeps the `TEST_ENV_LOCK.lock()` call shape so all existing callers are
    /// unchanged while routing through the one shared mutex.
    pub(crate) struct SharedEnvLock;

    impl SharedEnvLock {
        pub(crate) fn lock(
            &self,
        ) -> Result<MutexGuard<'static, ()>, PoisonError<MutexGuard<'static, ()>>> {
            crate::config::shared_test_env_lock().lock()
        }
    }

    pub(crate) static TEST_ENV_LOCK: SharedEnvLock = SharedEnvLock;

    pub(crate) fn lock_test_env() -> MutexGuard<'static, ()> {
        TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod actor_hydrate_regression_tests {
    use super::test_support::TEST_ENV_LOCK;
    use super::*;
    use std::path::{Path, PathBuf};
    use std::sync::MutexGuard;
    use std::time::SystemTime;

    const AGENTDESK_ROOT_DIR_ENV: &str = "AGENTDESK_ROOT_DIR";

    struct EnvGuard;

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe { std::env::remove_var(AGENTDESK_ROOT_DIR_ENV) };
        }
    }

    fn queue_file_path(
        root: &Path,
        provider: &ProviderKind,
        token_hash: &str,
        channel_id: ChannelId,
    ) -> PathBuf {
        root.join("runtime")
            .join("discord_pending_queue")
            .join(provider.as_str())
            .join(token_hash)
            .join(format!("{}.json", channel_id.get()))
    }

    fn make_intervention(message_id: u64, text: &str, created_at: Instant) -> Intervention {
        Intervention {
            author_id: UserId::new(1),
            author_is_bot: false,
            message_id: MessageId::new(message_id),
            source_message_ids: vec![MessageId::new(message_id)],
            text: text.to_string(),
            mode: InterventionMode::Soft,
            created_at,
            reply_context: None,
            has_reply_boundary: false,
            merge_consecutive: false,
            pending_uploads: Vec::new(),
            voice_announcement: None,
        }
    }

    fn make_intervention_with_sources(
        message_id: u64,
        source_ids: &[u64],
        text: &str,
        created_at: Instant,
    ) -> Intervention {
        Intervention {
            source_message_ids: source_ids.iter().copied().map(MessageId::new).collect(),
            ..make_intervention(message_id, text, created_at)
        }
    }

    fn lock_test_env() -> MutexGuard<'static, ()> {
        TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Drive an async body to completion on a fresh current-thread runtime.
    /// Used by the env-locked queue tests so the `lock_test_env()` guard is
    /// held across a *synchronous* `block_on` rather than across an `.await` —
    /// keeping the global `AGENTDESK_ROOT_DIR` env stable for the duration
    /// WITHOUT a `#[allow(clippy::await_holding_lock)]` site (#3034 ratchet).
    fn run_async<F: std::future::Future>(fut: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(fut)
    }

    #[test]
    fn remove_channel_pending_queue_files_all_tokens_only_removes_target_channel() {
        let _lock = lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, tmp.path().to_str().unwrap()) };
        let _env_guard = EnvGuard;

        let provider = ProviderKind::Claude;
        let channel_id = ChannelId::new(2708);
        let other_channel_id = ChannelId::new(2709);
        let first = queue_file_path(tmp.path(), &provider, "token-a", channel_id);
        let second = queue_file_path(tmp.path(), &provider, "token-b", channel_id);
        let other = queue_file_path(tmp.path(), &provider, "token-a", other_channel_id);
        for path in [&first, &second, &other] {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "[]").unwrap();
        }

        let removed = remove_channel_pending_queue_files_all_tokens(&provider, channel_id);

        assert_eq!(removed, 2);
        assert!(!first.exists());
        assert!(!second.exists());
        assert!(other.exists());
    }

    // SAFETY (await_holding_lock): the test-env Mutex is held across awaits to
    // serialize tests that mutate the process-global `AGENTDESK_ROOT_DIR` env;
    // releasing before the awaits would race concurrent tests. Test-only.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn hydrate_from_disk_does_not_reinsert_after_actor_dequeue_removed_file() {
        let _lock = lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, tmp.path().to_str().unwrap()) };
        let _env_guard = EnvGuard;

        let provider = ProviderKind::Claude;
        let token_hash = "mailbox-hydrate-after-dequeue";
        let channel_id = ChannelId::new(45);
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(channel_id);
        let persistence = QueuePersistenceContext::new(&provider, token_hash, None);

        handle
            .replace_queue(
                vec![make_intervention(10, "already-processed", Instant::now())],
                persistence.clone(),
            )
            .await;
        let path = queue_file_path(tmp.path(), &provider, token_hash, channel_id);
        assert!(path.exists(), "queue file must exist before dequeue");

        let taken = handle.take_next_soft(persistence.clone()).await;
        assert_eq!(
            taken.intervention.as_ref().map(|item| item.message_id),
            Some(MessageId::new(10))
        );
        assert_eq!(taken.queue_len_after, 0);
        assert!(
            !path.exists(),
            "actor dequeue must remove the disk file once the queue is empty"
        );

        let hydrate = handle.hydrate_pending_queue_from_disk(persistence).await;
        assert_eq!(
            hydrate.absorbed, 0,
            "#1683: actor-local disk hydrate must see the removed file, not reinsert a stale pre-dequeue snapshot"
        );
        assert_eq!(hydrate.queue_len_after, 0);
        assert!(handle.snapshot().await.intervention_queue.is_empty());
    }

    /// #3864 PRIMARY regression: a live reconcile-window `Enqueue` that lands
    /// before the SIGTERM restore merge must be PRESERVED, not overwritten.
    /// The old out-of-actor snapshot→build→`ReplaceQueue` RMW blind-replaced
    /// the queue and silently dropped the live message from BOTH memory and
    /// disk; the in-actor merge front-inserts the restored item ahead of the
    /// live one and persists both atomically.
    ///
    /// Sync test driving the actor via `run_async`/`block_on` so the env lock
    /// guard is not held across an `.await` (no await_holding_lock site).
    #[test]
    fn merge_restored_items_preserves_concurrent_live_enqueue() {
        let _lock = lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, tmp.path().to_str().unwrap()) };
        let _env_guard = EnvGuard;

        run_async(async {
            let provider = ProviderKind::Claude;
            let token_hash = "merge-restored-preserves-live";
            let channel_id = ChannelId::new(3864001);
            let registry = ChannelMailboxRegistry::default();
            let handle = registry.handle(channel_id);
            let persistence = QueuePersistenceContext::new(&provider, token_hash, None);

            // Live reconcile-window message B lands first (actor `Enqueue`).
            let live = handle
                .enqueue(
                    make_intervention(200, "live-during-reconcile", Instant::now()),
                    persistence.clone(),
                )
                .await;
            assert!(live.enqueued, "live reconcile-window enqueue must succeed");

            // SIGTERM-restored item A is merged AFTER (loaded out-of-actor,
            // handed to the actor as items). It must NOT clobber the live B.
            let result = handle
                .merge_restored_queue_items(
                    vec![make_intervention(
                        100,
                        "restored-from-sigterm",
                        Instant::now(),
                    )],
                    persistence.clone(),
                )
                .await;
            assert_eq!(result.absorbed, 1, "restored item A is absorbed");
            assert_eq!(result.queue_len_after, 2);
            assert!(result.persistence_error.is_none());

            // In memory: [A, B] — restored (older) front-inserted ahead of live.
            let queue = handle.snapshot().await.intervention_queue;
            let ids: Vec<u64> = queue.iter().map(|i| i.message_id.get()).collect();
            assert_eq!(
                ids,
                vec![100, 200],
                "merge must keep the live enqueue and front-insert the restored item"
            );

            // On disk: the same [A, B] (the old ReplaceQueue would persist only [A]).
            let (disk, _override) = load_channel_pending_queue(&provider, token_hash, channel_id);
            let disk_ids: Vec<u64> = disk.iter().map(|i| i.message_id.get()).collect();
            assert_eq!(
                disk_ids,
                vec![100, 200],
                "both the restored and the live item must be durably persisted"
            );
        });
    }

    /// #3864 order: multiple restored items keep their original order and are
    /// all front-inserted ahead of the (newer) live queue item.
    #[test]
    fn merge_restored_items_front_inserts_in_order_ahead_of_live() {
        let _lock = lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, tmp.path().to_str().unwrap()) };
        let _env_guard = EnvGuard;

        run_async(async {
            let provider = ProviderKind::Claude;
            let token_hash = "merge-restored-order";
            let channel_id = ChannelId::new(3864002);
            let registry = ChannelMailboxRegistry::default();
            let handle = registry.handle(channel_id);
            let persistence = QueuePersistenceContext::new(&provider, token_hash, None);

            handle
                .enqueue(
                    make_intervention(300, "live", Instant::now()),
                    persistence.clone(),
                )
                .await;
            let result = handle
                .merge_restored_queue_items(
                    vec![
                        make_intervention(100, "restored-older", Instant::now()),
                        make_intervention(200, "restored-newer", Instant::now()),
                    ],
                    persistence.clone(),
                )
                .await;
            assert_eq!(result.absorbed, 2);
            let ids: Vec<u64> = handle
                .snapshot()
                .await
                .intervention_queue
                .iter()
                .map(|i| i.message_id.get())
                .collect();
            assert_eq!(
                ids,
                vec![100, 200, 300],
                "restored items keep order and sit ahead of the live item"
            );
        });
    }

    /// #3864 thorough dedup: a restored item whose ids are fully covered by a
    /// live queued item's `source_message_ids` is skipped. The old
    /// `message_id`-only dedup would re-add it (its `message_id` is NOT a live
    /// head `message_id`, only a live SOURCE id), creating a duplicate.
    #[test]
    fn merge_restored_items_skips_overlapping_source_ids() {
        let _lock = lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, tmp.path().to_str().unwrap()) };
        let _env_guard = EnvGuard;

        run_async(async {
            let provider = ProviderKind::Claude;
            let token_hash = "merge-restored-dedup";
            let channel_id = ChannelId::new(3864003);
            let registry = ChannelMailboxRegistry::default();
            let handle = registry.handle(channel_id);
            let persistence = QueuePersistenceContext::new(&provider, token_hash, None);

            // Live queue holds a MERGED item: head message_id 300, source {300, 301}.
            handle
                .replace_queue(
                    vec![make_intervention_with_sources(
                        300,
                        &[300, 301],
                        "live-merged",
                        Instant::now(),
                    )],
                    persistence.clone(),
                )
                .await;

            // Restored item carries head message_id 301 (a live SOURCE id, not a
            // live head id) with source {301} — fully covered by the live item.
            let result = handle
                .merge_restored_queue_items(
                    vec![make_intervention_with_sources(
                        301,
                        &[301],
                        "restored-duplicate",
                        Instant::now(),
                    )],
                    persistence.clone(),
                )
                .await;
            assert_eq!(
                result.absorbed, 0,
                "restored item fully covered by a live item's source ids must be skipped"
            );
            let queue = handle.snapshot().await.intervention_queue;
            assert_eq!(queue.len(), 1, "no duplicate must be inserted");
            assert_eq!(queue[0].message_id.get(), 300);
        });
    }

    /// #3864 persist-failure rollback: when the merge's durable persist fails,
    /// the actor rolls the in-memory queue back. The live enqueue survives (it
    /// was persisted by its own `Enqueue` and lives in the rolled-back-to
    /// previous queue), and the failure is surfaced via `persistence_error`
    /// instead of being silently dropped.
    #[cfg(unix)]
    #[test]
    fn merge_restored_items_persist_failure_rolls_back_and_keeps_live() {
        use std::os::unix::fs::PermissionsExt;
        let _lock = lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, tmp.path().to_str().unwrap()) };
        let _env_guard = EnvGuard;

        run_async(async {
            let provider = ProviderKind::Claude;
            let token_hash = "merge-restored-persist-fail";
            let channel_id = ChannelId::new(3864004);
            let registry = ChannelMailboxRegistry::default();
            let handle = registry.handle(channel_id);
            let persistence = QueuePersistenceContext::new(&provider, token_hash, None);

            // Live message B persists successfully (dir writable).
            let live = handle
                .enqueue(
                    make_intervention(200, "live", Instant::now()),
                    persistence.clone(),
                )
                .await;
            assert!(live.enqueued);
            let path = queue_file_path(tmp.path(), &provider, token_hash, channel_id);
            assert!(path.exists());
            let dir = path.parent().unwrap().to_path_buf();

            // Make the channel's persistence dir read-only so the merge's atomic
            // write (tmp create + rename) fails.
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
            let result = handle
                .merge_restored_queue_items(
                    vec![make_intervention(100, "restored", Instant::now())],
                    persistence.clone(),
                )
                .await;
            // Restore perms before any assertion can early-return (and before drop).
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();

            assert!(
                result.persistence_error.is_some(),
                "merge persist failure must be surfaced"
            );
            assert_eq!(result.absorbed, 0, "rolled back → nothing absorbed");

            // In memory: rolled back to just the live B (restored A dropped).
            let ids: Vec<u64> = handle
                .snapshot()
                .await
                .intervention_queue
                .iter()
                .map(|i| i.message_id.get())
                .collect();
            assert_eq!(ids, vec![200], "rollback keeps the live enqueue");

            // On disk: still the live B only (atomic write never clobbered it).
            let (disk, _override) = load_channel_pending_queue(&provider, token_hash, channel_id);
            let disk_ids: Vec<u64> = disk.iter().map(|i| i.message_id.get()).collect();
            assert_eq!(disk_ids, vec![200], "live enqueue stays durably persisted");
        });
    }

    #[tokio::test]
    async fn cancel_active_turn_if_current_ignores_stale_watchdog_token() {
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(ChannelId::new(46));
        let active_token = Arc::new(CancelToken::new());
        let stale_token = Arc::new(CancelToken::new());

        handle
            .try_start_turn(active_token.clone(), UserId::new(9), MessageId::new(91))
            .await;

        let stale = handle.cancel_active_turn_if_current(stale_token).await;
        assert!(stale.token.is_none());
        assert!(!stale.already_stopping);
        assert!(
            !active_token
                .cancelled
                .load(std::sync::atomic::Ordering::Relaxed)
        );

        let current = handle
            .cancel_active_turn_if_current(active_token.clone())
            .await;
        assert!(current.token.is_some());
        assert!(!current.already_stopping);
        assert!(
            active_token
                .cancelled
                .load(std::sync::atomic::Ordering::Relaxed)
        );
    }

    #[test]
    fn cleanup_stale_pending_queue_tmp_files_removes_only_stale_tmp_artifacts() {
        let tmp = tempfile::tempdir().unwrap();
        let provider = ProviderKind::Claude;
        let token_hash = "tmp-cleanup-direct";
        let stale_tmp_a = tmp.path().join(".12345.json.interrupted.tmp");
        let stale_tmp_b = tmp.path().join(".23456.json.interrupted.tmp");
        let queue_json = tmp.path().join("34567.json");
        std::fs::write(&stale_tmp_a, b"partial").unwrap();
        std::fs::write(&stale_tmp_b, b"partial").unwrap();
        std::fs::write(&queue_json, b"[]").unwrap();

        let audits = cleanup_stale_pending_queue_tmp_files_in_dir(
            &provider,
            token_hash,
            tmp.path(),
            SystemTime::now() + Duration::from_secs(120),
            Duration::from_secs(60),
        );

        assert_eq!(audits.len(), 2);
        assert!(
            audits.iter().any(|audit| {
                audit.channel_id == Some(12345) && audit.action == "removed_stale"
            })
        );
        assert!(
            audits.iter().any(|audit| {
                audit.channel_id == Some(23456) && audit.action == "removed_stale"
            })
        );
        assert!(!stale_tmp_a.exists());
        assert!(!stale_tmp_b.exists());
        assert!(
            queue_json.exists(),
            "cleanup must not touch real queue files"
        );
    }

    #[test]
    fn cleanup_stale_pending_queue_tmp_files_preserves_active_tmp_writes() {
        let tmp = tempfile::tempdir().unwrap();
        let provider = ProviderKind::Claude;
        let token_hash = "tmp-cleanup-active";
        let active_tmp = tmp.path().join(".45678.json.inflight.tmp");
        std::fs::write(&active_tmp, b"partial").unwrap();

        let audits = cleanup_stale_pending_queue_tmp_files_in_dir(
            &provider,
            token_hash,
            tmp.path(),
            SystemTime::now(),
            Duration::from_secs(60),
        );

        assert_eq!(audits.len(), 1);
        assert_eq!(audits[0].channel_id, Some(45678));
        assert_eq!(audits[0].action, "preserved_active");
        assert!(active_tmp.exists(), "fresh tmp writes must be preserved");
    }

    #[test]
    fn cleanup_stale_pending_queue_tmp_files_under_root_scans_all_token_dirs() {
        let root = tempfile::tempdir().unwrap();
        let claude_token_dir = root.path().join("claude").join("token-a");
        let codex_token_dir = root.path().join("codex").join("token-b");
        std::fs::create_dir_all(&claude_token_dir).unwrap();
        std::fs::create_dir_all(&codex_token_dir).unwrap();

        let stale_tmp = claude_token_dir.join(".11111.json.interrupted.tmp");
        let stale_tmp_other_provider = codex_token_dir.join(".22222.json.inflight.tmp");
        let queue_json = claude_token_dir.join("33333.json");
        let out_of_scope_tmp = root.path().join(".44444.json.interrupted.tmp");
        std::fs::write(&stale_tmp, b"partial").unwrap();
        std::fs::write(&stale_tmp_other_provider, b"partial").unwrap();
        std::fs::write(&queue_json, b"[]").unwrap();
        std::fs::write(&out_of_scope_tmp, b"partial").unwrap();

        let audits = cleanup_stale_pending_queue_tmp_files_under_root(
            root.path(),
            SystemTime::now() + Duration::from_secs(120),
            Duration::from_secs(60),
        );

        assert_eq!(audits.len(), 2);
        assert!(
            audits.iter().any(|audit| {
                audit.channel_id == Some(11111) && audit.action == "removed_stale"
            }),
            "stale tmp files in token directories should be removed"
        );
        assert!(
            audits.iter().any(|audit| {
                audit.channel_id == Some(22222) && audit.action == "removed_stale"
            }),
            "old tmp files for every provider/token should be checked"
        );
        assert!(!stale_tmp.exists());
        assert!(!stale_tmp_other_provider.exists());
        assert!(queue_json.exists(), "real queue files must be preserved");
        assert!(
            out_of_scope_tmp.exists(),
            "root-level tmp files are not pending queue token snapshots"
        );
    }

    /// #2374 — the mailbox actor must own the reason-write so that the
    /// reason and the `cancelled` flip happen as one serialized
    /// transition per channel. Verifies: after a single
    /// `cancel_active_turn_with_reason` round trip, the returned token
    /// is cancelled AND carries the supplied label.
    #[tokio::test]
    async fn cancel_active_turn_with_reason_writes_label_and_flips_atomically() {
        let channel_id = ChannelId::new(2374001);
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(channel_id);

        let cancel_token = Arc::new(CancelToken::new());
        let started = handle
            .try_start_turn(cancel_token.clone(), UserId::new(1), MessageId::new(11))
            .await;
        assert!(started, "fresh channel must accept the new turn");

        let result = handle
            .cancel_active_turn_with_reason("voice_foreground_cancel_during_handoff".to_string())
            .await;

        let returned = result.token.expect("cancel returned the active token");
        assert!(
            returned.cancelled.load(Ordering::Relaxed),
            "actor must flip `cancelled` as part of the reason-owned transition"
        );
        assert_eq!(
            returned.cancel_source().as_deref(),
            Some("voice_foreground_cancel_during_handoff"),
            "actor must write the reason label inside the same actor step \
             (not from the caller task)"
        );
        assert!(
            !result.already_stopping,
            "first cancel must not report already_stopping"
        );
    }

    /// #2374 — two concurrent cancellers must not trample each other's
    /// reason. The first cancel wins both the flip and the label; a
    /// second cancel observing `already_stopping=true` must NOT
    /// overwrite the recorded reason. Without actor ownership of the
    /// reason write, the caller-side `set_cancel_source` from the second
    /// canceller could race with the first canceller's write between
    /// the "is it already cancelled?" read and the actual store.
    #[tokio::test]
    async fn concurrent_cancels_do_not_trample_each_others_reason() {
        let channel_id = ChannelId::new(2374002);
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(channel_id);

        let cancel_token = Arc::new(CancelToken::new());
        handle
            .try_start_turn(cancel_token.clone(), UserId::new(1), MessageId::new(22))
            .await;

        // Fire two concurrent cancel attempts with different reasons.
        // Whichever the actor dequeues first must win the attribution;
        // the loser must observe `already_stopping=true` AND find the
        // recorded reason unchanged.
        let handle_a = handle.clone();
        let handle_b = handle.clone();
        let task_a = tokio::spawn(async move {
            handle_a
                .cancel_active_turn_with_reason("voice_barge_in_live_cut".to_string())
                .await
        });
        let task_b = tokio::spawn(async move {
            handle_b
                .cancel_active_turn_with_reason("watchdog_timeout".to_string())
                .await
        });
        let res_a = task_a.await.expect("task a panicked");
        let res_b = task_b.await.expect("task b panicked");

        // Exactly one of the two cancellers must observe
        // `already_stopping=false` (the winner). The other must observe
        // `already_stopping=true` (the loser).
        let winner_count = [&res_a, &res_b]
            .iter()
            .filter(|r| !r.already_stopping)
            .count();
        assert_eq!(
            winner_count, 1,
            "exactly one canceller can win the actor's serialized flip"
        );

        // The winner's reason must be the one persisted. Since the
        // actor is the sole writer, the winner's label is whichever
        // task the actor dequeued first; the loser's later message
        // must NOT mutate the label.
        let winner_label = if !res_a.already_stopping {
            "voice_barge_in_live_cut"
        } else {
            "watchdog_timeout"
        };
        assert_eq!(
            cancel_token.cancel_source().as_deref(),
            Some(winner_label),
            "loser's reason must NOT overwrite the winner's (actor-owned write)"
        );
        assert!(
            cancel_token.cancelled.load(Ordering::Relaxed),
            "token must be cancelled after either cancel returns"
        );
    }

    /// #2374 — `cancel_active_turn_if_current_with_reason` keeps the
    /// stale-caller guard. A token that no longer matches the active
    /// turn must NOT flip `cancelled` on the live turn nor write a
    /// reason.
    #[tokio::test]
    async fn cancel_if_current_with_reason_rejects_stale_token() {
        let channel_id = ChannelId::new(2374003);
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(channel_id);

        let stale_token = Arc::new(CancelToken::new());
        let live_token = Arc::new(CancelToken::new());
        handle
            .try_start_turn(live_token.clone(), UserId::new(1), MessageId::new(33))
            .await;

        let result = handle
            .cancel_active_turn_if_current_with_reason(
                stale_token.clone(),
                "stale_caller_reason".to_string(),
            )
            .await;

        assert!(
            result.token.is_none(),
            "stale `if_current` caller must not match the live turn"
        );
        assert!(
            !live_token.cancelled.load(Ordering::Relaxed),
            "live turn must NOT be cancelled by a stale caller"
        );
        assert!(
            live_token.cancel_source().is_none(),
            "live turn must NOT carry the stale caller's reason"
        );
    }

    /// #2374 Codex round-1 fix (HIGH-1) —
    /// `cancel_active_turn_if_user_message_with_reason` MUST cancel
    /// only when the active turn's `user_message_id` matches.
    #[tokio::test]
    async fn cancel_if_user_message_matches_cancels_with_reason() {
        let channel_id = ChannelId::new(2374004);
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(channel_id);

        let live_token = Arc::new(CancelToken::new());
        let handoff_msg = MessageId::new(987_654);
        handle
            .try_start_turn(live_token.clone(), UserId::new(1), handoff_msg)
            .await;

        let result = handle
            .cancel_active_turn_if_user_message_with_reason(
                handoff_msg,
                "voice_foreground_cancel_during_handoff".to_string(),
            )
            .await;

        assert!(
            result.token.is_some(),
            "matching user_message_id must cancel the active turn"
        );
        assert!(live_token.cancelled.load(Ordering::Relaxed));
        assert_eq!(
            live_token.cancel_source().as_deref(),
            Some("voice_foreground_cancel_during_handoff"),
        );
    }

    /// #2374 Codex round-1 fix (HIGH-1) — identity-guarded cancel MUST
    /// NOT touch the live turn when the active `user_message_id`
    /// belongs to a DIFFERENT message id than the caller's expected
    /// handoff id. This is the exact scenario the original PR missed:
    /// a tombstone retry arriving after the original handoff turn
    /// finalized and an unrelated turn started on the same target
    /// channel.
    #[tokio::test]
    async fn cancel_if_user_message_rejects_unrelated_active_turn() {
        let channel_id = ChannelId::new(2374005);
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(channel_id);

        // Active turn is an UNRELATED message (e.g. the original
        // handoff turn finalized and a new turn started).
        let live_token = Arc::new(CancelToken::new());
        let unrelated_msg = MessageId::new(111_111);
        let handoff_msg = MessageId::new(999_999);
        handle
            .try_start_turn(live_token.clone(), UserId::new(1), unrelated_msg)
            .await;

        let result = handle
            .cancel_active_turn_if_user_message_with_reason(
                handoff_msg,
                "voice_foreground_cancel_during_handoff".to_string(),
            )
            .await;

        assert!(
            result.token.is_none(),
            "identity-guarded cancel must NOT match an unrelated active turn"
        );
        assert!(
            !live_token.cancelled.load(Ordering::Relaxed),
            "unrelated active turn must NOT be cancelled by a tombstone retry"
        );
        assert!(
            live_token.cancel_source().is_none(),
            "unrelated active turn must NOT carry the handoff reason"
        );
    }

    /// #2374 Codex round-1 fix (HIGH-1) — identity-guarded cancel
    /// returns `None` when no active turn exists. This is the
    /// "handoff turn already finalized" case: the tombstone retry
    /// must observe no live token AND not affect any future turn.
    #[tokio::test]
    async fn cancel_if_user_message_returns_none_when_no_active_turn() {
        let channel_id = ChannelId::new(2374006);
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(channel_id);

        let result = handle
            .cancel_active_turn_if_user_message_with_reason(
                MessageId::new(42),
                "voice_foreground_cancel_during_handoff".to_string(),
            )
            .await;

        assert!(
            result.token.is_none(),
            "no-active-turn case must return None — no work to cancel"
        );
        assert!(
            !result.already_stopping,
            "no-active-turn case must not report already_stopping"
        );
    }
}

// #3167 — the active-turn priority class lets the external-input dequeue treat
// a low-priority background relay (monitor terminal-output relay / self-paced
// TUI loop) as non-blocking, so a queued external USER intervention is not
// starved behind a continuously-cycling background turn.
#[cfg(test)]
mod active_turn_kind_tests {
    use super::test_support::{AGENTDESK_ROOT_DIR_ENV, lock_test_env};
    use super::*;

    // #3167 BLOCKER-3 — serialize every test in this module that mutates the
    // process-global `AGENTDESK_ROOT_DIR` env (the durable-queue persistence
    // root) via the SINGLE crate-wide `test_support::TEST_ENV_LOCK` shared by
    // ALL env-touching test modules in this file (per-module locks do not
    // serialize cross-module). A RAII `EnvGuard` removes the var on drop.
    // Without this, `background_start_yields_to_queued_backlog` (and the new
    // reservation tests) clobbered the var under the default parallel
    // `cargo test --lib`, contaminating other modules' tests → spurious failures.
    struct EnvGuard;
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe { std::env::remove_var(AGENTDESK_ROOT_DIR_ENV) };
        }
    }

    #[tokio::test]
    async fn background_turn_is_active_but_not_blocking() {
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(ChannelId::new(3_167_001));

        assert!(
            handle
                .try_start_turn_kinded(
                    Arc::new(CancelToken::new()),
                    UserId::new(1),
                    MessageId::new(11),
                    ActiveTurnKind::Background,
                )
                .await
        );

        assert!(
            handle.has_active_turn().await,
            "a background turn still holds the slot for `has_active_turn`"
        );
        assert!(
            !handle.has_blocking_active_turn().await,
            "#3167: a background turn must NOT block a queued user intervention"
        );
        assert_eq!(
            handle.active_turn_kind().await,
            Some(ActiveTurnKind::Background),
        );
    }

    #[tokio::test]
    async fn user_turn_is_blocking() {
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(ChannelId::new(3_167_002));

        assert!(
            handle
                .try_start_turn(
                    Arc::new(CancelToken::new()),
                    UserId::new(2),
                    MessageId::new(22),
                )
                .await
        );

        assert!(handle.has_active_turn().await);
        assert!(
            handle.has_blocking_active_turn().await,
            "a real user/agent turn must block the dequeue"
        );
        assert_eq!(
            handle.active_turn_kind().await,
            Some(ActiveTurnKind::UserOrAgent),
        );
    }

    #[tokio::test]
    async fn finalize_clears_kind() {
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(ChannelId::new(3_167_003));

        assert!(
            handle
                .try_start_turn_kinded(
                    Arc::new(CancelToken::new()),
                    UserId::new(3),
                    MessageId::new(33),
                    ActiveTurnKind::Background,
                )
                .await
        );
        assert_eq!(
            handle.active_turn_kind().await,
            Some(ActiveTurnKind::Background),
        );

        let _ = handle.hard_stop().await;

        assert!(!handle.has_active_turn().await);
        assert_eq!(
            handle.active_turn_kind().await,
            None,
            "#3167: finalize must clear the priority class with the anchor"
        );

        // A fresh default turn after a background finalize is UserOrAgent, not
        // a leaked Background.
        assert!(
            handle
                .try_start_turn(
                    Arc::new(CancelToken::new()),
                    UserId::new(3),
                    MessageId::new(34),
                )
                .await
        );
        assert_eq!(
            handle.active_turn_kind().await,
            Some(ActiveTurnKind::UserOrAgent),
            "the kind must not leak from the previous background turn"
        );
    }

    #[tokio::test]
    async fn restore_preserves_kind() {
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(ChannelId::new(3_167_004));

        handle
            .restore_active_turn_kinded(
                Arc::new(CancelToken::new()),
                UserId::new(4),
                MessageId::new(44),
                ActiveTurnKind::Background,
            )
            .await;

        assert!(handle.has_active_turn().await);
        assert!(
            !handle.has_blocking_active_turn().await,
            "#3167: restore must preserve the background classification"
        );
        assert_eq!(
            handle.active_turn_kind().await,
            Some(ActiveTurnKind::Background),
        );
    }

    fn test_intervention(message_id: u64) -> Intervention {
        Intervention {
            author_id: UserId::new(1),
            author_is_bot: false,
            message_id: MessageId::new(message_id),
            source_message_ids: vec![MessageId::new(message_id)],
            text: format!("msg-{message_id}"),
            mode: InterventionMode::Soft,
            created_at: Instant::now(),
            reply_context: None,
            has_reply_boundary: false,
            merge_consecutive: false,
            pending_uploads: Vec::new(),
            voice_announcement: None,
        }
    }

    fn test_persistence() -> QueuePersistenceContext {
        QueuePersistenceContext::new(&ProviderKind::Claude, "background-supersede-test", None)
    }

    // #3167 BLOCKER-1 — the atomic, kind-guarded supersede cancels ONLY a
    // background turn. A real user/agent turn (or an idle slot) is never
    // cancelled, which is what closes the TOCTOU window: a stale supersede that
    // arrives after the background turn finalized and a real user turn started
    // must NOT abort that real turn.
    #[tokio::test]
    async fn cancel_active_background_turn_if_current_cancels_only_background() {
        let registry = ChannelMailboxRegistry::default();

        // (1) Background turn → cancelled, returns true, reason recorded.
        let bg = registry.handle(ChannelId::new(3_167_101));
        let bg_token = Arc::new(CancelToken::new());
        assert!(
            bg.try_start_turn_kinded(
                bg_token.clone(),
                UserId::new(1),
                MessageId::new(11),
                ActiveTurnKind::Background,
            )
            .await
        );
        assert!(
            bg.cancel_active_background_turn_if_current().await,
            "a background turn holding the slot must be cancelled (returns true)"
        );
        assert!(
            bg_token.cancelled.load(Ordering::Relaxed),
            "the background turn's token must be flipped cancelled"
        );
        assert_eq!(
            bg_token.cancel_source().as_deref(),
            Some("idle_queue_user_supersede_background"),
            "the supersede reason must be recorded in the same actor step"
        );

        // (2) Real user/agent turn → NEVER cancelled, returns false (no-op).
        let user = registry.handle(ChannelId::new(3_167_102));
        let user_token = Arc::new(CancelToken::new());
        assert!(
            user.try_start_turn(user_token.clone(), UserId::new(2), MessageId::new(22))
                .await
        );
        assert!(
            !user.cancel_active_background_turn_if_current().await,
            "a real user/agent turn must NOT be cancelled by a stale supersede (returns false)"
        );
        assert!(
            !user_token.cancelled.load(Ordering::Relaxed),
            "the real turn's token must remain un-cancelled — this is the TOCTOU fix"
        );
        assert!(
            user.has_active_turn().await,
            "the real turn must still hold the slot"
        );

        // (3) Idle slot → no-op, returns false.
        let idle = registry.handle(ChannelId::new(3_167_103));
        assert!(
            !idle.cancel_active_background_turn_if_current().await,
            "an idle slot is a no-op (returns false)"
        );
    }

    // #3167 BLOCKER-2 — a Background start yields to a queued backlog. Once a
    // user/dispatch intervention is queued, no new Background turn may
    // re-acquire the freed slot ahead of it (starvation/livelock fix). A
    // UserOrAgent start is unaffected by queue contents.
    //
    // SAFETY (await_holding_lock): `TEST_ENV_LOCK` is held across awaits to
    // serialize tests that mutate the process-global `AGENTDESK_ROOT_DIR`;
    // releasing before the awaits would race concurrent tests. Test-only.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn background_start_yields_to_queued_backlog() {
        // `enqueue` durably persists the queue; point the persistence root at a
        // throwaway tempdir so the enqueue succeeds deterministically (the real
        // home dir / a stale tempdir leaked by another test would make this
        // flaky). #3167 BLOCKER-3: serialize the env mutation under the parallel
        // default `cargo test --lib` (NOT --test-threads=1).
        let _lock = lock_test_env();
        let _env_guard = EnvGuard;
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, tmp.path().to_str().unwrap()) };

        let registry = ChannelMailboxRegistry::default();

        // Empty queue → Background start acquires the slot (returns true).
        let empty = registry.handle(ChannelId::new(3_167_201));
        assert!(
            empty
                .try_start_turn_kinded(
                    Arc::new(CancelToken::new()),
                    UserId::new(1),
                    MessageId::new(11),
                    ActiveTurnKind::Background,
                )
                .await,
            "with an empty queue a Background turn may start"
        );

        // Non-empty queue → Background start REFUSES (returns false, no slot).
        let backlog = registry.handle(ChannelId::new(3_167_202));
        let enqueued = backlog
            .enqueue(test_intervention(101), test_persistence())
            .await;
        assert!(enqueued.enqueued, "fixture intervention must enqueue");
        assert!(
            !backlog
                .try_start_turn_kinded(
                    Arc::new(CancelToken::new()),
                    UserId::new(2),
                    MessageId::new(22),
                    ActiveTurnKind::Background,
                )
                .await,
            "a Background turn must NOT acquire the slot ahead of a queued backlog"
        );
        assert!(
            !backlog.has_active_turn().await,
            "the slot must stay free so the kickoff can drain the queued user"
        );

        // UserOrAgent start is UNAFFECTED by a queued backlog.
        let user = registry.handle(ChannelId::new(3_167_203));
        let enqueued = user
            .enqueue(test_intervention(201), test_persistence())
            .await;
        assert!(enqueued.enqueued);
        assert!(
            user.try_start_turn(
                Arc::new(CancelToken::new()),
                UserId::new(3),
                MessageId::new(33)
            )
            .await,
            "a real user/agent turn must still start even with a queued backlog"
        );
        assert!(user.has_active_turn().await);
        // `EnvGuard` removes `AGENTDESK_ROOT_DIR` on drop.
    }

    // #3167 BLOCKER-1 — a SECOND supersede against an already-cancelling
    // background slot is a no-op and returns `false`. This is what stops the
    // caller's immediate re-kick from hot-looping while the background finalizer
    // drains the slot.
    #[tokio::test]
    async fn cancel_active_background_turn_if_current_second_call_is_noop_false() {
        let registry = ChannelMailboxRegistry::default();
        let bg = registry.handle(ChannelId::new(3_167_301));
        let bg_token = Arc::new(CancelToken::new());
        assert!(
            bg.try_start_turn_kinded(
                bg_token.clone(),
                UserId::new(1),
                MessageId::new(11),
                ActiveTurnKind::Background,
            )
            .await
        );

        // First supersede performs the NEW cancel → true.
        assert!(
            bg.cancel_active_background_turn_if_current().await,
            "first supersede performs a NEW cancel and returns true"
        );
        assert!(bg_token.cancelled.load(Ordering::Relaxed));

        // The slot is still held by the (now cancelling) background turn — its
        // identity-guarded finalizer has not released it yet. A second supersede
        // must be a NO-OP and return false so the caller spawns NO new re-kick.
        assert!(
            !bg.cancel_active_background_turn_if_current().await,
            "#3167 BLOCKER-1: an already-cancelling background slot returns false (no hot-loop)"
        );
        // And a third, to prove it stays false (no livelock cadence).
        assert!(
            !bg.cancel_active_background_turn_if_current().await,
            "repeated supersede of an already-cancelling slot stays false"
        );
    }

    // #3167 BLOCKER-2 — the dequeue→claim window. `TakeNextSoft` removes the
    // queued head BEFORE the dequeued user turn claims the slot, leaving an
    // EMPTY queue. A Background start arriving in that window must still yield
    // because the `pending_user_dispatch` reservation is live.
    //
    // SAFETY (await_holding_lock): see `background_start_yields_to_queued_backlog`.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn background_yields_during_dequeue_to_claim_window() {
        let _lock = lock_test_env();
        let _env_guard = EnvGuard;
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, tmp.path().to_str().unwrap()) };

        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(ChannelId::new(3_167_401));

        // Queue one user intervention, then dequeue it for dispatch. After the
        // dequeue the queue is EMPTY but the reservation is set.
        assert!(
            handle
                .enqueue(test_intervention(101), test_persistence())
                .await
                .enqueued
        );
        let taken = handle.take_next_soft(test_persistence()).await;
        assert!(
            taken.intervention.is_some(),
            "the queued head must be dequeued for dispatch"
        );
        assert_eq!(
            taken.queue_len_after, 0,
            "the queue is EMPTY after the dequeue — only the reservation guards the window"
        );

        // A Background start in this window must YIELD even though the queue is
        // empty (this is the BLOCKER-2 starvation fix).
        assert!(
            !handle
                .try_start_turn_kinded(
                    Arc::new(CancelToken::new()),
                    UserId::new(2),
                    MessageId::new(22),
                    ActiveTurnKind::Background,
                )
                .await,
            "Background must yield during the dequeue→claim window (reservation held, queue empty)"
        );
        assert!(
            !handle.has_active_turn().await,
            "the slot must stay free so the dequeued user can claim it"
        );

        // The reserved user turn now claims the slot → reservation cleared.
        assert!(
            handle
                .try_start_turn(
                    Arc::new(CancelToken::new()),
                    UserId::new(2),
                    MessageId::new(101)
                )
                .await,
            "the dequeued UserOrAgent turn claims the slot"
        );
        // Release and prove the reservation is GONE: a Background start with an
        // empty queue now succeeds.
        let _ = handle.hard_stop().await;
        assert!(
            handle
                .try_start_turn_kinded(
                    Arc::new(CancelToken::new()),
                    UserId::new(3),
                    MessageId::new(33),
                    ActiveTurnKind::Background,
                )
                .await,
            "after the user claim cleared the reservation, Background may start again"
        );
    }

    // #3167 BLOCKER-2 SAFETY VALVE — if the dequeued user turn is lost (never
    // claims, never requeues), the reservation must not lock Background out
    // forever. After PENDING_USER_DISPATCH_MAX_YIELDS consecutive
    // reservation-only refusals, the reservation is force-cleared.
    //
    // SAFETY (await_holding_lock): see `background_start_yields_to_queued_backlog`.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn safety_valve_clears_stuck_reservation_after_n_refusals() {
        let _lock = lock_test_env();
        let _env_guard = EnvGuard;
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, tmp.path().to_str().unwrap()) };

        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(ChannelId::new(3_167_402));

        // Set a reservation, then NEVER claim/requeue it (simulate a lost turn).
        assert!(
            handle
                .enqueue(test_intervention(201), test_persistence())
                .await
                .enqueued
        );
        let taken = handle.take_next_soft(test_persistence()).await;
        assert!(taken.intervention.is_some());
        assert_eq!(taken.queue_len_after, 0);

        // The first N refusals all yield (queue empty, reservation held).
        for attempt in 1..=PENDING_USER_DISPATCH_MAX_YIELDS {
            assert!(
                !handle
                    .try_start_turn_kinded(
                        Arc::new(CancelToken::new()),
                        UserId::new(9),
                        MessageId::new(900 + attempt as u64),
                        ActiveTurnKind::Background,
                    )
                    .await,
                "refusal {attempt}/{PENDING_USER_DISPATCH_MAX_YIELDS} must still yield"
            );
            assert!(!handle.has_active_turn().await);
        }

        // The Nth refusal force-cleared the (stuck) reservation. The NEXT
        // Background start succeeds — proving the valve is non-permanent.
        assert!(
            handle
                .try_start_turn_kinded(
                    Arc::new(CancelToken::new()),
                    UserId::new(9),
                    MessageId::new(999),
                    ActiveTurnKind::Background,
                )
                .await,
            "after N reservation-only refusals the safety valve clears the reservation"
        );
        assert!(handle.has_active_turn().await);
    }

    // #3167 BLOCKER-2 — a failed dispatch requeues the reserved head; that
    // clears the reservation (the now non-empty queue covers the Background
    // gate) and resets the valve counter.
    //
    // SAFETY (await_holding_lock): see `background_start_yields_to_queued_backlog`.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn requeue_of_reserved_head_clears_reservation() {
        let _lock = lock_test_env();
        let _env_guard = EnvGuard;
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, tmp.path().to_str().unwrap()) };

        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(ChannelId::new(3_167_403));

        let intervention = test_intervention(301);
        assert!(
            handle
                .enqueue(intervention.clone(), test_persistence())
                .await
                .enqueued
        );
        let taken = handle.take_next_soft(test_persistence()).await;
        assert!(taken.intervention.is_some());
        assert_eq!(taken.queue_len_after, 0);

        // Dispatch failed → requeue the reserved head. The reservation is now
        // cleared, but the queue is non-empty so Background still yields.
        handle.requeue_front(intervention, test_persistence()).await;
        assert!(
            !handle
                .try_start_turn_kinded(
                    Arc::new(CancelToken::new()),
                    UserId::new(4),
                    MessageId::new(44),
                    ActiveTurnKind::Background,
                )
                .await,
            "Background still yields — now because the queue is non-empty, not the reservation"
        );
        assert!(!handle.has_active_turn().await);
    }

    // #3903 — a genuine user message queued behind a `/loop`/system-injection
    // turn must NOT be lost. The live incident: a queued user reply lost the
    // start-turn race to a `/loop` auto-check (a Background turn), so it was
    // re-enqueued behind the injection. The race-loss drain-scheduling guard
    // (`race_loss.rs`) keyed on `has_active_turn` (ANY turn) and therefore
    // skipped scheduling the deferred drain while the Background injection held
    // the slot — and the injection's own finalize never re-kicks the user
    // queue, so the message stranded until an external fetch surfaced it.
    //
    // This test pins the two invariants the fix relies on:
    //   1. the scheduling DISCRIMINATOR — a Background injection makes
    //      `has_active_turn()` true (the old guard skips → bug) but
    //      `has_blocking_active_turn()` false (the new guard schedules → fix);
    //   2. the END-TO-END outcome — once the injection turn completes, the
    //      queued user message is dequeued exactly once (not lost, not doubled).
    //
    // #3034: hold the test-env lock across a SYNCHRONOUS `block_on` (not across
    // an `.await` inside an async fn) so the global `AGENTDESK_ROOT_DIR` stays
    // stable for the durable-queue persistence WITHOUT an
    // `#[allow(clippy::await_holding_lock)]` site (matches the `run_async`
    // pattern in `actor_hydrate_regression_tests`).
    #[test]
    fn queued_user_message_survives_loop_injection_preemption() {
        let _lock = lock_test_env();
        let _env_guard = EnvGuard;
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, tmp.path().to_str().unwrap()) };

        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let registry = ChannelMailboxRegistry::default();
                let handle = registry.handle(ChannelId::new(3_903_001));

                // A `/loop` auto-check is injected and claims the slot as a
                // Background turn (mirrors `synthetic_start.rs`
                // `try_start_turn_kinded(Background)`).
                let loop_token = Arc::new(CancelToken::new());
                assert!(
                    handle
                        .try_start_turn_kinded(
                            loop_token.clone(),
                            UserId::new(1),
                            MessageId::new(7_001),
                            ActiveTurnKind::Background,
                        )
                        .await,
                    "the /loop injection claims the idle slot as a Background turn"
                );

                // The genuine user reply lost the start-turn race and is queued
                // behind the injection.
                let user_msg = test_intervention(7_100);
                assert!(
                    handle
                        .enqueue(user_msg.clone(), test_persistence())
                        .await
                        .enqueued,
                    "the genuine user message is queued behind the injection"
                );

                // Invariant 1 — the scheduling discriminator. The OLD race-loss
                // guard (`!has_active_turn`) would be FALSE here and skip the
                // drain (the #3903 bug); the NEW guard
                // (`!has_blocking_active_turn`) is TRUE and schedules it.
                assert!(
                    handle.has_active_turn().await,
                    "the Background injection holds the slot for has_active_turn — old guard skipped the drain"
                );
                assert!(
                    !handle.has_blocking_active_turn().await,
                    "#3903: a Background injection is non-blocking, so the new guard schedules the rescue drain"
                );

                // The deferred drain supersedes the non-blocking injection
                // (`#3167` `cancel_active_background_turn_if_current`) and the
                // injection's finalizer releases the slot.
                assert!(
                    handle.cancel_active_background_turn_if_current().await,
                    "the drain cancels ONLY the Background injection to free the slot for the user"
                );
                let finish = handle.finish_turn(test_persistence()).await;
                assert!(
                    finish.has_pending,
                    "the queued user message is still pending after the injection finalizes"
                );
                assert!(!handle.has_active_turn().await, "the slot is now free");

                // Invariant 2 — exactly-once delivery. The drain dequeues the
                // queued user message and the dispatched user turn claims the
                // slot.
                let taken = handle.take_next_soft(test_persistence()).await;
                let dequeued = taken.intervention.expect(
                    "the queued user message must be dequeued after the injection completes",
                );
                assert_eq!(
                    dequeued.message_id,
                    MessageId::new(7_100),
                    "the genuine user message is the one delivered — not lost"
                );
                assert_eq!(
                    taken.queue_len_after, 0,
                    "no duplicate copy is left in the queue"
                );
                assert!(
                    handle
                        .try_start_turn(
                            Arc::new(CancelToken::new()),
                            UserId::new(2),
                            MessageId::new(7_100),
                        )
                        .await,
                    "the dispatched user turn claims the slot and clears the dequeue reservation"
                );

                // Not doubled — after the user turn finishes there is nothing
                // left to re-deliver.
                let finish = handle.finish_turn(test_persistence()).await;
                assert!(
                    !finish.has_pending,
                    "the user message was delivered exactly once — the queue is drained"
                );
                let drained = handle.take_next_soft(test_persistence()).await;
                assert!(
                    drained.intervention.is_none(),
                    "a second dequeue yields nothing — no double-processing"
                );
            });
    }
}

// #2728 — verify the refusal_reason field correctly tags each of the
// three false-return paths in `enqueue_intervention` / the handle layer.
// Without this signal callers could only infer the path from code
// archaeology (cf. the adk-cc 07:27 KST 2026-05-20 incident).
#[cfg(test)]
mod enqueue_refusal_reason_tests {
    use super::*;

    fn intervention(message_id: u64, text: &str, created_at: Instant) -> Intervention {
        Intervention {
            author_id: UserId::new(1),
            author_is_bot: false,
            message_id: MessageId::new(message_id),
            source_message_ids: vec![MessageId::new(message_id)],
            text: text.to_string(),
            mode: InterventionMode::Soft,
            created_at,
            reply_context: None,
            has_reply_boundary: false,
            merge_consecutive: false,
            pending_uploads: Vec::new(),
            voice_announcement: None,
        }
    }

    #[test]
    fn source_id_already_queued_is_tagged() {
        let now = Instant::now();
        let mut queue = vec![intervention(1, "hello", now)];
        let incoming = intervention(1, "hello again", now);
        let result = enqueue_intervention(&mut queue, incoming);
        assert!(!result.enqueued);
        assert_eq!(
            result.refusal_reason,
            Some(EnqueueRefusalReason::SourceIdAlreadyQueued),
        );
    }

    #[test]
    fn last_item_dedup_is_tagged() {
        let now = Instant::now();
        let mut queue = vec![intervention(1, "same text", now)];
        let incoming = intervention(2, "same text", now);
        let result = enqueue_intervention(&mut queue, incoming);
        assert!(!result.enqueued);
        assert_eq!(
            result.refusal_reason,
            Some(EnqueueRefusalReason::LastItemDedup),
        );
    }

    #[test]
    fn upload_bearing_interventions_are_not_deduped_by_empty_text() {
        let now = Instant::now();
        let mut first = intervention(1, "", now);
        first.pending_uploads =
            vec!["[File uploaded] one.png → /tmp/one.png (1 bytes)".to_string()];
        let mut second = intervention(2, "", now);
        second.pending_uploads =
            vec!["[File uploaded] two.png → /tmp/two.png (2 bytes)".to_string()];
        let mut queue = vec![first];

        let result = enqueue_intervention(&mut queue, second);

        assert!(result.enqueued);
        assert_eq!(result.refusal_reason, None);
        assert_eq!(queue.len(), 2);
    }

    #[test]
    fn refusal_reason_absent_on_success() {
        let now = Instant::now();
        let mut queue: Vec<Intervention> = Vec::new();
        let incoming = intervention(1, "first", now);
        let result = enqueue_intervention(&mut queue, incoming);
        assert!(result.enqueued);
        assert_eq!(result.refusal_reason, None);
    }
}

// #3177: queued user messages must never be age-evicted. The old
// `prune_interventions_at` dropped anything older than `INTERVENTION_TTL`
// (10 min) as `QueueExitKind::Expired`, silently losing user input when a turn
// stayed busy. These tests pin the new behaviour: arbitrarily old items survive
// prune, and only the MAX_INTERVENTIONS_PER_CHANNEL overflow cap still trims the
// queue (as `Superseded`).
#[cfg(test)]
mod no_ttl_evict_tests {
    use super::*;

    fn intervention_at(message_id: u64, created_at: Instant) -> Intervention {
        Intervention {
            author_id: UserId::new(1),
            author_is_bot: false,
            message_id: MessageId::new(message_id),
            source_message_ids: vec![MessageId::new(message_id)],
            text: format!("msg-{message_id}"),
            mode: InterventionMode::Soft,
            created_at,
            reply_context: None,
            has_reply_boundary: false,
            merge_consecutive: false,
            pending_uploads: Vec::new(),
            voice_announcement: None,
        }
    }

    #[test]
    fn very_old_intervention_survives_prune() {
        let now = Instant::now();
        // Far past the old 10-minute TTL.
        let ancient = now
            .checked_sub(Duration::from_secs(60 * 60))
            .expect("test clock should subtract an hour");
        let mut queue = vec![intervention_at(1, ancient)];

        let exits = prune_interventions_at(&mut queue, now);

        assert_eq!(
            queue.len(),
            1,
            "an hour-old intervention must remain queued (no age eviction)"
        );
        assert_eq!(queue[0].message_id, MessageId::new(1));
        assert!(
            exits.is_empty(),
            "no QueueExitEvent should be produced for an old-but-under-cap queue"
        );
        // The soft-queue probe must also keep it.
        assert!(has_soft_intervention_at(&mut queue, now));
        assert_eq!(queue.len(), 1);
    }

    #[test]
    fn overflow_cap_still_supersedes_oldest() {
        let now = Instant::now();
        let mut queue: Vec<Intervention> = (0..(MAX_INTERVENTIONS_PER_CHANNEL as u64 + 3))
            .map(|i| intervention_at(i + 1, now))
            .collect();

        let exits = prune_interventions_at(&mut queue, now);

        assert_eq!(
            queue.len(),
            MAX_INTERVENTIONS_PER_CHANNEL,
            "overflow cap must bound the queue"
        );
        assert_eq!(exits.len(), 3, "the 3 oldest must be evicted");
        assert!(
            exits.iter().all(|e| e.kind == QueueExitKind::Superseded),
            "overflow eviction must be Superseded, never Expired"
        );
        // The evicted ones are the oldest (lowest message ids).
        assert_eq!(exits[0].intervention.message_id, MessageId::new(1));
        assert_eq!(exits[2].intervention.message_id, MessageId::new(3));
    }
}

#[cfg(test)]
mod persistence_tests {
    use super::test_support::lock_test_env;
    use super::*;
    use std::path::{Path, PathBuf};

    const AGENTDESK_ROOT_DIR_ENV: &str = "AGENTDESK_ROOT_DIR";

    struct EnvGuard {
        previous: Option<String>,
    }

    impl EnvGuard {
        fn set_root(root: &Path) -> Self {
            let previous = std::env::var(AGENTDESK_ROOT_DIR_ENV).ok();
            unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, root.to_str().unwrap()) };
            Self { previous }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            if let Some(previous) = self.previous.as_ref() {
                unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, previous) };
            } else {
                unsafe { std::env::remove_var(AGENTDESK_ROOT_DIR_ENV) };
            }
        }
    }

    fn queue_file_path(
        root: &Path,
        provider: &ProviderKind,
        token_hash: &str,
        channel_id: ChannelId,
    ) -> PathBuf {
        root.join("runtime")
            .join("discord_pending_queue")
            .join(provider.as_str())
            .join(token_hash)
            .join(format!("{}.json", channel_id.get()))
    }

    fn marker_file_path(
        root: &Path,
        provider: &ProviderKind,
        token_hash: &str,
        channel_id: ChannelId,
    ) -> PathBuf {
        root.join("runtime")
            .join("discord_pending_queue")
            .join(provider.as_str())
            .join(token_hash)
            .join(format!("{}.dispatch", channel_id.get()))
    }

    fn read_saved_items(
        root: &Path,
        provider: &ProviderKind,
        token_hash: &str,
        channel_id: ChannelId,
    ) -> Vec<PendingQueueItem> {
        let path = queue_file_path(root, provider, token_hash, channel_id);
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    fn voice_announcement(
        transcript: &str,
        utterance_id: &str,
    ) -> crate::voice::prompt::VoiceTranscriptAnnouncement {
        crate::voice::prompt::VoiceTranscriptAnnouncement {
            transcript: transcript.to_string(),
            user_id: "42".to_string(),
            utterance_id: utterance_id.to_string(),
            language: "ko-KR".to_string(),
            verbose_progress: true,
            started_at: Some("2026-05-24T21:00:00+09:00".to_string()),
            completed_at: Some("2026-05-24T21:00:01+09:00".to_string()),
            samples_written: Some(48_000),
            control_channel_id: Some(300),
            stt_mode: Some("file".to_string()),
            stt_latency_ms: Some(120),
        }
    }

    fn make_intervention(
        message_id: u64,
        text: &str,
        voice_announcement: Option<crate::voice::prompt::VoiceTranscriptAnnouncement>,
    ) -> Intervention {
        Intervention {
            author_id: UserId::new(100),
            author_is_bot: voice_announcement.is_some(),
            message_id: MessageId::new(message_id),
            source_message_ids: vec![MessageId::new(message_id)],
            text: text.to_string(),
            mode: InterventionMode::Soft,
            created_at: Instant::now(),
            reply_context: None,
            has_reply_boundary: false,
            merge_consecutive: false,
            pending_uploads: Vec::new(),
            voice_announcement,
        }
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn take_next_soft_writes_pending_dispatch_marker_with_head() {
        let _lock = lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        let _env_guard = EnvGuard::set_root(tmp.path());

        let provider = ProviderKind::Claude;
        let token_hash = "dispatch-marker-take";
        let channel_id = ChannelId::new(4_024_210);
        let persistence = QueuePersistenceContext::new(&provider, token_hash, None);
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(channel_id);
        let head = make_intervention(4_024_211, "head", None);
        handle
            .replace_queue(vec![head.clone()], persistence.clone())
            .await;

        let taken = handle.take_next_soft(persistence).await;

        assert_eq!(
            taken.intervention.as_ref().map(|item| item.message_id),
            Some(head.message_id)
        );
        let marker_path = marker_file_path(tmp.path(), &provider, token_hash, channel_id);
        let marker: PendingQueueItem =
            serde_json::from_str(&std::fs::read_to_string(marker_path).unwrap()).unwrap();
        assert_eq!(marker.message_id, head.message_id.get());
        assert_eq!(marker.text, "head");
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn requeue_front_restores_head_and_clears_pending_dispatch_marker() {
        let _lock = lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        let _env_guard = EnvGuard::set_root(tmp.path());

        let provider = ProviderKind::Claude;
        let token_hash = "dispatch-marker-requeue";
        let channel_id = ChannelId::new(4_024_220);
        let persistence = QueuePersistenceContext::new(&provider, token_hash, None);
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(channel_id);
        let head = make_intervention(4_024_221, "head", None);
        handle
            .replace_queue(vec![head.clone()], persistence.clone())
            .await;
        let taken = handle.take_next_soft(persistence.clone()).await;
        let intervention = taken.intervention.expect("head should be dequeued");
        let marker_path = marker_file_path(tmp.path(), &provider, token_hash, channel_id);
        assert!(marker_path.exists());

        let requeue = handle.requeue_front(intervention, persistence).await;

        assert!(requeue.persistence_error.is_none());
        assert!(
            !marker_path.exists(),
            "successful requeue-front consumes the pending dispatch marker"
        );
        let saved = read_saved_items(tmp.path(), &provider, token_hash, channel_id);
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].message_id, head.message_id.get());
    }

    #[test]
    fn boot_recovery_restores_marker_into_empty_queue_front() {
        let _lock = lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        let _env_guard = EnvGuard::set_root(tmp.path());

        let provider = ProviderKind::Claude;
        let token_hash = "dispatch-marker-boot-empty";
        let channel_id = ChannelId::new(4_024_230);
        let marker = make_intervention(4_024_231, "marker head", None);
        save_channel_pending_dispatch_marker(&provider, token_hash, channel_id, &marker, None)
            .unwrap();

        let (loaded, _) = load_channel_pending_queue(&provider, token_hash, channel_id);

        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].message_id, marker.message_id);
        assert!(
            !marker_file_path(tmp.path(), &provider, token_hash, channel_id).exists(),
            "boot recovery deletes marker after queue persist succeeds"
        );
        let saved = read_saved_items(tmp.path(), &provider, token_hash, channel_id);
        assert_eq!(saved[0].message_id, marker.message_id.get());
    }

    #[test]
    fn boot_recovery_drops_marker_when_identity_already_queued() {
        let _lock = lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        let _env_guard = EnvGuard::set_root(tmp.path());

        let provider = ProviderKind::Claude;
        let token_hash = "dispatch-marker-boot-duplicate";
        let channel_id = ChannelId::new(4_024_240);
        let queued = make_intervention(4_024_241, "queued", None);
        save_channel_queue(
            &provider,
            token_hash,
            channel_id,
            std::slice::from_ref(&queued),
            None,
        )
        .unwrap();
        save_channel_pending_dispatch_marker(&provider, token_hash, channel_id, &queued, None)
            .unwrap();

        let (loaded, _) = load_channel_pending_queue(&provider, token_hash, channel_id);

        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].message_id, queued.message_id);
        assert!(
            !marker_file_path(tmp.path(), &provider, token_hash, channel_id).exists(),
            "duplicate marker must be dropped instead of duplicating the queue"
        );
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn take_next_soft_persist_failure_restores_queue_and_deletes_marker() {
        let _lock = lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        let _env_guard = EnvGuard::set_root(tmp.path());

        let provider = ProviderKind::Claude;
        let token_hash = "dispatch-marker-persist-fail";
        let channel_id = ChannelId::new(4_024_250);
        let persistence = QueuePersistenceContext::new(&provider, token_hash, None);
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(channel_id);
        let head = make_intervention(4_024_251, "head", None);
        handle
            .replace_queue(vec![head.clone()], persistence.clone())
            .await;

        let queue_path = queue_file_path(tmp.path(), &provider, token_hash, channel_id);
        std::fs::remove_file(&queue_path).unwrap();
        std::fs::create_dir(&queue_path).unwrap();

        let taken = handle.take_next_soft(persistence).await;

        assert!(taken.intervention.is_none());
        assert!(taken.persistence_error.is_some());
        assert_eq!(handle.snapshot().await.intervention_queue.len(), 1);
        assert_eq!(
            handle.snapshot().await.intervention_queue[0].message_id,
            head.message_id
        );
        assert!(
            !marker_file_path(tmp.path(), &provider, token_hash, channel_id).exists(),
            "marker must be deleted when queue-without-head persistence fails"
        );
    }

    // SAFETY (await_holding_lock): `TEST_ENV_LOCK` serializes env-mutating tests
    // and must stay held across the awaits to prevent concurrent env clobbering.
    // Test-only.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn enqueue_rolls_back_when_pending_queue_persistence_fails() {
        let _lock = lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        let _env_guard = EnvGuard::set_root(tmp.path());
        std::fs::write(tmp.path().join("runtime"), "not-a-directory").unwrap();

        let provider = ProviderKind::Codex;
        let token_hash = "unwritable-pending-queue";
        let channel_id = ChannelId::new(2_867_001);
        let persistence = QueuePersistenceContext::new(&provider, token_hash, None);
        let direct_error = save_channel_queue(
            &provider,
            token_hash,
            channel_id,
            &[make_intervention(2_867_002, "must persist", None)],
            None,
        )
        .expect_err("direct pending queue write must surface persistence failure");
        assert!(
            direct_error.contains("create_dir_all") || direct_error.contains("Not a directory")
        );

        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(channel_id);
        let result = handle
            .enqueue(
                make_intervention(2_867_003, "must not be accepted without disk", None),
                persistence,
            )
            .await;

        assert!(!result.enqueued);
        assert_eq!(result.refusal_reason, None);
        assert!(result.persistence_error.is_some());
        let snapshot = handle.snapshot().await;
        assert!(
            snapshot.intervention_queue.is_empty(),
            "mailbox must roll back non-durable queued work"
        );
    }

    #[test]
    fn pending_queue_roundtrip_preserves_author_is_bot() {
        let _lock = lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        let _env_guard = EnvGuard::set_root(tmp.path());

        let provider = ProviderKind::Codex;
        let token_hash = "author_bot_roundtrip";
        let channel_id = ChannelId::new(4242);
        let message_id = MessageId::new(9001);
        let intervention = Intervention {
            author_id: UserId::new(100),
            author_is_bot: true,
            message_id,
            source_message_ids: vec![message_id],
            text: "DISPATCH: restore me".to_string(),
            mode: InterventionMode::Soft,
            created_at: Instant::now(),
            reply_context: None,
            has_reply_boundary: false,
            merge_consecutive: false,
            pending_uploads: Vec::new(),
            voice_announcement: None,
        };

        save_channel_queue(&provider, token_hash, channel_id, &[intervention], None).unwrap();

        let path = tmp
            .path()
            .join("runtime")
            .join("discord_pending_queue")
            .join(provider.as_str())
            .join(token_hash)
            .join(format!("{}.json", channel_id.get()));
        let saved: Vec<PendingQueueItem> =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert!(saved[0].author_is_bot);

        let (loaded, _) = load_pending_queues(&provider, token_hash);
        assert!(loaded[&channel_id][0].author_is_bot);
    }

    #[test]
    fn pending_queue_roundtrip_preserves_voice_announcement_payload() {
        let _lock = lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        let _env_guard = EnvGuard::set_root(tmp.path());

        let provider = ProviderKind::Codex;
        let token_hash = "voice_announcement_roundtrip";
        let channel_id = ChannelId::new(2_777_001);
        let announcement =
            voice_announcement("큐에 들어간 음성 요청 처리해줘", "issue-2777-roundtrip");
        let intervention = make_intervention(
            2_777_002,
            "ADK_VOICE_TRANSCRIPT v1\n큐에 들어간 음성 요청 처리해줘",
            Some(announcement.clone()),
        );

        save_channel_queue(
            &provider,
            token_hash,
            channel_id,
            std::slice::from_ref(&intervention),
            None,
        )
        .unwrap();

        let saved = read_saved_items(tmp.path(), &provider, token_hash, channel_id);
        assert_eq!(saved[0].voice_announcement.as_ref(), Some(&announcement));

        let (loaded, _) = load_pending_queues(&provider, token_hash);
        assert_eq!(
            loaded[&channel_id][0].voice_announcement.as_ref(),
            Some(&announcement),
            "post-restart disk load must not depend on the in-memory announcement TTL"
        );
    }

    #[test]
    fn pending_queue_roundtrip_preserves_upload_context() {
        let _lock = lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        let _env_guard = EnvGuard::set_root(tmp.path());

        let provider = ProviderKind::Codex;
        let token_hash = "upload_context_roundtrip";
        let channel_id = ChannelId::new(2_840_001);
        let mut intervention = make_intervention(2_840_002, "", None);
        intervention.pending_uploads = vec![
            "[File uploaded] report.pdf → /runtime/discord_uploads/1/report.pdf (123 bytes)"
                .to_string(),
        ];

        save_channel_queue(
            &provider,
            token_hash,
            channel_id,
            std::slice::from_ref(&intervention),
            None,
        )
        .unwrap();

        let saved = read_saved_items(tmp.path(), &provider, token_hash, channel_id);
        assert_eq!(saved[0].pending_uploads, intervention.pending_uploads);

        let (loaded, _) = load_pending_queues(&provider, token_hash);
        assert_eq!(
            loaded[&channel_id][0].pending_uploads, intervention.pending_uploads,
            "queued attachment-only turns must carry their own upload context"
        );
    }

    // SAFETY (await_holding_lock): `TEST_ENV_LOCK` serializes env-mutating tests
    // and must stay held across the awaits. Test-only.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn actor_hydrate_from_disk_preserves_voice_announcement_payload() {
        let _lock = lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        let _env_guard = EnvGuard::set_root(tmp.path());

        let provider = ProviderKind::Claude;
        let token_hash = "voice_announcement_actor_hydrate";
        let channel_id = ChannelId::new(2_777_011);
        let announcement = voice_announcement(
            "재시작 후 hydrate 된 음성 요청 처리해줘",
            "issue-2777-hydrate",
        );
        let intervention = make_intervention(
            2_777_012,
            "ADK_VOICE_TRANSCRIPT v1\n재시작 후 hydrate 된 음성 요청 처리해줘",
            Some(announcement.clone()),
        );
        save_channel_queue(
            &provider,
            token_hash,
            channel_id,
            std::slice::from_ref(&intervention),
            None,
        )
        .unwrap();

        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(channel_id);
        let result = handle
            .hydrate_pending_queue_from_disk(QueuePersistenceContext::new(
                &provider, token_hash, None,
            ))
            .await;

        assert_eq!(result.absorbed, 1);
        assert_eq!(result.queue_len_after, 1);
        let snapshot = handle.snapshot().await;
        assert_eq!(
            snapshot.intervention_queue[0].voice_announcement.as_ref(),
            Some(&announcement)
        );
    }

    // SAFETY (await_holding_lock): `TEST_ENV_LOCK` serializes env-mutating tests
    // and must stay held across the awaits. Test-only.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn restart_drain_persists_voice_announcement_payload() {
        let _lock = lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        let _env_guard = EnvGuard::set_root(tmp.path());

        let provider = ProviderKind::Claude;
        let token_hash = "voice_announcement_restart_drain";
        let channel_id = ChannelId::new(2_777_021);
        let announcement = voice_announcement(
            "restart drain 중인 음성 요청 처리해줘",
            "issue-2777-restart-drain",
        );
        let intervention = make_intervention(
            2_777_022,
            "ADK_VOICE_TRANSCRIPT v1\nrestart drain 중인 음성 요청 처리해줘",
            Some(announcement.clone()),
        );
        let persistence = QueuePersistenceContext::new(&provider, token_hash, None);
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(channel_id);
        handle
            .replace_queue(vec![intervention], persistence.clone())
            .await;

        let path = queue_file_path(tmp.path(), &provider, token_hash, channel_id);
        std::fs::remove_file(&path).unwrap();
        let result = handle.restart_drain(persistence).await;

        assert_eq!(result.queued_count, 1);
        let saved = read_saved_items(tmp.path(), &provider, token_hash, channel_id);
        assert_eq!(saved[0].voice_announcement.as_ref(), Some(&announcement));
    }

    // SAFETY (await_holding_lock): `TEST_ENV_LOCK` serializes env-mutating tests
    // and must stay held across the awaits. Test-only.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn restart_drain_all_reports_pending_queue_persistence_errors() {
        let _lock = lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        let _env_guard = EnvGuard::set_root(tmp.path());

        let provider = ProviderKind::Claude;
        let token_hash = "mailbox-restart-drain-failure";
        let channel_id = ChannelId::new(143);
        let registry = ChannelMailboxRegistry::default();

        registry
            .handle(channel_id)
            .replace_queue(
                vec![make_intervention(1, "queued item", None)],
                QueuePersistenceContext::new(&provider, token_hash, None),
            )
            .await;

        std::fs::remove_dir_all(tmp.path().join("runtime")).unwrap();
        std::fs::write(tmp.path().join("runtime"), "not-a-directory").unwrap();

        let drain = registry
            .restart_drain_all(&provider, token_hash, &dashmap::DashMap::new())
            .await;

        assert_eq!(drain.queued_count, 0);
        assert_eq!(drain.persistence_errors.len(), 1);
        assert_eq!(drain.persistence_errors[0].channel_id, channel_id);
        assert!(
            drain.persistence_errors[0].error.contains("create_dir_all")
                || drain.persistence_errors[0]
                    .error
                    .contains("Not a directory")
        );
    }
}

// #2706: PurgeQueue regression guards. Kept in a plain `#[cfg(test)]` module so
// they run under the default `cargo test` invocation. The older SQLite-only
// mailbox harness was removed, so queue-only purge coverage must live in the
// normal test build.
#[cfg(test)]
mod purge_queue_tests {
    use std::sync::Arc;
    use std::time::Instant;

    use poise::serenity_prelude::{ChannelId, MessageId, UserId};

    use crate::services::provider::ProviderKind;
    use crate::services::turn_orchestrator::test_support::lock_test_env;
    use crate::services::turn_orchestrator::{
        CancelToken, ChannelMailboxRegistry, Intervention, InterventionMode,
        QueuePersistenceContext,
    };

    fn make_intervention(message_id: u64, text: &str, created_at: Instant) -> Intervention {
        Intervention {
            author_id: UserId::new(1),
            author_is_bot: false,
            message_id: MessageId::new(message_id),
            source_message_ids: vec![MessageId::new(message_id)],
            text: text.to_string(),
            mode: InterventionMode::Soft,
            created_at,
            reply_context: None,
            has_reply_boundary: false,
            merge_consecutive: false,
            pending_uploads: Vec::new(),
            voice_announcement: None,
        }
    }

    // PurgeQueue empties the intervention queue without touching the
    // active cancel_token, so a turn that entered the mailbox between
    // force-kill and the purge survives.
    //
    // #3167 BLOCKER-3 — this test PERSISTS to (and reads back from) the default
    // `AGENTDESK_ROOT_DIR`; hold the shared env lock so a concurrent
    // env-mutating test cannot redirect the persistence root mid-run (the
    // `drained == 3` assertion was the observed flake). SAFETY
    // (await_holding_lock): the lock must stay held across the awaits to
    // serialize against env mutators. Test-only.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn purge_queue_drains_queue_without_disturbing_active_turn() {
        let _env_lock = lock_test_env();
        let provider = ProviderKind::Claude;
        let registry = ChannelMailboxRegistry::default();
        let channel_id = ChannelId::new(2706);
        let handle = registry.handle(channel_id);
        let persistence = QueuePersistenceContext::new(&provider, "mailbox-purge-2706", None);
        let now = Instant::now();

        handle
            .replace_queue(
                vec![
                    make_intervention(20, "first", now),
                    make_intervention(21, "second", now),
                    make_intervention(22, "third", now),
                ],
                persistence.clone(),
            )
            .await;

        let active_token = Arc::new(CancelToken::new());
        handle
            .restore_active_turn(active_token.clone(), UserId::new(7), MessageId::new(70))
            .await;

        let purge = handle.purge_queue(persistence, false).await;
        assert_eq!(purge.drained, 3);
        assert!(!purge.cleared_active_anchor);

        let snapshot = handle.snapshot().await;
        assert!(snapshot.intervention_queue.is_empty());

        // Active turn (its token and ownership) must survive the queue purge.
        let surviving = handle.cancel_token().await;
        assert!(surviving.is_some());
        assert!(Arc::ptr_eq(&surviving.unwrap(), &active_token));
    }

    // purge_queue is a no-op on an empty mailbox.
    // #3167 BLOCKER-3: shares the env lock (persists to the default root).
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn purge_queue_is_idempotent_on_empty_mailbox() {
        let _env_lock = lock_test_env();
        let provider = ProviderKind::Claude;
        let registry = ChannelMailboxRegistry::default();
        let channel_id = ChannelId::new(2707);
        let handle = registry.handle(channel_id);
        let persistence =
            QueuePersistenceContext::new(&provider, "mailbox-purge-idempotent-2706", None);

        let drained_first = handle.purge_queue(persistence.clone(), false).await;
        let drained_second = handle.purge_queue(persistence, false).await;
        assert_eq!(drained_first.drained, 0);
        assert_eq!(drained_second.drained, 0);
        assert!(handle.snapshot().await.intervention_queue.is_empty());
    }

    // #3029(D): a force purge (clear_cancelled_active_anchor=true) against an
    // already-cancelled active turn releases the anchor so the next dispatch
    // is not blocked by a stale cancel_token / active_user_message_id.
    // #3167 BLOCKER-3: shares the env lock (persists to the default root).
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn force_purge_clears_cancelled_active_anchor() {
        let _env_lock = lock_test_env();
        let provider = ProviderKind::Claude;
        let registry = ChannelMailboxRegistry::default();
        let channel_id = ChannelId::new(30290);
        let handle = registry.handle(channel_id);
        let persistence = QueuePersistenceContext::new(&provider, "mailbox-force-purge-3029", None);

        let active_token = Arc::new(CancelToken::new());
        handle
            .restore_active_turn(active_token.clone(), UserId::new(7), MessageId::new(70))
            .await;
        // The force path flips `cancelled` (via cancel_active_token) before
        // purging; emulate that here.
        active_token
            .cancelled
            .store(true, std::sync::atomic::Ordering::Relaxed);

        let purge = handle.purge_queue(persistence, true).await;
        assert!(
            purge.cleared_active_anchor,
            "force purge must release a cancelled active-turn anchor (#3029 D)"
        );
        assert!(
            handle.cancel_token().await.is_none(),
            "cancelled active anchor must be cleared after force purge"
        );
    }

    // #3029(D) / #2706: a force purge must NOT clear the anchor of a fresh,
    // *uncancelled* turn that raced into the actor after the force-kill —
    // otherwise force=true would collaterally cancel the new turn.
    // #3167 BLOCKER-3: shares the env lock (persists to the default root).
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn force_purge_preserves_uncancelled_active_anchor() {
        let _env_lock = lock_test_env();
        let provider = ProviderKind::Claude;
        let registry = ChannelMailboxRegistry::default();
        let channel_id = ChannelId::new(30291);
        let handle = registry.handle(channel_id);
        let persistence =
            QueuePersistenceContext::new(&provider, "mailbox-force-purge-fresh-3029", None);

        let fresh_token = Arc::new(CancelToken::new());
        handle
            .restore_active_turn(fresh_token.clone(), UserId::new(7), MessageId::new(71))
            .await;
        // Token is NOT cancelled — represents a fresh turn that raced in.

        let purge = handle.purge_queue(persistence, true).await;
        assert!(
            !purge.cleared_active_anchor,
            "uncancelled fresh turn must keep its anchor (#2706 no-collateral-cancel)"
        );
        let surviving = handle.cancel_token().await;
        assert!(surviving.is_some());
        assert!(Arc::ptr_eq(&surviving.unwrap(), &fresh_token));
    }
}

#[cfg(test)]
mod finish_cancelled_turn_tests {
    use std::sync::Arc;
    use std::time::Instant;

    use poise::serenity_prelude::{ChannelId, MessageId, UserId};

    use crate::services::provider::ProviderKind;
    use crate::services::turn_orchestrator::test_support::TEST_ENV_LOCK;
    use crate::services::turn_orchestrator::{
        CancelToken, ChannelMailboxRegistry, Intervention, InterventionMode,
        QueuePersistenceContext, save_channel_queue,
    };

    const AGENTDESK_ROOT_DIR_ENV: &str = "AGENTDESK_ROOT_DIR";

    fn make_intervention(message_id: u64, text: &str) -> Intervention {
        Intervention {
            author_id: UserId::new(1),
            author_is_bot: false,
            message_id: MessageId::new(message_id),
            source_message_ids: vec![MessageId::new(message_id)],
            text: text.to_string(),
            mode: InterventionMode::Soft,
            created_at: Instant::now(),
            reply_context: None,
            has_reply_boundary: false,
            merge_consecutive: false,
            pending_uploads: Vec::new(),
            voice_announcement: None,
        }
    }

    // SAFETY (await_holding_lock): `TEST_ENV_LOCK` serializes env-mutating tests
    // and must stay held across the awaits. Test-only.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn finish_cancelled_turn_clears_cancelled_active_without_rehydrating_queue() {
        let _lock = match TEST_ENV_LOCK.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, tmp.path().to_str().unwrap()) };

        let provider = ProviderKind::Codex;
        let token_hash = "finish-cancelled-no-rehydrate";
        let channel_id = ChannelId::new(2_997_001);
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(channel_id);
        let persistence = QueuePersistenceContext::new(&provider, token_hash, None);

        handle.replace_queue(Vec::new(), persistence).await;
        save_channel_queue(
            &provider,
            token_hash,
            channel_id,
            &[make_intervention(30, "disk-only queued prompt")],
            None,
        )
        .expect("seed disk-only pending queue");

        let token = Arc::new(CancelToken::new());
        token
            .cancelled
            .store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(
            handle
                .try_start_turn(token.clone(), UserId::new(7), MessageId::new(70))
                .await
        );

        let finished = handle.finish_cancelled_turn().await;

        assert!(
            finished
                .removed_token
                .as_ref()
                .is_some_and(|removed| Arc::ptr_eq(removed, &token)),
            "removed_token tells recovery it may decrement global_active",
        );
        assert!(!finished.has_pending);
        assert!(finished.mailbox_online);
        let snapshot = handle.snapshot().await;
        assert!(snapshot.cancel_token.is_none());
        assert!(snapshot.active_user_message_id.is_none());
        assert!(
            snapshot.intervention_queue.is_empty(),
            "finish_cancelled_turn must not hydrate disk-only pending queues",
        );

        unsafe { std::env::remove_var(AGENTDESK_ROOT_DIR_ENV) };
    }

    #[tokio::test]
    async fn finish_cancelled_turn_preserves_uncancelled_active_turn() {
        let registry = ChannelMailboxRegistry::default();
        let channel_id = ChannelId::new(2_997_002);
        let handle = registry.handle(channel_id);
        let token = Arc::new(CancelToken::new());

        assert!(
            handle
                .try_start_turn(token.clone(), UserId::new(7), MessageId::new(71))
                .await
        );

        let finished = handle.finish_cancelled_turn().await;

        assert!(finished.removed_token.is_none());
        assert!(finished.mailbox_online);
        let snapshot = handle.snapshot().await;
        assert!(
            snapshot
                .cancel_token
                .as_ref()
                .is_some_and(|active| Arc::ptr_eq(active, &token)),
            "fresh active turn must survive a stale finish_cancelled_turn call",
        );
        assert_eq!(snapshot.active_user_message_id, Some(MessageId::new(71)));
    }

    #[tokio::test]
    async fn finish_cancelled_turn_is_noop_when_mailbox_is_idle() {
        let registry = ChannelMailboxRegistry::default();
        let channel_id = ChannelId::new(2_997_003);
        let handle = registry.handle(channel_id);

        let finished = handle.finish_cancelled_turn().await;

        assert!(finished.removed_token.is_none());
        assert!(finished.mailbox_online);
        let snapshot = handle.snapshot().await;
        assert!(snapshot.cancel_token.is_none());
        assert!(snapshot.active_user_message_id.is_none());
    }
}

#[cfg(test)]
mod recovery_done_signal_tests {
    use super::*;

    /// #2443 — verify the latch-then-wait race-free contract.
    #[tokio::test]
    async fn recovery_done_latch_short_circuits_late_subscribers() {
        let signal = RecoveryDoneSignal::new();
        signal.mark_done();
        // Subscriber registers AFTER mark_done — must still complete.
        tokio::time::timeout(std::time::Duration::from_millis(100), signal.wait())
            .await
            .expect("late subscriber should observe latched done state");
    }

    /// #2443 — verify the reset clears the latch so the next recovery
    /// cycle's watcher does not see a stale signal.
    #[tokio::test]
    async fn recovery_done_reset_unlatches_for_next_cycle() {
        let signal = std::sync::Arc::new(RecoveryDoneSignal::new());
        signal.mark_done();
        signal.reset();
        // After reset, wait should NOT short-circuit — must time out.
        let result =
            tokio::time::timeout(std::time::Duration::from_millis(50), signal.wait()).await;
        assert!(
            result.is_err(),
            "reset() should clear the latch so subsequent waits block until next mark_done"
        );
        // Now fire mark_done in a background task and confirm a fresh
        // waiter wakes up.
        let signal_for_task = signal.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            signal_for_task.mark_done();
        });
        tokio::time::timeout(std::time::Duration::from_millis(500), signal.wait())
            .await
            .expect("wait after reset should resolve when mark_done fires again");
    }

    /// #2443 — global resolution path used by watchers/lifecycle.rs.
    #[tokio::test]
    async fn registry_recovery_done_is_globally_resolvable() {
        let registry = ChannelMailboxRegistry::default();
        let channel_id = ChannelId::new(99_443);
        let signal = registry.recovery_done(channel_id);
        let resolved =
            ChannelMailboxRegistry::global_recovery_done(channel_id).expect("global signal");
        // Identity check via mark_done propagation: marking one wakes
        // the other if they point to the same underlying Arc.
        signal.mark_done();
        tokio::time::timeout(std::time::Duration::from_millis(50), resolved.wait())
            .await
            .expect("global_recovery_done should resolve to the same Arc");
    }
}
