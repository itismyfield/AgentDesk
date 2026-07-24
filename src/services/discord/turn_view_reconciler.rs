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

use super::{SharedData, queue_reactions, reaction_lifecycle, runtime_store, settings};

// #4278: descendant module owns orphan-`⏳` defense while retaining access to
// the reconciler's private target store and persistence helpers.
mod orphan_sweep;
// #4554: mailbox-truth repair is isolated to keep this giant module net-zero.
mod api;
mod apply;
pub(in crate::services::discord) use api::*;
mod queue_repair;
mod reaction_set;
mod resolution;
mod store;
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
