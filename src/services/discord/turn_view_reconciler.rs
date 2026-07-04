use std::{collections::HashMap, fs, path::PathBuf, sync::Arc};

use dashmap::DashMap;
use poise::serenity_prelude as serenity;
use serde::{Deserialize, Serialize};
use serenity::{ChannelId, MessageId};

use super::SharedData;

const TURN_LIFECYCLE_REACTIONS: [char; 4] = ['⏳', '✅', '⚠', '🛑'];
const PERSISTED_STATE_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::services::discord) enum TurnViewState {
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
            Self::Pending => "pending",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Stopped => "stopped",
            Self::None => "none",
        }
    }

    fn from_str(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(Self::Pending),
            "completed" => Some(Self::Completed),
            "failed" => Some(Self::Failed),
            "stopped" => Some(Self::Stopped),
            "none" => Some(Self::None),
            _ => None,
        }
    }

    fn emoji(self) -> Option<char> {
        match self {
            Self::Pending => Some('⏳'),
            Self::Completed => Some('✅'),
            Self::Failed => Some('⚠'),
            Self::Stopped => Some('🛑'),
            Self::None => None,
        }
    }

    fn terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Stopped)
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
}

#[derive(Default)]
pub(in crate::services::discord) struct TurnViewReconciler {
    targets: DashMap<TurnViewTarget, AppliedTarget>,
    target_locks: std::sync::Mutex<HashMap<TurnViewTarget, Arc<tokio::sync::Mutex<()>>>>,
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
        self.note_state(
            shared,
            target,
            owner,
            identity,
            TurnViewState::Pending,
            source,
        )
        .await
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
        if !super::reaction_lifecycle::is_real_discord_message_id(target.message_id) {
            tracing::debug!(
                channel = target.channel_id.get(),
                message = target.message_id.get(),
                target_kind = ?target.kind,
                desired = ?desired,
                source,
                "turn view reaction skipped for non-Discord/synthetic message id"
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
        if let Some(current) = current.as_ref() {
            if current.owner != owner {
                if desired == TurnViewState::Pending {
                    if current.applied == desired {
                        let transferred = AppliedTarget {
                            owner,
                            applied: current.applied,
                            identity: current.identity.clone(),
                        };
                        self.targets.insert(target, transferred.clone());
                        self.persist_target(target, &transferred, shared, source);
                        return TurnViewDelivery::Delivered;
                    }
                } else {
                    tracing::debug!(
                        channel = target.channel_id.get(),
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
                        self.finalize_target_locked(target, source, &target_lock);
                    }
                    return TurnViewDelivery::Delivered;
                }
            }
            if current.applied == desired {
                self.targets.insert(target, current.clone());
                if desired.terminal() {
                    self.finalize_target_locked(target, source, &target_lock);
                }
                return TurnViewDelivery::Delivered;
            }
        }

        let resolved_identity = match current.as_ref() {
            Some(current) => current.identity.clone(),
            None => match self.resolve_identity(shared, target.kind, identity, source) {
                Some(identity) => identity,
                None => return TurnViewDelivery::Failed,
            },
        };

        if current.is_none() && desired == TurnViewState::None {
            let delivery = self
                .apply_unknown_clear(shared, target, &resolved_identity, source)
                .await;
            if delivery.delivered() {
                self.discard_target_locked(target, source, &target_lock);
            }
            return delivery;
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
        if desired == TurnViewState::None {
            self.discard_target_locked(target, source, &target_lock);
        } else if desired.terminal() {
            let applied_target = AppliedTarget {
                owner,
                applied: desired,
                identity: resolved_identity,
            };
            self.targets.insert(target, applied_target);
            self.finalize_target_locked(target, source, &target_lock);
        } else {
            let applied_target = AppliedTarget {
                owner,
                applied: desired,
                identity: resolved_identity,
            };
            self.targets.insert(target, applied_target.clone());
            self.persist_target(target, &applied_target, shared, source);
        }
        TurnViewDelivery::Delivered
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
        source: &'static str,
        target_lock: &Arc<tokio::sync::Mutex<()>>,
    ) {
        self.finish_target_locked(target, source, target_lock, false);
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
        if record.version != PERSISTED_STATE_VERSION
            || record.provider != shared.provider.as_str()
            || TurnViewTargetKind::from_str(&record.kind) != Some(target.kind)
            || record.channel_id != target.channel_id.get()
            || record.message_id != target.message_id.get()
        {
            tracing::warn!(
                path = %path.display(),
                version = record.version,
                provider = %record.provider,
                kind = %record.kind,
                channel = record.channel_id,
                message = record.message_id,
                source,
                "turn view persisted reaction state did not match target; deleting"
            );
            let _ = fs::remove_file(&path);
            return None;
        }
        let Some(applied) = TurnViewState::from_str(&record.applied) else {
            tracing::warn!(
                path = %path.display(),
                applied = %record.applied,
                source,
                "turn view persisted reaction state had unknown applied value; deleting"
            );
            let _ = fs::remove_file(&path);
            return None;
        };
        if applied == TurnViewState::None {
            let _ = fs::remove_file(&path);
            return None;
        }
        let identity = self.resolve_persisted_identity(&record, shared, source)?;
        Some(AppliedTarget {
            owner: TurnViewOwner::new(record.owner_generation, record.owner_turn_id),
            applied,
            identity,
        })
    }

    fn persist_target(
        &self,
        target: TurnViewTarget,
        applied: &AppliedTarget,
        shared: &SharedData,
        source: &'static str,
    ) {
        if applied.applied == TurnViewState::None {
            self.delete_persisted_target(target, source);
            return;
        }
        let Some(path) = Self::persisted_target_path(target) else {
            return;
        };
        let record = PersistedTargetState {
            version: PERSISTED_STATE_VERSION,
            provider: shared.provider.as_str().to_string(),
            kind: target.kind.as_str().to_string(),
            channel_id: target.channel_id.get(),
            message_id: target.message_id.get(),
            owner_generation: applied.owner.generation,
            owner_turn_id: applied.owner.turn_id.clone(),
            applied: applied.applied.as_str().to_string(),
            identity_label: applied.identity.label.clone(),
            token_hash: applied.identity.token_hash.clone(),
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

    async fn apply_diff_or_cold_terminal(
        &self,
        shared: &SharedData,
        target: TurnViewTarget,
        applied: TurnViewState,
        desired: TurnViewState,
        cold: bool,
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

        self.apply_diff(shared, target, applied, desired, identity, source)
            .await
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
        let mut delivery = TurnViewDelivery::Delivered;
        let desired_emoji = desired.emoji();
        if let Some(applied_emoji) = applied.emoji()
            && Some(applied_emoji) != desired_emoji
        {
            delivery = delivery.merge(
                self.apply_reaction(shared, target, applied_emoji, false, identity, source)
                    .await,
            );
        }
        if let Some(desired_emoji) = desired_emoji
            && applied.emoji() != Some(desired_emoji)
        {
            delivery = delivery.merge(
                self.apply_reaction(shared, target, desired_emoji, true, identity, source)
                    .await,
            );
        }
        delivery
    }

    async fn apply_unknown_clear(
        &self,
        shared: &SharedData,
        target: TurnViewTarget,
        identity: &ResolvedIdentity,
        source: &'static str,
    ) -> TurnViewDelivery {
        let mut delivery = TurnViewDelivery::Delivered;
        for emoji in TURN_LIFECYCLE_REACTIONS {
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
        debug_assert!(TURN_LIFECYCLE_REACTIONS.contains(&emoji));
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
                    channel = target.channel_id.get(),
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
