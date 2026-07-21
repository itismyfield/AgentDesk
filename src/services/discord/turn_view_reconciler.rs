use std::{
    collections::HashMap,
    fs,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time,
};

use dashmap::DashMap;
use poise::serenity_prelude as serenity;
use serde::{Deserialize, Serialize};
use serenity::{ChannelId, MessageId};

use super::{SharedData, queue_reactions};

// #4278: descendant module owns orphan-`⏳` defense while retaining access to
// the reconciler's private target store and persistence helpers.
mod orphan_sweep;
// #4554: mailbox-truth repair is isolated to keep this giant module net-zero.
mod queue_repair;
mod reaction_set;
pub(in crate::services::discord) use orphan_sweep::sweep_orphan_tui_anchor_reactions;

const TURN_VIEW_REACTIONS: [char; 7] = ['📬', '➕', '🔄', '⏳', '✅', '⚠', '🛑'];
const QUEUE_EXIT_FEEDBACK_REACTIONS: [char; 3] = ['🚫', '⌛', '⏏'];
const PERSISTED_STATE_VERSION: u32 = 1;
const LEGACY_QUEUED_HOURGLASS_STATE_VERSION: u32 = 2;
const QUEUED_MARKER_ONLY_STATE_VERSION: u32 = 3;
const RECENTLY_FINALIZED_TARGET_MAX: usize = 1024;
const RECENTLY_FINALIZED_TARGET_TTL: time::Duration = time::Duration::from_secs(10 * 60);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::services::discord) enum TurnViewState {
    Queued,
    QueuedMerged,
    QueuedReconcile,
    Pending,
    Completed,
    Failed,
    Stopped,
    None,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::services::discord) enum TurnViewDelivery {
    Delivered,
    Failed,
    FailedPermanent,
}

impl TurnViewDelivery {
    fn delivered(self) -> bool {
        matches!(self, Self::Delivered)
    }

    fn merge(self, other: Self) -> Self {
        match (self, other) {
            (Self::FailedPermanent, _) | (_, Self::FailedPermanent) => Self::FailedPermanent,
            (Self::Failed, _) | (_, Self::Failed) => Self::Failed,
            _ => Self::Delivered,
        }
    }

    fn from_reaction_error_status(status: Option<u16>) -> Self {
        if status.is_some_and(super::placeholder_sweeper::is_permanent_message_gone_status) {
            Self::FailedPermanent
        } else {
            Self::Failed
        }
    }
}

impl TurnViewState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::QueuedMerged => "queued_merged",
            Self::QueuedReconcile => "queued_reconcile",
            Self::Pending => "pending",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Stopped => "stopped",
            Self::None => "none",
        }
    }

    fn from_str(value: &str) -> Option<Self> {
        match value {
            "queued" => Some(Self::Queued),
            "queued_merged" => Some(Self::QueuedMerged),
            "queued_reconcile" => Some(Self::QueuedReconcile),
            "pending" => Some(Self::Pending),
            "completed" => Some(Self::Completed),
            "failed" => Some(Self::Failed),
            "stopped" => Some(Self::Stopped),
            "none" => Some(Self::None),
            _ => None,
        }
    }

    fn terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Stopped)
    }

    fn is_queue_marker(self) -> bool {
        matches!(
            self,
            Self::Queued | Self::QueuedMerged | Self::QueuedReconcile
        )
    }

    fn from_queue_marker_emoji(emoji: char) -> Option<Self> {
        match emoji {
            queue_reactions::QUEUE_STANDALONE_PENDING_REACTION => Some(Self::Queued),
            queue_reactions::QUEUE_MERGED_PENDING_REACTION => Some(Self::QueuedMerged),
            queue_reactions::QUEUE_RECONCILE_PENDING_REACTION => Some(Self::QueuedReconcile),
            _ => None,
        }
    }

    fn started_or_terminal(self) -> bool {
        matches!(
            self,
            Self::Pending | Self::Completed | Self::Failed | Self::Stopped
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(in crate::services::discord) enum TurnViewTargetKind {
    IntakeUserMessage,
    TuiDirectBotAnchor,
}

impl TurnViewTargetKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::IntakeUserMessage => "intake_user_message",
            Self::TuiDirectBotAnchor => "tui_direct_bot_anchor",
        }
    }

    fn from_str(value: &str) -> Option<Self> {
        match value {
            "intake_user_message" => Some(Self::IntakeUserMessage),
            "tui_direct_bot_anchor" => Some(Self::TuiDirectBotAnchor),
            _ => None,
        }
    }

    fn identity_label(self) -> &'static str {
        match self {
            Self::IntakeUserMessage => "intake",
            Self::TuiDirectBotAnchor => "provider",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(in crate::services::discord) struct TurnViewTarget {
    pub(in crate::services::discord) kind: TurnViewTargetKind,
    pub(in crate::services::discord) channel_id: ChannelId,
    pub(in crate::services::discord) message_id: MessageId,
}

impl TurnViewTarget {
    pub(in crate::services::discord) fn intake_user_message(
        channel_id: ChannelId,
        message_id: MessageId,
    ) -> Self {
        Self {
            kind: TurnViewTargetKind::IntakeUserMessage,
            channel_id,
            message_id,
        }
    }

    pub(in crate::services::discord) fn tui_direct_bot_anchor(
        channel_id: ChannelId,
        message_id: MessageId,
    ) -> Self {
        Self {
            kind: TurnViewTargetKind::TuiDirectBotAnchor,
            channel_id,
            message_id,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::services::discord) struct TurnViewOwner {
    generation: u64,
    turn_id: String,
}

impl TurnViewOwner {
    pub(in crate::services::discord) fn new(generation: u64, turn_id: impl Into<String>) -> Self {
        Self {
            generation,
            turn_id: turn_id.into(),
        }
    }

    pub(in crate::services::discord) fn for_message(
        channel_id: ChannelId,
        message_id: MessageId,
        generation: u64,
    ) -> Self {
        Self::new(
            generation,
            format!("discord:{}:{}", channel_id.get(), message_id.get()),
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(in crate::services::discord) struct TurnStartAttempt(u64);

impl TurnStartAttempt {
    fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::services::discord) struct TurnViewStartRecord {
    delivered: bool,
    attempt: Option<TurnStartAttempt>,
}

impl TurnViewStartRecord {
    pub(in crate::services::discord) fn delivered(self) -> bool {
        self.delivered
    }

    pub(in crate::services::discord) fn attempt(self) -> Option<TurnStartAttempt> {
        self.attempt
    }
}

#[derive(Clone)]
pub(in crate::services::discord) enum TurnViewIdentity {
    IntakeHttp(Arc<serenity::http::Http>),
    IntakeShared,
    ProviderBot,
    #[cfg(test)]
    Test(&'static str),
}

#[derive(Clone)]
struct ResolvedIdentity {
    label: String,
    token_hash: Option<String>,
    #[cfg(not(test))]
    http: Arc<serenity::http::Http>,
}

#[derive(Clone)]
struct AppliedTarget {
    owner: TurnViewOwner,
    applied: TurnViewState,
    identity: ResolvedIdentity,
    start_attempt: Option<TurnStartAttempt>,
    legacy_queue_reactions: Vec<char>,
}

#[derive(Clone, Copy)]
struct RecentlyFinalizedTarget {
    generation: u64,
    recorded_at: time::Instant,
}

#[derive(Default)]
struct RecentlyFinalizedTargets {
    targets: HashMap<TurnViewTarget, RecentlyFinalizedTarget>,
}

impl RecentlyFinalizedTargets {
    fn blocks_queued(&mut self, target: TurnViewTarget, generation: u64) -> bool {
        self.prune(time::Instant::now());
        self.targets
            .get(&target)
            .is_some_and(|entry| generation <= entry.generation)
    }

    fn remember(&mut self, target: TurnViewTarget, generation: u64) {
        let now = time::Instant::now();
        self.prune(now);
        self.targets
            .entry(target)
            .and_modify(|entry| {
                if generation >= entry.generation {
                    entry.generation = generation;
                    entry.recorded_at = now;
                }
            })
            .or_insert(RecentlyFinalizedTarget {
                generation,
                recorded_at: now,
            });
        while self.targets.len() > RECENTLY_FINALIZED_TARGET_MAX {
            let Some(oldest) = self
                .targets
                .iter()
                .min_by_key(|(_, entry)| entry.recorded_at)
                .map(|(target, _)| *target)
            else {
                break;
            };
            self.targets.remove(&oldest);
        }
    }

    fn prune(&mut self, now: time::Instant) {
        self.targets.retain(|_, entry| {
            now.duration_since(entry.recorded_at) <= RECENTLY_FINALIZED_TARGET_TTL
        });
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct PersistedTargetState {
    version: u32,
    provider: String,
    kind: String,
    channel_id: u64,
    message_id: u64,
    owner_generation: u64,
    owner_turn_id: String,
    applied: String,
    identity_label: String,
    #[serde(default)]
    token_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    start_attempt_id: Option<u64>,
}

#[derive(Default)]
pub(in crate::services::discord) struct TurnViewReconciler {
    targets: DashMap<TurnViewTarget, AppliedTarget>,
    target_locks: std::sync::Mutex<HashMap<TurnViewTarget, Arc<tokio::sync::Mutex<()>>>>,
    next_start_attempt: AtomicU64,
    // Bounded in-memory only. A restart loses this guard, and durable queue
    // replay remains the source of truth for messages still queued on disk.
    recently_finalized: std::sync::Mutex<RecentlyFinalizedTargets>,
    #[cfg(test)]
    ops: Arc<std::sync::Mutex<Vec<TestReactionOp>>>,
    #[cfg(test)]
    test_deliveries: Arc<std::sync::Mutex<std::collections::VecDeque<TurnViewDelivery>>>,
}

impl TurnViewReconciler {
    pub(in crate::services::discord) async fn note_turn_started(
        &self,
        shared: &SharedData,
        target: TurnViewTarget,
        owner: TurnViewOwner,
        identity: TurnViewIdentity,
        source: &'static str,
    ) -> bool {
        self.note_turn_started_with_attempt(shared, target, owner, identity, source)
            .await
            .delivered()
    }

    pub(in crate::services::discord) async fn note_turn_started_with_attempt(
        &self,
        shared: &SharedData,
        target: TurnViewTarget,
        owner: TurnViewOwner,
        identity: TurnViewIdentity,
        source: &'static str,
    ) -> TurnViewStartRecord {
        let (delivery, attempt) = self
            .note_state_delivery_with_attempt(
                shared,
                target,
                owner,
                identity,
                TurnViewState::Pending,
                source,
            )
            .await;
        TurnViewStartRecord {
            delivered: delivery.delivered(),
            attempt,
        }
    }

    fn mint_start_attempt(&self) -> TurnStartAttempt {
        TurnStartAttempt(self.next_start_attempt.fetch_add(1, Ordering::Relaxed) + 1)
    }

    fn start_attempt_for(&self, desired: TurnViewState) -> Option<TurnStartAttempt> {
        (desired == TurnViewState::Pending).then(|| self.mint_start_attempt())
    }

    fn applied_target(
        owner: TurnViewOwner,
        applied: TurnViewState,
        identity: ResolvedIdentity,
        start_attempt: Option<TurnStartAttempt>,
    ) -> AppliedTarget {
        AppliedTarget {
            owner,
            applied,
            identity,
            start_attempt: (applied == TurnViewState::Pending)
                .then_some(start_attempt)
                .flatten(),
            legacy_queue_reactions: Vec::new(),
        }
    }

    fn update_matching_pending_attempt(
        &self,
        shared: &SharedData,
        target: TurnViewTarget,
        owner: TurnViewOwner,
        current: &AppliedTarget,
        start_attempt: Option<TurnStartAttempt>,
        source: &'static str,
    ) -> Option<TurnStartAttempt> {
        let start_attempt = start_attempt?;
        let updated = AppliedTarget {
            owner,
            applied: TurnViewState::Pending,
            identity: current.identity.clone(),
            start_attempt: Some(start_attempt),
            legacy_queue_reactions: current.legacy_queue_reactions.clone(),
        };
        self.targets.insert(target, updated.clone());
        self.persist_target(target, &updated, shared, source);
        Some(start_attempt)
    }

    pub(in crate::services::discord) async fn note_queue_marker_added(
        &self,
        shared: &SharedData,
        target: TurnViewTarget,
        owner: TurnViewOwner,
        identity: TurnViewIdentity,
        emoji: char,
        source: &'static str,
    ) -> bool {
        let Some(desired) = TurnViewState::from_queue_marker_emoji(emoji) else {
            tracing::warn!(
                channel_id = target.channel_id.get(),
                message = target.message_id.get(),
                target_kind = ?target.kind,
                emoji = %emoji,
                source,
                "turn view queue marker add ignored for unsupported emoji"
            );
            return false;
        };
        self.note_state(shared, target, owner, identity, desired, source)
            .await
    }

    pub(in crate::services::discord) async fn note_start_rolled_back_to_queued(
        &self,
        shared: &SharedData,
        target: TurnViewTarget,
        owner: TurnViewOwner,
        start_attempt: TurnStartAttempt,
        source: &'static str,
    ) -> bool {
        self.note_start_rolled_back_to_queued_delivery(shared, target, owner, start_attempt, source)
            .await
            .delivered()
    }

    pub(in crate::services::discord) async fn note_queue_marker_removed(
        &self,
        shared: &SharedData,
        target: TurnViewTarget,
        owner: TurnViewOwner,
        identity: TurnViewIdentity,
        emoji: char,
        source: &'static str,
    ) -> bool {
        self.note_queue_marker_removed_delivery(shared, target, owner, identity, emoji, source)
            .await
            .delivered()
    }

    pub(in crate::services::discord) async fn note_untracked_reaction_added(
        &self,
        shared: &SharedData,
        target: TurnViewTarget,
        identity: TurnViewIdentity,
        emoji: char,
        source: &'static str,
    ) -> bool {
        self.note_untracked_reaction(shared, target, identity, emoji, true, source)
            .await
            .delivered()
    }

    async fn note_untracked_reaction(
        &self,
        shared: &SharedData,
        target: TurnViewTarget,
        identity: TurnViewIdentity,
        emoji: char,
        add: bool,
        source: &'static str,
    ) -> TurnViewDelivery {
        if !super::reaction_lifecycle::is_real_discord_message_id(target.message_id) {
            tracing::debug!(
                channel_id = target.channel_id.get(),
                message = target.message_id.get(),
                target_kind = ?target.kind,
                emoji = %emoji,
                add,
                source,
                "turn view untracked reaction skipped for non-Discord/synthetic message id"
            );
            return TurnViewDelivery::Delivered;
        }

        let target_lock = self.target_lock(target);
        {
            let _target_guard = target_lock.lock().await;
            let resolved_identity =
                match self.resolve_identity(shared, target.kind, identity, source) {
                    Some(identity) => identity,
                    None => return TurnViewDelivery::Failed,
                };
            let delivery = self
                .apply_reaction(shared, target, emoji, add, &resolved_identity, source)
                .await;
            if !delivery.delivered() {
                return delivery;
            }
        }
        if !self.targets.contains_key(&target) {
            self.prune_target_lock_if_idle(target);
        }
        TurnViewDelivery::Delivered
    }

    async fn note_start_rolled_back_to_queued_delivery(
        &self,
        shared: &SharedData,
        target: TurnViewTarget,
        owner: TurnViewOwner,
        start_attempt: TurnStartAttempt,
        source: &'static str,
    ) -> TurnViewDelivery {
        if !super::reaction_lifecycle::is_real_discord_message_id(target.message_id) {
            tracing::debug!(
                channel_id = target.channel_id.get(),
                message = target.message_id.get(),
                target_kind = ?target.kind,
                source,
                "turn view start rollback skipped for non-Discord/synthetic message id"
            );
            return TurnViewDelivery::Delivered;
        }

        let target_lock = self.target_lock(target);
        let _target_guard = target_lock.lock().await;

        let current = self
            .targets
            .get(&target)
            .map(|entry| entry.clone())
            .or_else(|| self.load_persisted_target(target, shared, source));

        let Some(current) = current else {
            tracing::info!(
                channel_id = target.channel_id.get(),
                message = target.message_id.get(),
                target_kind = ?target.kind,
                source,
                "turn view start rollback ignored without current pending state"
            );
            return TurnViewDelivery::Delivered;
        };

        if current.owner != owner
            || current.applied != TurnViewState::Pending
            || current.start_attempt != Some(start_attempt)
        {
            tracing::info!(
                channel_id = target.channel_id.get(),
                message = target.message_id.get(),
                target_kind = ?target.kind,
                source,
                current_state = ?current.applied,
                current_generation = current.owner.generation,
                current_turn_id = %current.owner.turn_id,
                current_start_attempt = current.start_attempt.map(TurnStartAttempt::get),
                rollback_generation = owner.generation,
                rollback_turn_id = %owner.turn_id,
                rollback_start_attempt = start_attempt.get(),
                "turn view start rollback ignored because current state is not the matching pending start attempt"
            );
            if current.applied.terminal() {
                self.finalize_target_locked(target, current.owner.generation, source, &target_lock);
            } else {
                self.targets.insert(target, current);
            }
            return TurnViewDelivery::Delivered;
        }

        let resolved_identity = current.identity.clone();
        let delivery = self
            .apply_diff(
                shared,
                target,
                TurnViewState::Pending,
                TurnViewState::Queued,
                &resolved_identity,
                source,
            )
            .await;
        if !delivery.delivered() {
            if matches!(delivery, TurnViewDelivery::FailedPermanent) {
                self.discard_target_locked(target, source, &target_lock);
            }
            return delivery;
        }

        let applied_target =
            Self::applied_target(owner, TurnViewState::Queued, resolved_identity, None);
        self.targets.insert(target, applied_target.clone());
        self.persist_target(target, &applied_target, shared, source);
        TurnViewDelivery::Delivered
    }

    pub(in crate::services::discord) async fn note_turn_completed(
        &self,
        shared: &SharedData,
        target: TurnViewTarget,
        owner: TurnViewOwner,
        identity: TurnViewIdentity,
        source: &'static str,
    ) -> bool {
        self.note_state(
            shared,
            target,
            owner,
            identity,
            TurnViewState::Completed,
            source,
        )
        .await
    }

    pub(in crate::services::discord) async fn note_turn_failed(
        &self,
        shared: &SharedData,
        target: TurnViewTarget,
        owner: TurnViewOwner,
        identity: TurnViewIdentity,
        source: &'static str,
    ) -> bool {
        self.note_state(
            shared,
            target,
            owner,
            identity,
            TurnViewState::Failed,
            source,
        )
        .await
    }

    #[allow(dead_code)]
    pub(in crate::services::discord) async fn note_turn_stopped(
        &self,
        shared: &SharedData,
        target: TurnViewTarget,
        owner: TurnViewOwner,
        identity: TurnViewIdentity,
        source: &'static str,
    ) -> bool {
        self.note_state(
            shared,
            target,
            owner,
            identity,
            TurnViewState::Stopped,
            source,
        )
        .await
    }

    pub(in crate::services::discord) async fn note_turn_cleared(
        &self,
        shared: &SharedData,
        target: TurnViewTarget,
        owner: TurnViewOwner,
        identity: TurnViewIdentity,
        source: &'static str,
    ) -> bool {
        self.note_state(shared, target, owner, identity, TurnViewState::None, source)
            .await
    }

    pub(in crate::services::discord) async fn note_turn_cleared_if_attempt_matches(
        &self,
        shared: &SharedData,
        target: TurnViewTarget,
        owner: TurnViewOwner,
        identity: TurnViewIdentity,
        start_attempt: TurnStartAttempt,
        source: &'static str,
    ) -> bool {
        let (delivery, _) = self
            .note_state_delivery_with_clear_attempt_guard(
                shared,
                target,
                owner,
                identity,
                TurnViewState::None,
                Some(start_attempt),
                source,
            )
            .await;
        delivery.delivered()
    }

    #[allow(dead_code)]
    pub(in crate::services::discord) async fn note_anchor_replaced(
        &self,
        shared: &SharedData,
        old_target: TurnViewTarget,
        new_target: TurnViewTarget,
        owner: TurnViewOwner,
        identity: TurnViewIdentity,
        source: &'static str,
    ) -> bool {
        let cleared = self
            .note_turn_cleared(shared, old_target, owner.clone(), identity.clone(), source)
            .await;
        let started = self
            .note_turn_started(shared, new_target, owner, identity, source)
            .await;
        cleared && started
    }

    #[cfg_attr(test, allow(dead_code))]
    pub(in crate::services::discord) fn evict_finalized(
        &self,
        target: TurnViewTarget,
        owner: &TurnViewOwner,
    ) {
        let remove = self
            .targets
            .get(&target)
            .map(|entry| entry.owner == *owner)
            .unwrap_or(false);
        if remove {
            self.remember_recently_finalized(target, owner.generation);
            self.targets.remove(&target);
            self.delete_persisted_target(target, "evict_finalized");
            self.prune_target_lock_if_idle(target);
        }
    }

    async fn note_state(
        &self,
        shared: &SharedData,
        target: TurnViewTarget,
        owner: TurnViewOwner,
        identity: TurnViewIdentity,
        desired: TurnViewState,
        source: &'static str,
    ) -> bool {
        self.note_state_delivery(shared, target, owner, identity, desired, source)
            .await
            .delivered()
    }

    async fn note_state_delivery(
        &self,
        shared: &SharedData,
        target: TurnViewTarget,
        owner: TurnViewOwner,
        identity: TurnViewIdentity,
        desired: TurnViewState,
        source: &'static str,
    ) -> TurnViewDelivery {
        let (delivery, _) = self
            .note_state_delivery_with_attempt(shared, target, owner, identity, desired, source)
            .await;
        delivery
    }

    async fn note_state_delivery_with_attempt(
        &self,
        shared: &SharedData,
        target: TurnViewTarget,
        owner: TurnViewOwner,
        identity: TurnViewIdentity,
        desired: TurnViewState,
        source: &'static str,
    ) -> (TurnViewDelivery, Option<TurnStartAttempt>) {
        self.note_state_delivery_with_clear_attempt_guard(
            shared, target, owner, identity, desired, None, source,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn note_state_delivery_with_clear_attempt_guard(
        &self,
        shared: &SharedData,
        target: TurnViewTarget,
        owner: TurnViewOwner,
        identity: TurnViewIdentity,
        desired: TurnViewState,
        clear_start_attempt: Option<TurnStartAttempt>,
        source: &'static str,
    ) -> (TurnViewDelivery, Option<TurnStartAttempt>) {
        let start_attempt = self.start_attempt_for(desired);
        if !super::reaction_lifecycle::is_real_discord_message_id(target.message_id) {
            tracing::debug!(
                channel_id = target.channel_id.get(),
                message = target.message_id.get(),
                target_kind = ?target.kind,
                desired = ?desired,
                source,
                "turn view reaction skipped for non-Discord/synthetic message id"
            );
            return (TurnViewDelivery::Delivered, None);
        }

        let target_lock = self.target_lock(target);
        let _target_guard = target_lock.lock().await;

        if desired.is_queue_marker()
            && self.recently_finalized_blocks_queued(target, owner.generation)
            && !queue_repair::allows(shared, target, None, source).await
        {
            tracing::debug!(
                channel_id = target.channel_id.get(),
                message = target.message_id.get(),
                target_kind = ?target.kind,
                source,
                queued_generation = owner.generation,
                "turn view queued notification ignored for recently finalized generation"
            );
            return (TurnViewDelivery::Delivered, None);
        }

        let current = self
            .targets
            .get(&target)
            .map(|entry| entry.clone())
            .or_else(|| self.load_persisted_target(target, shared, source));
        if let Some(current) = current.as_ref() {
            if desired == TurnViewState::None
                && clear_start_attempt.is_none()
                && !current.legacy_queue_reactions.is_empty()
            {
                let delivery = self
                    .remove_legacy_queue_reactions(
                        shared,
                        target,
                        &current.legacy_queue_reactions,
                        &current.identity,
                        source,
                    )
                    .await;
                if delivery.delivered() || matches!(delivery, TurnViewDelivery::FailedPermanent) {
                    self.discard_target_locked(target, source, &target_lock);
                }
                return (delivery, None);
            }
            if desired == TurnViewState::None
                && let Some(clear_start_attempt) = clear_start_attempt
                && (current.owner != owner
                    || current.applied != TurnViewState::Pending
                    || current.start_attempt != Some(clear_start_attempt))
            {
                tracing::debug!(
                    channel_id = target.channel_id.get(),
                    message = target.message_id.get(),
                    target_kind = ?target.kind,
                    source,
                    current_state = ?current.applied,
                    current_generation = current.owner.generation,
                    current_turn_id = %current.owner.turn_id,
                    current_start_attempt = current.start_attempt.map(TurnStartAttempt::get),
                    clear_generation = owner.generation,
                    clear_turn_id = %owner.turn_id,
                    clear_start_attempt = clear_start_attempt.get(),
                    "turn view attempt-scoped clear ignored because current state is not the matching pending start attempt"
                );
                self.targets.insert(target, current.clone());
                return (TurnViewDelivery::Delivered, None);
            }
            if current.owner == owner
                && desired.is_queue_marker()
                && current.applied.started_or_terminal()
                && !queue_repair::allows(shared, target, Some(current.applied), source).await
            {
                tracing::debug!(
                    channel_id = target.channel_id.get(),
                    message = target.message_id.get(),
                    target_kind = ?target.kind,
                    source,
                    current_state = ?current.applied,
                    "turn view queued notification ignored after target already started"
                );
                if current.applied.terminal() {
                    self.finalize_target_locked(
                        target,
                        current.owner.generation,
                        source,
                        &target_lock,
                    );
                } else {
                    self.targets.insert(target, current.clone());
                }
                return (TurnViewDelivery::Delivered, None);
            }
            if current.owner != owner {
                if desired == TurnViewState::Pending
                    && current.applied == TurnViewState::Queued
                    && current.owner.turn_id == owner.turn_id
                {
                    // Restart generation handoff: the same queued message may
                    // be promoted by a fresh dcserver generation. This remains
                    // monotonic (`Queued` -> `Pending`) and preserves the
                    // original reaction identity for the mailbox removal.
                } else if desired == TurnViewState::Pending || desired.is_queue_marker() {
                    if current.applied == desired {
                        let transferred = Self::applied_target(
                            owner,
                            current.applied,
                            current.identity.clone(),
                            start_attempt,
                        );
                        self.targets.insert(target, transferred.clone());
                        self.persist_target(target, &transferred, shared, source);
                        return (TurnViewDelivery::Delivered, transferred.start_attempt);
                    }
                } else {
                    tracing::debug!(
                        channel_id = target.channel_id.get(),
                        message = target.message_id.get(),
                        target_kind = ?target.kind,
                        desired = ?desired,
                        source,
                        current_generation = current.owner.generation,
                        current_turn_id = %current.owner.turn_id,
                        stale_generation = owner.generation,
                        stale_turn_id = %owner.turn_id,
                        "turn view reaction notification ignored for stale owner"
                    );
                    if current.applied.terminal() {
                        self.finalize_target_locked(
                            target,
                            current.owner.generation,
                            source,
                            &target_lock,
                        );
                    }
                    return (TurnViewDelivery::Delivered, None);
                }
            }
            if current.applied == desired {
                if desired == TurnViewState::Pending {
                    let attempt = self.update_matching_pending_attempt(
                        shared,
                        target,
                        owner,
                        current,
                        start_attempt,
                        source,
                    );
                    return (TurnViewDelivery::Delivered, attempt);
                }
                self.targets.insert(target, current.clone());
                if desired.terminal() {
                    self.finalize_target_locked(
                        target,
                        current.owner.generation,
                        source,
                        &target_lock,
                    );
                }
                return (TurnViewDelivery::Delivered, None);
            }
        }

        let resolved_identity = match current.as_ref() {
            Some(current) => current.identity.clone(),
            None => match self.resolve_identity(shared, target.kind, identity, source) {
                Some(identity) => identity,
                None => return (TurnViewDelivery::Failed, None),
            },
        };

        if current.is_none() && desired == TurnViewState::None {
            let delivery = self
                .apply_unknown_clear(shared, target, &resolved_identity, source)
                .await;
            if delivery.delivered() {
                self.discard_target_locked(target, source, &target_lock);
            }
            return (delivery, None);
        }

        let applied = current
            .as_ref()
            .map(|entry| entry.applied)
            .unwrap_or_else(|| {
                if desired.terminal() {
                    TurnViewState::Pending
                } else {
                    TurnViewState::None
                }
            });

        let delivery = self
            .apply_diff_or_cold_terminal(
                shared,
                target,
                applied,
                desired,
                current.is_none(),
                current
                    .as_ref()
                    .map(|entry| entry.legacy_queue_reactions.as_slice())
                    .unwrap_or_default(),
                &resolved_identity,
                source,
            )
            .await;
        if !delivery.delivered() {
            if matches!(delivery, TurnViewDelivery::FailedPermanent) {
                self.discard_target_locked(target, source, &target_lock);
            }
            return (delivery, None);
        }
        if desired == TurnViewState::None {
            self.discard_target_locked(target, source, &target_lock);
        } else if desired.terminal() {
            let finalized_generation = owner.generation;
            let applied_target = Self::applied_target(owner, desired, resolved_identity, None);
            self.targets.insert(target, applied_target);
            self.finalize_target_locked(target, finalized_generation, source, &target_lock);
        } else {
            let applied_target =
                Self::applied_target(owner, desired, resolved_identity, start_attempt);
            self.targets.insert(target, applied_target.clone());
            self.persist_target(target, &applied_target, shared, source);
        }
        (TurnViewDelivery::Delivered, start_attempt)
    }

    async fn note_queue_marker_removed_delivery(
        &self,
        shared: &SharedData,
        target: TurnViewTarget,
        owner: TurnViewOwner,
        identity: TurnViewIdentity,
        emoji: char,
        source: &'static str,
    ) -> TurnViewDelivery {
        let Some(expected_state) = TurnViewState::from_queue_marker_emoji(emoji) else {
            tracing::warn!(
                channel_id = target.channel_id.get(),
                message = target.message_id.get(),
                target_kind = ?target.kind,
                emoji = %emoji,
                source,
                "turn view queue marker clear ignored for unsupported emoji"
            );
            return TurnViewDelivery::Delivered;
        };
        if !super::reaction_lifecycle::is_real_discord_message_id(target.message_id) {
            tracing::debug!(
                channel_id = target.channel_id.get(),
                message = target.message_id.get(),
                target_kind = ?target.kind,
                source,
                "turn view queued reaction clear skipped for non-Discord/synthetic message id"
            );
            return TurnViewDelivery::Delivered;
        }

        let target_lock = self.target_lock(target);
        let _target_guard = target_lock.lock().await;

        let current = self
            .targets
            .get(&target)
            .map(|entry| entry.clone())
            .or_else(|| self.load_persisted_target(target, shared, source));

        let Some(current) = current else {
            tracing::debug!(
                channel_id = target.channel_id.get(),
                message = target.message_id.get(),
                target_kind = ?target.kind,
                emoji = %emoji,
                source,
                "turn view queued reaction clear applying untracked fallback without queued state"
            );
            let Some(resolved_identity) =
                self.resolve_identity(shared, target.kind, identity, source)
            else {
                return TurnViewDelivery::Failed;
            };
            let delivery = self
                .apply_reaction(shared, target, emoji, false, &resolved_identity, source)
                .await;
            self.finish_target_locked(target, source, &target_lock, true);
            return delivery;
        };

        if current.applied != expected_state {
            if current.owner == owner && current.legacy_queue_reactions.contains(&emoji) {
                let delivery = self
                    .remove_legacy_queue_reactions(
                        shared,
                        target,
                        &current.legacy_queue_reactions,
                        &current.identity,
                        source,
                    )
                    .await;
                if delivery.delivered() || matches!(delivery, TurnViewDelivery::FailedPermanent) {
                    self.discard_target_locked(target, source, &target_lock);
                }
                return delivery;
            }
            tracing::debug!(
                channel_id = target.channel_id.get(),
                message = target.message_id.get(),
                target_kind = ?target.kind,
                emoji = %emoji,
                source,
                current_state = ?current.applied,
                "turn view queued reaction clear ignored because target has a different queue marker"
            );
            if current.applied.terminal() {
                self.finalize_target_locked(target, current.owner.generation, source, &target_lock);
            } else {
                self.targets.insert(target, current);
            }
            return TurnViewDelivery::Delivered;
        }

        if current.owner != owner {
            tracing::debug!(
                channel_id = target.channel_id.get(),
                message = target.message_id.get(),
                target_kind = ?target.kind,
                source,
                current_generation = current.owner.generation,
                current_turn_id = %current.owner.turn_id,
                cancel_generation = owner.generation,
                cancel_turn_id = %owner.turn_id,
                "turn view queued reaction clear ignored for non-matching queued generation"
            );
            self.targets.insert(target, current);
            return TurnViewDelivery::Delivered;
        }

        let resolved_identity = current.identity.clone();
        let delivery = self
            .apply_diff(
                shared,
                target,
                current.applied,
                TurnViewState::None,
                &resolved_identity,
                source,
            )
            .await;
        if delivery.delivered() || matches!(delivery, TurnViewDelivery::FailedPermanent) {
            self.discard_target_locked(target, source, &target_lock);
        }
        delivery
    }

    fn target_lock(&self, target: TurnViewTarget) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self
            .target_locks
            .lock()
            .expect("turn view target lock registry");
        locks
            .entry(target)
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    fn discard_target_locked(
        &self,
        target: TurnViewTarget,
        source: &'static str,
        target_lock: &Arc<tokio::sync::Mutex<()>>,
    ) {
        self.finish_target_locked(target, source, target_lock, true);
    }

    fn finalize_target_locked(
        &self,
        target: TurnViewTarget,
        finalized_generation: u64,
        source: &'static str,
        target_lock: &Arc<tokio::sync::Mutex<()>>,
    ) {
        self.remember_recently_finalized(target, finalized_generation);
        self.finish_target_locked(target, source, target_lock, false);
    }

    fn recently_finalized_blocks_queued(&self, target: TurnViewTarget, generation: u64) -> bool {
        self.recently_finalized
            .lock()
            .expect("turn view recently finalized guard")
            .blocks_queued(target, generation)
    }

    fn remember_recently_finalized(&self, target: TurnViewTarget, generation: u64) {
        self.recently_finalized
            .lock()
            .expect("turn view recently finalized guard")
            .remember(target, generation);
    }

    fn finish_target_locked(
        &self,
        target: TurnViewTarget,
        source: &'static str,
        target_lock: &Arc<tokio::sync::Mutex<()>>,
        force_remove_target: bool,
    ) {
        self.delete_persisted_target(target, source);
        let mut locks = self
            .target_locks
            .lock()
            .expect("turn view target lock registry");
        let prune_lock = locks.get(&target).is_some_and(|registered| {
            Arc::ptr_eq(registered, target_lock) && Arc::strong_count(registered) == 2
        });
        if force_remove_target || prune_lock {
            self.targets.remove(&target);
        }
        if prune_lock {
            locks.remove(&target);
        }
    }

    fn prune_target_lock_if_idle(&self, target: TurnViewTarget) {
        let mut locks = self
            .target_locks
            .lock()
            .expect("turn view target lock registry");
        let remove = locks
            .get(&target)
            .is_some_and(|registered| Arc::strong_count(registered) == 1);
        if remove {
            locks.remove(&target);
        }
    }

    fn resolve_identity(
        &self,
        shared: &SharedData,
        target_kind: TurnViewTargetKind,
        identity: TurnViewIdentity,
        source: &'static str,
    ) -> Option<ResolvedIdentity> {
        #[cfg(test)]
        let _ = (shared, target_kind, source);
        match identity {
            TurnViewIdentity::IntakeHttp(http) => {
                #[cfg(test)]
                let _ = &http;
                Some(ResolvedIdentity {
                    label: TurnViewTargetKind::IntakeUserMessage
                        .identity_label()
                        .to_string(),
                    token_hash: Some(shared.token_hash.clone()),
                    #[cfg(not(test))]
                    http,
                })
            }
            TurnViewIdentity::IntakeShared => {
                #[cfg(not(test))]
                {
                    let Some(http) = shared.serenity_http_or_token_fallback() else {
                        tracing::warn!(
                            target_kind = ?target_kind,
                            source,
                            "turn view reaction skipped; intake serenity http unavailable"
                        );
                        return None;
                    };
                    Some(ResolvedIdentity {
                        label: TurnViewTargetKind::IntakeUserMessage
                            .identity_label()
                            .to_string(),
                        token_hash: Some(shared.token_hash.clone()),
                        http,
                    })
                }
                #[cfg(test)]
                {
                    let _ = shared;
                    Some(ResolvedIdentity {
                        label: TurnViewTargetKind::IntakeUserMessage
                            .identity_label()
                            .to_string(),
                        token_hash: Some(shared.token_hash.clone()),
                    })
                }
            }
            TurnViewIdentity::ProviderBot => {
                #[cfg(not(test))]
                {
                    let Some(http) = shared.serenity_http_or_token_fallback() else {
                        tracing::warn!(
                            target_kind = ?target_kind,
                            source,
                            "turn view reaction skipped; provider serenity http unavailable"
                        );
                        return None;
                    };
                    Some(ResolvedIdentity {
                        label: TurnViewTargetKind::TuiDirectBotAnchor
                            .identity_label()
                            .to_string(),
                        token_hash: Some(shared.token_hash.clone()),
                        http,
                    })
                }
                #[cfg(test)]
                {
                    let _ = shared;
                    Some(ResolvedIdentity {
                        label: TurnViewTargetKind::TuiDirectBotAnchor
                            .identity_label()
                            .to_string(),
                        token_hash: Some(shared.token_hash.clone()),
                    })
                }
            }
            #[cfg(test)]
            TurnViewIdentity::Test(label) => {
                let _ = (shared, target_kind, source);
                Some(ResolvedIdentity {
                    label: label.to_string(),
                    token_hash: None,
                })
            }
        }
    }

    fn resolve_persisted_identity(
        &self,
        record: &PersistedTargetState,
        shared: &SharedData,
        source: &'static str,
    ) -> Option<ResolvedIdentity> {
        #[cfg(not(test))]
        {
            let http = match record.token_hash.as_deref() {
                Some(token_hash) if token_hash != shared.token_hash => {
                    match super::settings::resolve_discord_token_by_hash(token_hash) {
                        Some(token) => Arc::new(serenity::http::Http::new(&token)),
                        None => {
                            tracing::warn!(
                                token_hash,
                                source,
                                "turn view persisted reaction identity token hash could not be resolved; falling back to current runtime identity"
                            );
                            shared.serenity_http_or_token_fallback()?
                        }
                    }
                }
                _ => shared.serenity_http_or_token_fallback()?,
            };
            Some(ResolvedIdentity {
                label: record.identity_label.clone(),
                token_hash: record.token_hash.clone(),
                http,
            })
        }
        #[cfg(test)]
        {
            let _ = (shared, source);
            Some(ResolvedIdentity {
                label: record.identity_label.clone(),
                token_hash: record.token_hash.clone(),
            })
        }
    }

    fn persisted_target_path(target: TurnViewTarget) -> Option<PathBuf> {
        super::runtime_store::discord_turn_view_reconciler_root().map(|root| {
            root.join(target.kind.as_str()).join(format!(
                "{}-{}.json",
                target.channel_id.get(),
                target.message_id.get()
            ))
        })
    }

    fn load_persisted_target(
        &self,
        target: TurnViewTarget,
        shared: &SharedData,
        source: &'static str,
    ) -> Option<AppliedTarget> {
        let path = Self::persisted_target_path(target)?;
        let text = fs::read_to_string(&path).ok()?;
        let record = match serde_json::from_str::<PersistedTargetState>(&text) {
            Ok(record) => record,
            Err(error) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %error,
                    source,
                    "turn view persisted reaction state was malformed; deleting"
                );
                let _ = fs::remove_file(&path);
                return None;
            }
        };
        if !matches!(
            record.version,
            PERSISTED_STATE_VERSION
                | LEGACY_QUEUED_HOURGLASS_STATE_VERSION
                | QUEUED_MARKER_ONLY_STATE_VERSION
        ) || record.provider != shared.provider.as_str()
            || TurnViewTargetKind::from_str(&record.kind) != Some(target.kind)
            || record.channel_id != target.channel_id.get()
            || record.message_id != target.message_id.get()
        {
            tracing::warn!(
                path = %path.display(),
                version = record.version,
                provider = %record.provider,
                kind = %record.kind,
                channel_id = record.channel_id,
                message = record.message_id,
                source,
                "turn view persisted reaction state did not match target; deleting"
            );
            let _ = fs::remove_file(&path);
            return None;
        }
        let Some(recorded_applied) = TurnViewState::from_str(&record.applied) else {
            tracing::warn!(
                path = %path.display(),
                applied = %record.applied,
                source,
                "turn view persisted reaction state had unknown applied value; deleting"
            );
            let _ = fs::remove_file(&path);
            return None;
        };
        if recorded_applied == TurnViewState::None {
            let _ = fs::remove_file(&path);
            return None;
        }
        let identity = self.resolve_persisted_identity(&record, shared, source)?;
        let legacy_queue_reactions = match record.version {
            PERSISTED_STATE_VERSION if recorded_applied.is_queue_marker() => {
                vec![reaction_set::for_state(recorded_applied)[0]]
            }
            LEGACY_QUEUED_HOURGLASS_STATE_VERSION if recorded_applied.is_queue_marker() => vec![
                reaction_set::for_state(recorded_applied)[0],
                reaction_set::for_state(TurnViewState::Pending)[0],
            ],
            _ => Vec::new(),
        };
        let applied = if legacy_queue_reactions.is_empty() {
            recorded_applied
        } else {
            TurnViewState::None
        };
        let mut target = Self::applied_target(
            TurnViewOwner::new(record.owner_generation, record.owner_turn_id),
            applied,
            identity,
            record.start_attempt_id.map(TurnStartAttempt),
        );
        target.legacy_queue_reactions = legacy_queue_reactions;
        Some(target)
    }

    fn persist_target(
        &self,
        target: TurnViewTarget,
        applied: &AppliedTarget,
        shared: &SharedData,
        source: &'static str,
    ) {
        if applied.applied == TurnViewState::None && applied.legacy_queue_reactions.is_empty() {
            self.delete_persisted_target(target, source);
            return;
        }
        let Some(path) = Self::persisted_target_path(target) else {
            return;
        };
        let applied_state = applied
            .legacy_queue_reactions
            .iter()
            .find_map(|emoji| TurnViewState::from_queue_marker_emoji(*emoji))
            .unwrap_or(applied.applied);
        let record = PersistedTargetState {
            version: if applied.applied.is_queue_marker()
                || !applied.legacy_queue_reactions.is_empty()
            {
                QUEUED_MARKER_ONLY_STATE_VERSION
            } else {
                PERSISTED_STATE_VERSION
            },
            provider: shared.provider.as_str().to_string(),
            kind: target.kind.as_str().to_string(),
            channel_id: target.channel_id.get(),
            message_id: target.message_id.get(),
            owner_generation: applied.owner.generation,
            owner_turn_id: applied.owner.turn_id.clone(),
            applied: applied_state.as_str().to_string(),
            identity_label: applied.identity.label.clone(),
            token_hash: applied.identity.token_hash.clone(),
            start_attempt_id: applied.start_attempt.map(TurnStartAttempt::get),
        };
        let Ok(json) = serde_json::to_string_pretty(&record) else {
            return;
        };
        if let Err(error) = super::runtime_store::atomic_write(&path, &json) {
            tracing::warn!(
                path = %path.display(),
                error = %error,
                source,
                "turn view persisted reaction state write failed"
            );
        }
    }

    fn delete_persisted_target(&self, target: TurnViewTarget, source: &'static str) {
        let Some(path) = Self::persisted_target_path(target) else {
            return;
        };
        if let Err(error) = fs::remove_file(&path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(
                path = %path.display(),
                error = %error,
                source,
                "turn view persisted reaction state delete failed"
            );
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn apply_diff_or_cold_terminal(
        &self,
        shared: &SharedData,
        target: TurnViewTarget,
        applied: TurnViewState,
        desired: TurnViewState,
        cold: bool,
        legacy_queue_reactions: &[char],
        identity: &ResolvedIdentity,
        source: &'static str,
    ) -> TurnViewDelivery {
        if cold && desired.terminal() {
            let mut delivery = self
                .apply_unknown_clear(shared, target, identity, source)
                .await;
            if !matches!(delivery, TurnViewDelivery::FailedPermanent) {
                delivery = delivery.merge(
                    self.apply_diff(
                        shared,
                        target,
                        TurnViewState::None,
                        desired,
                        identity,
                        source,
                    )
                    .await,
                );
            }
            return delivery;
        }

        if !legacy_queue_reactions.is_empty() {
            let delivery = self
                .remove_legacy_queue_reactions(
                    shared,
                    target,
                    legacy_queue_reactions,
                    identity,
                    source,
                )
                .await;
            if !delivery.delivered() {
                return delivery;
            }
        }
        self.apply_diff(shared, target, applied, desired, identity, source)
            .await
    }

    async fn remove_legacy_queue_reactions(
        &self,
        shared: &SharedData,
        target: TurnViewTarget,
        reactions: &[char],
        identity: &ResolvedIdentity,
        source: &'static str,
    ) -> TurnViewDelivery {
        let mut removed = Vec::new();
        for emoji in reactions {
            let delivery = self
                .apply_reaction(shared, target, *emoji, false, identity, source)
                .await;
            if !delivery.delivered() {
                self.compensate_reaction_ops(shared, target, identity, source, &removed)
                    .await;
                return delivery;
            }
            removed.push((*emoji, false));
        }
        TurnViewDelivery::Delivered
    }

    async fn apply_diff(
        &self,
        shared: &SharedData,
        target: TurnViewTarget,
        applied: TurnViewState,
        desired: TurnViewState,
        identity: &ResolvedIdentity,
        source: &'static str,
    ) -> TurnViewDelivery {
        let applied_reactions = reaction_set::for_state(applied);
        let desired_reactions = reaction_set::for_state(desired);
        let mut applied_ops = Vec::new();
        for emoji in applied_reactions {
            if desired_reactions.contains(emoji) {
                continue;
            }
            let delivery = self
                .apply_reaction(shared, target, *emoji, false, identity, source)
                .await;
            if !delivery.delivered() {
                self.compensate_reaction_ops(shared, target, identity, source, &applied_ops)
                    .await;
                return delivery;
            }
            applied_ops.push((*emoji, false));
        }
        for emoji in desired_reactions {
            if applied_reactions.contains(emoji) {
                continue;
            }
            let delivery = self
                .apply_reaction(shared, target, *emoji, true, identity, source)
                .await;
            if !delivery.delivered() {
                self.compensate_reaction_ops(shared, target, identity, source, &applied_ops)
                    .await;
                return delivery;
            }
            applied_ops.push((*emoji, true));
        }
        TurnViewDelivery::Delivered
    }

    async fn compensate_reaction_ops(
        &self,
        shared: &SharedData,
        target: TurnViewTarget,
        identity: &ResolvedIdentity,
        source: &'static str,
        applied_ops: &[(char, bool)],
    ) {
        for (emoji, add) in applied_ops.iter().rev() {
            let compensation = self
                .apply_reaction(shared, target, *emoji, !*add, identity, source)
                .await;
            if !compensation.delivered() {
                tracing::warn!(
                    channel_id = target.channel_id.get(),
                    message = target.message_id.get(),
                    emoji = %emoji,
                    source,
                    "turn view reaction compensation failed"
                );
            }
        }
    }

    async fn apply_unknown_clear(
        &self,
        shared: &SharedData,
        target: TurnViewTarget,
        identity: &ResolvedIdentity,
        source: &'static str,
    ) -> TurnViewDelivery {
        let mut delivery = TurnViewDelivery::Delivered;
        for emoji in TURN_VIEW_REACTIONS {
            delivery = delivery.merge(
                self.apply_reaction(shared, target, emoji, false, identity, source)
                    .await,
            );
        }
        delivery
    }

    async fn apply_reaction(
        &self,
        shared: &SharedData,
        target: TurnViewTarget,
        emoji: char,
        add: bool,
        identity: &ResolvedIdentity,
        source: &'static str,
    ) -> TurnViewDelivery {
        debug_assert!(
            TURN_VIEW_REACTIONS.contains(&emoji) || QUEUE_EXIT_FEEDBACK_REACTIONS.contains(&emoji)
        );
        #[cfg(not(test))]
        {
            let result = if add {
                super::reaction_lifecycle::try_add_reaction_raw_with_shared_detailed(
                    &identity.http,
                    shared,
                    target.channel_id,
                    target.message_id,
                    emoji,
                )
                .await
            } else {
                super::reaction_lifecycle::try_remove_reaction_raw_with_shared_detailed(
                    &identity.http,
                    shared,
                    target.channel_id,
                    target.message_id,
                    emoji,
                )
                .await
            };
            if let Err(error) = result {
                tracing::warn!(
                    channel_id = target.channel_id.get(),
                    message = target.message_id.get(),
                    target_kind = ?target.kind,
                    identity = identity.label,
                    emoji = %emoji,
                    add,
                    source,
                    error = %error,
                    "turn view reaction apply failed"
                );
                return TurnViewDelivery::from_reaction_error_status(error.status());
            }
        }
        #[cfg(test)]
        {
            let _ = (shared, source);
            tokio::task::yield_now().await;
            let delivery = self
                .test_deliveries
                .lock()
                .expect("turn view test delivery lock")
                .pop_front()
                .unwrap_or(TurnViewDelivery::Delivered);
            self.ops
                .lock()
                .expect("turn view test op lock")
                .push(TestReactionOp {
                    target,
                    emoji,
                    add,
                    identity: identity.label.clone(),
                });
            delivery
        }
        #[cfg(not(test))]
        {
            TurnViewDelivery::Delivered
        }
    }
}

pub(in crate::services::discord) fn turn_view_owner_for_message(
    channel_id: ChannelId,
    message_id: MessageId,
    generation: u64,
) -> TurnViewOwner {
    TurnViewOwner::for_message(channel_id, message_id, generation)
}

pub(in crate::services::discord) async fn note_intake_turn_started(
    shared: &Arc<SharedData>,
    http: &Arc<serenity::http::Http>,
    channel_id: ChannelId,
    message_id: MessageId,
    generation: u64,
    source: &'static str,
) -> bool {
    let target = TurnViewTarget::intake_user_message(channel_id, message_id);
    let owner = turn_view_owner_for_message(channel_id, message_id, generation);
    shared
        .turn_view_reconciler
        .note_turn_started(
            shared,
            target,
            owner,
            TurnViewIdentity::IntakeHttp(http.clone()),
            source,
        )
        .await
}

pub(in crate::services::discord) async fn note_intake_turn_started_with_attempt(
    shared: &Arc<SharedData>,
    http: &Arc<serenity::http::Http>,
    channel_id: ChannelId,
    message_id: MessageId,
    generation: u64,
    source: &'static str,
) -> TurnViewStartRecord {
    let target = TurnViewTarget::intake_user_message(channel_id, message_id);
    let owner = turn_view_owner_for_message(channel_id, message_id, generation);
    shared
        .turn_view_reconciler
        .note_turn_started_with_attempt(
            shared,
            target,
            owner,
            TurnViewIdentity::IntakeHttp(http.clone()),
            source,
        )
        .await
}

pub(in crate::services::discord) async fn note_intake_queue_marker_added(
    shared: &Arc<SharedData>,
    http: &Arc<serenity::http::Http>,
    channel_id: ChannelId,
    message_id: MessageId,
    generation: u64,
    emoji: char,
    source: &'static str,
) -> bool {
    let target = TurnViewTarget::intake_user_message(channel_id, message_id);
    let owner = turn_view_owner_for_message(channel_id, message_id, generation);
    shared
        .turn_view_reconciler
        .note_queue_marker_added(
            shared,
            target,
            owner,
            TurnViewIdentity::IntakeHttp(http.clone()),
            emoji,
            source,
        )
        .await
}

pub(in crate::services::discord) async fn note_intake_start_rolled_back_to_queued(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    message_id: MessageId,
    generation: u64,
    start_attempt: TurnStartAttempt,
    source: &'static str,
) -> bool {
    let target = TurnViewTarget::intake_user_message(channel_id, message_id);
    let owner = turn_view_owner_for_message(channel_id, message_id, generation);
    shared
        .turn_view_reconciler
        .note_start_rolled_back_to_queued(shared, target, owner, start_attempt, source)
        .await
}

pub(in crate::services::discord) async fn note_intake_turn_completed(
    shared: &Arc<SharedData>,
    http: &Arc<serenity::http::Http>,
    channel_id: ChannelId,
    message_id: MessageId,
    generation: u64,
    source: &'static str,
) -> bool {
    let target = TurnViewTarget::intake_user_message(channel_id, message_id);
    let owner = turn_view_owner_for_message(channel_id, message_id, generation);
    shared
        .turn_view_reconciler
        .note_turn_completed(
            shared,
            target,
            owner,
            TurnViewIdentity::IntakeHttp(http.clone()),
            source,
        )
        .await
}

pub(in crate::services::discord) async fn note_intake_turn_failed(
    shared: &Arc<SharedData>,
    http: &Arc<serenity::http::Http>,
    channel_id: ChannelId,
    message_id: MessageId,
    generation: u64,
    source: &'static str,
) -> bool {
    let target = TurnViewTarget::intake_user_message(channel_id, message_id);
    let owner = turn_view_owner_for_message(channel_id, message_id, generation);
    shared
        .turn_view_reconciler
        .note_turn_failed(
            shared,
            target,
            owner,
            TurnViewIdentity::IntakeHttp(http.clone()),
            source,
        )
        .await
}

pub(in crate::services::discord) async fn note_intake_turn_cleared(
    shared: &Arc<SharedData>,
    http: &Arc<serenity::http::Http>,
    channel_id: ChannelId,
    message_id: MessageId,
    generation: u64,
    source: &'static str,
) -> bool {
    let target = TurnViewTarget::intake_user_message(channel_id, message_id);
    let owner = turn_view_owner_for_message(channel_id, message_id, generation);
    shared
        .turn_view_reconciler
        .note_turn_cleared(
            shared,
            target,
            owner,
            TurnViewIdentity::IntakeHttp(http.clone()),
            source,
        )
        .await
}

pub(in crate::services::discord) async fn note_intake_turn_cleared_if_attempt_matches(
    shared: &Arc<SharedData>,
    http: &Arc<serenity::http::Http>,
    channel_id: ChannelId,
    message_id: MessageId,
    generation: u64,
    start_attempt: TurnStartAttempt,
    source: &'static str,
) -> bool {
    let target = TurnViewTarget::intake_user_message(channel_id, message_id);
    let owner = turn_view_owner_for_message(channel_id, message_id, generation);
    shared
        .turn_view_reconciler
        .note_turn_cleared_if_attempt_matches(
            shared,
            target,
            owner,
            TurnViewIdentity::IntakeHttp(http.clone()),
            start_attempt,
            source,
        )
        .await
}

pub(in crate::services::discord) async fn note_intake_turn_cleared_current_if_attempt_matches(
    shared: &Arc<SharedData>,
    http: &Arc<serenity::http::Http>,
    channel_id: ChannelId,
    message_id: MessageId,
    start_attempt: Option<TurnStartAttempt>,
    source: &'static str,
) -> bool {
    let Some(start_attempt) = start_attempt else {
        return true;
    };
    note_intake_turn_cleared_if_attempt_matches(
        shared,
        http,
        channel_id,
        message_id,
        shared.restart.current_generation,
        start_attempt,
        source,
    )
    .await
}

pub(in crate::services::discord) async fn note_intake_queue_marker_removed(
    shared: &Arc<SharedData>,
    http: &Arc<serenity::http::Http>,
    channel_id: ChannelId,
    message_id: MessageId,
    generation: u64,
    emoji: char,
    source: &'static str,
) -> bool {
    let target = TurnViewTarget::intake_user_message(channel_id, message_id);
    let owner = turn_view_owner_for_message(channel_id, message_id, generation);
    shared
        .turn_view_reconciler
        .note_queue_marker_removed(
            shared,
            target,
            owner,
            TurnViewIdentity::IntakeHttp(http.clone()),
            emoji,
            source,
        )
        .await
}

pub(in crate::services::discord) async fn note_intake_turn_started_current(
    shared: &Arc<SharedData>,
    http: &Arc<serenity::http::Http>,
    channel_id: ChannelId,
    message_id: MessageId,
    source: &'static str,
) -> bool {
    note_intake_turn_started(
        shared,
        http,
        channel_id,
        message_id,
        shared.restart.current_generation,
        source,
    )
    .await
}

pub(in crate::services::discord) async fn note_intake_turn_started_current_with_attempt(
    shared: &Arc<SharedData>,
    http: &Arc<serenity::http::Http>,
    channel_id: ChannelId,
    message_id: MessageId,
    source: &'static str,
) -> TurnViewStartRecord {
    note_intake_turn_started_with_attempt(
        shared,
        http,
        channel_id,
        message_id,
        shared.restart.current_generation,
        source,
    )
    .await
}

pub(in crate::services::discord) async fn note_intake_queue_marker_added_current(
    shared: &Arc<SharedData>,
    http: &Arc<serenity::http::Http>,
    channel_id: ChannelId,
    message_id: MessageId,
    emoji: char,
    source: &'static str,
) -> bool {
    note_intake_queue_marker_added(
        shared,
        http,
        channel_id,
        message_id,
        shared.restart.current_generation,
        emoji,
        source,
    )
    .await
}

pub(in crate::services::discord) async fn note_intake_start_rolled_back_to_queued_current(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    message_id: MessageId,
    start_attempt: TurnStartAttempt,
    source: &'static str,
) -> bool {
    note_intake_start_rolled_back_to_queued(
        shared,
        channel_id,
        message_id,
        shared.restart.current_generation,
        start_attempt,
        source,
    )
    .await
}

pub(in crate::services::discord) async fn note_intake_turn_cleared_current(
    shared: &Arc<SharedData>,
    http: &Arc<serenity::http::Http>,
    channel_id: ChannelId,
    message_id: MessageId,
    source: &'static str,
) -> bool {
    note_intake_turn_cleared(
        shared,
        http,
        channel_id,
        message_id,
        shared.restart.current_generation,
        source,
    )
    .await
}

pub(in crate::services::discord) async fn note_intake_queue_marker_removed_current(
    shared: &Arc<SharedData>,
    http: &Arc<serenity::http::Http>,
    channel_id: ChannelId,
    message_id: MessageId,
    emoji: char,
    source: &'static str,
) -> bool {
    note_intake_queue_marker_removed(
        shared,
        http,
        channel_id,
        message_id,
        shared.restart.current_generation,
        emoji,
        source,
    )
    .await
}

async fn note_intake_turn_via_shared(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    message_id: MessageId,
    generation: u64,
    state: TurnViewState,
    source: &'static str,
) -> bool {
    let target = TurnViewTarget::intake_user_message(channel_id, message_id);
    let owner = turn_view_owner_for_message(channel_id, message_id, generation);
    shared
        .turn_view_reconciler
        .note_state(
            shared,
            target,
            owner,
            TurnViewIdentity::IntakeShared,
            state,
            source,
        )
        .await
}

pub(in crate::services::discord) async fn note_intake_turn_completed_via_shared(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    message_id: MessageId,
    generation: u64,
    source: &'static str,
) -> bool {
    note_intake_turn_via_shared(
        shared,
        channel_id,
        message_id,
        generation,
        TurnViewState::Completed,
        source,
    )
    .await
}

pub(in crate::services::discord) async fn note_intake_turn_failed_via_shared(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    message_id: MessageId,
    generation: u64,
    source: &'static str,
) -> bool {
    note_intake_turn_via_shared(
        shared,
        channel_id,
        message_id,
        generation,
        TurnViewState::Failed,
        source,
    )
    .await
}

pub(in crate::services::discord) async fn note_intake_turn_stopped_via_shared(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    message_id: MessageId,
    generation: u64,
    source: &'static str,
) -> bool {
    note_intake_turn_via_shared(
        shared,
        channel_id,
        message_id,
        generation,
        TurnViewState::Stopped,
        source,
    )
    .await
}

pub(in crate::services::discord) async fn note_intake_turn_cleared_via_shared(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    message_id: MessageId,
    generation: u64,
    source: &'static str,
) -> bool {
    note_intake_turn_via_shared(
        shared,
        channel_id,
        message_id,
        generation,
        TurnViewState::None,
        source,
    )
    .await
}

pub(in crate::services::discord) async fn note_tui_anchor_started(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    message_id: MessageId,
    generation: u64,
    source: &'static str,
) -> bool {
    let target = TurnViewTarget::tui_direct_bot_anchor(channel_id, message_id);
    let owner = turn_view_owner_for_message(channel_id, message_id, generation);
    shared
        .turn_view_reconciler
        .note_turn_started(shared, target, owner, TurnViewIdentity::ProviderBot, source)
        .await
}

pub(in crate::services::discord) async fn note_tui_anchor_completed(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    message_id: MessageId,
    generation: u64,
    source: &'static str,
) -> bool {
    let target = TurnViewTarget::tui_direct_bot_anchor(channel_id, message_id);
    let owner = turn_view_owner_for_message(channel_id, message_id, generation);
    shared
        .turn_view_reconciler
        .note_turn_completed(shared, target, owner, TurnViewIdentity::ProviderBot, source)
        .await
}

pub(in crate::services::discord) async fn note_tui_anchor_completed_delivery(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    message_id: MessageId,
    generation: u64,
    source: &'static str,
) -> TurnViewDelivery {
    note_tui_anchor_delivery(
        shared,
        channel_id,
        message_id,
        generation,
        TurnViewState::Completed,
        source,
    )
    .await
}

pub(in crate::services::discord) async fn note_tui_anchor_failed_delivery(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    message_id: MessageId,
    generation: u64,
    source: &'static str,
) -> TurnViewDelivery {
    note_tui_anchor_delivery(
        shared,
        channel_id,
        message_id,
        generation,
        TurnViewState::Failed,
        source,
    )
    .await
}

async fn note_tui_anchor_delivery(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    message_id: MessageId,
    generation: u64,
    state: TurnViewState,
    source: &'static str,
) -> TurnViewDelivery {
    let target = TurnViewTarget::tui_direct_bot_anchor(channel_id, message_id);
    let owner = turn_view_owner_for_message(channel_id, message_id, generation);
    shared
        .turn_view_reconciler
        .note_state_delivery(
            shared,
            target,
            owner,
            TurnViewIdentity::ProviderBot,
            state,
            source,
        )
        .await
}

#[cfg(test)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::services::discord) struct TestReactionOp {
    pub(in crate::services::discord) target: TurnViewTarget,
    pub(in crate::services::discord) emoji: char,
    pub(in crate::services::discord) add: bool,
    pub(in crate::services::discord) identity: String,
}

#[cfg(test)]
impl TurnViewReconciler {
    pub(in crate::services::discord) fn ops(&self) -> Vec<TestReactionOp> {
        self.ops.lock().expect("turn view test op lock").clone()
    }

    pub(in crate::services::discord) fn with_test_deliveries(
        deliveries: Vec<TurnViewDelivery>,
    ) -> Self {
        let reconciler = Self::default();
        reconciler
            .test_deliveries
            .lock()
            .expect("turn view test delivery lock")
            .extend(deliveries);
        reconciler
    }

    pub(in crate::services::discord) fn target_lock_count(&self, target: TurnViewTarget) -> usize {
        usize::from(
            self.target_locks
                .lock()
                .expect("turn view target lock registry")
                .contains_key(&target),
        )
    }
}

#[cfg(test)]
mod tests;
