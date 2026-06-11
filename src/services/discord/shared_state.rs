//! #3038 S1/S2 — extracted field clusters of [`SharedData`].
//!
//! This module hosts named sub-structs that group cohesive `SharedData` fields
//! together with the inherent `impl SharedData` methods that exclusively own
//! those fields. The split follows the `CoreState` precedent (a field group +
//! dedicated accessors) and the #3294/#3295 behaviour-preserving decomposition
//! standard: field declarations, doc comments, visibility annotations, and
//! method bodies move *verbatim*; the only edits are the mechanical field-path
//! re-wiring forced by the new nesting (`self.<field>` →
//! `self.<group>.<field>`) and module-path adjustments (`queued_placeholders_store::`
//! → `super::queued_placeholders_store::`).
//!
//! Inherent `impl` blocks are valid from any module in the defining crate, so
//! moving the methods here keeps `SharedData`'s public surface and every call
//! site unchanged while removing ~200 production LoC from the `discord/mod.rs`
//! giant.

use std::sync::Arc;

use poise::serenity_prelude as serenity;
use poise::serenity_prelude::{ChannelId, MessageId};

use crate::services::provider::ProviderKind;

use super::{ModelPickerPendingState, QueueExitVisibleCard, SharedData};

/// #3038 cluster C — the queued-placeholder handoff state.
///
/// Groups the three fields that together implement the `📬 메시지 대기 중`
/// queued-card lifecycle: the in-memory mapping, the queue-exit clear sidecar
/// mirror, and the per-channel persistence mutexes that serialize ownership-
/// coupled mutations. See the per-field docs below for the round-5 P2 lock-span
/// invariant they jointly enforce.
pub(in crate::services::discord) struct QueuedPlaceholderState {
    /// #1332: per-channel mapping from a mailbox-queued user message id to the
    /// Discord placeholder message id displaying the `📬 메시지 대기 중` card.
    /// Populated when `mailbox_try_start_turn` reports the new message lost the
    /// race; consumed by the dispatch path when the queued turn is dequeued so
    /// the existing Queued card transitions to `Active` instead of leaking a
    /// duplicate placeholder.
    pub(in crate::services::discord) queued_placeholders:
        dashmap::DashMap<(ChannelId, MessageId), MessageId>,
    /// #1362: queue-exit placeholder cards that were removed from
    /// `queued_placeholders` while `cached_serenity_ctx` was not ready. Kept in
    /// memory and mirrored to a sidecar so ready-time drain can delete the
    /// visible stale `📬` cards after the Discord HTTP client exists.
    pub(in crate::services::discord) queue_exit_placeholder_clears:
        dashmap::DashMap<(ChannelId, MessageId), MessageId>,
    /// #1332 round-4 codex review P2 + round-5 P2: per-channel mutex guarding
    /// `queued_placeholders` snapshot writes AND any Discord PATCH that
    /// asserts queued ownership. When two updates for the same channel race
    /// (e.g., two messages lose the start-turn race simultaneously, or an
    /// insert races a queue-exit drain), each caller must serialize its
    /// `(snapshot DashMap → atomic_write file)` block so an older snapshot
    /// cannot finish last and overwrite a newer mapping. Round-5 extends the
    /// lock to span the ownership recheck + Discord edit + persistence
    /// rollback in the race-loss render path so the same Discord message can
    /// never be written by both the queued-placeholder render and the
    /// dispatch/queue-exit cleanup paths.
    ///
    /// Invariant (round-5 P2): any Discord PATCH that asserts queued
    /// ownership MUST hold this lock across both the ownership recheck AND
    /// the PATCH (and across the persistence write that follows). The map
    /// fast-path stays on the lock-free `DashMap` above; only ownership-
    /// coupled mutations are serialized per channel. The lock is async
    /// (`tokio::sync::Mutex`) so it can be held across `.await` points
    /// without blocking the runtime worker.
    pub(in crate::services::discord) queued_placeholders_persist_locks:
        dashmap::DashMap<ChannelId, Arc<tokio::sync::Mutex<()>>>,
}

/// #3038 cluster C — inherent methods that exclusively own
/// [`QueuedPlaceholderState`]. Moved verbatim from `discord/mod.rs`; the only
/// edits are the mechanical `self.<field>` → `self.queued.<field>` re-wiring
/// and the `queued_placeholders_store::` → `super::queued_placeholders_store::`
/// path adjustment. Signatures, visibility, `.await` points, and lock
/// acquisition/release order are unchanged.
impl SharedData {
    /// #1332 round-4 codex review P2 + round-5 P2: fetch (or create) the
    /// per-channel persistence mutex. The mutex itself is stored as
    /// `Arc<tokio::sync::Mutex<()>>` so callers can clone it out of the
    /// `DashMap` and release the shard lock before acquiring the channel
    /// mutex — eliminating any chance of a deadlock between DashMap shard
    /// locks and the persistence mutex. Round-5 switched from
    /// `std::sync::Mutex` to `tokio::sync::Mutex` so the lock can be held
    /// across `.await` points (specifically the `ensure_queued` Discord
    /// PATCH in the race-loss render path) without blocking a runtime
    /// worker.
    pub(in crate::services::discord) fn queued_placeholders_persist_lock(
        &self,
        channel_id: ChannelId,
    ) -> Arc<tokio::sync::Mutex<()>> {
        self.queued
            .queued_placeholders_persist_locks
            .entry(channel_id)
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    /// #1332 round-5 codex review P2: insert variant that assumes the
    /// caller already holds the per-channel persistence mutex. Used by the
    /// race-loss render path so the lock can span ownership recheck +
    /// `ensure_queued` PATCH + persistence write (and an optional rollback)
    /// without re-acquiring the lock between steps.
    pub(in crate::services::discord) fn insert_queued_placeholder_locked(
        &self,
        channel_id: ChannelId,
        user_msg_id: MessageId,
        placeholder_msg_id: MessageId,
    ) {
        self.queued
            .queued_placeholders
            .insert((channel_id, user_msg_id), placeholder_msg_id);
        super::queued_placeholders_store::persist_channel_from_map(
            &self.queued.queued_placeholders,
            &self.provider,
            &self.token_hash,
            channel_id,
        );
    }

    /// #1332 round-3 codex review P2 + round-4 P2 + round-5 P2: write-through
    /// remove for the `queued_placeholders` mapping. Returns the placeholder
    /// message id that was removed (if any) so callers can drive the same
    /// downstream flow as the raw `DashMap::remove`. Mutation + snapshot run
    /// under the per-channel persistence mutex; see
    /// `insert_queued_placeholder` for the deadlock-avoidance rationale.
    pub(super) async fn remove_queued_placeholder(
        &self,
        channel_id: ChannelId,
        user_msg_id: MessageId,
    ) -> Option<MessageId> {
        let persist_lock = self.queued_placeholders_persist_lock(channel_id);
        let _persist_guard = persist_lock.lock().await;
        self.remove_queued_placeholder_locked(channel_id, user_msg_id)
    }

    /// #1332 round-5 codex review P2: remove variant that assumes the caller
    /// already holds the per-channel persistence mutex. Used by the
    /// race-loss render path's rollback branch so the entire ownership-
    /// coupled critical section runs under one async lock acquisition.
    pub(in crate::services::discord) fn remove_queued_placeholder_locked(
        &self,
        channel_id: ChannelId,
        user_msg_id: MessageId,
    ) -> Option<MessageId> {
        let removed = self
            .queued
            .queued_placeholders
            .remove(&(channel_id, user_msg_id))
            .map(|(_, msg_id)| msg_id);
        super::queued_placeholders_store::persist_channel_from_map(
            &self.queued.queued_placeholders,
            &self.provider,
            &self.token_hash,
            channel_id,
        );
        removed
    }

    /// #1332 round-3 codex review P1: atomic ownership recheck for the
    /// race-loss render path. After enqueueing the intervention, the active
    /// turn might finish concurrently and the dispatch path can already have
    /// consumed our `(channel_id, user_msg_id)` mapping — at which point the
    /// placeholder we POSTed has been promoted to the live response card.
    /// Returns `true` only when the mapping still points at our exact
    /// `placeholder_msg_id`; callers MUST exit gracefully (without editing or
    /// deleting Discord state) if this returns `false`.
    pub(super) fn queued_placeholder_still_owned(
        &self,
        channel_id: ChannelId,
        user_msg_id: MessageId,
        placeholder_msg_id: MessageId,
    ) -> bool {
        self.queued
            .queued_placeholders
            .get(&(channel_id, user_msg_id))
            .map(|entry| *entry == placeholder_msg_id)
            .unwrap_or(false)
    }

    // #3038 S1: this method was module-private in `discord/mod.rs`; the verbatim
    // move to this sibling module requires widening its visibility to
    // `pub(in crate::services::discord)` so the same mod.rs callers
    // (`apply_queue_exit_feedback`) still resolve it. This is a compile-time-only
    // re-annotation that keeps the effective reachability identical (the method
    // was already reachable from every `discord` module via inherent dispatch).
    pub(in crate::services::discord) async fn add_pending_queue_exit_placeholder_clears(
        &self,
        channel_id: ChannelId,
        cards: &[QueueExitVisibleCard],
    ) {
        if cards.is_empty() {
            return;
        }
        let persist_lock = self.queued_placeholders_persist_lock(channel_id);
        let _persist_guard = persist_lock.lock().await;
        for card in cards {
            self.queued
                .queue_exit_placeholder_clears
                .insert((channel_id, card.user_msg_id), card.placeholder_msg_id);
        }
        super::queued_placeholders_store::persist_queue_exit_placeholder_clears_channel_from_map(
            &self.queued.queue_exit_placeholder_clears,
            &self.provider,
            &self.token_hash,
            channel_id,
        );
    }

    /// #2044 F13: enqueue a single deferred placeholder-clear when an
    /// inline `delete_message` from a non-queue-exit path (e.g.
    /// `render_visible_queued_ack`) fails. Mirrors the persistence
    /// behaviour of `add_pending_queue_exit_placeholder_clears` so the
    /// retry survives a restart and is drained by the same
    /// `drain_pending_queue_exit_placeholder_clears` worker.
    pub(in crate::services::discord) async fn add_pending_queue_exit_placeholder_clear_one(
        &self,
        channel_id: ChannelId,
        user_msg_id: MessageId,
        placeholder_msg_id: MessageId,
    ) {
        let persist_lock = self.queued_placeholders_persist_lock(channel_id);
        let _persist_guard = persist_lock.lock().await;
        self.queued
            .queue_exit_placeholder_clears
            .insert((channel_id, user_msg_id), placeholder_msg_id);
        super::queued_placeholders_store::persist_queue_exit_placeholder_clears_channel_from_map(
            &self.queued.queue_exit_placeholder_clears,
            &self.provider,
            &self.token_hash,
            channel_id,
        );
    }

    // #3038 S1: widened from module-private to `pub(in crate::services::discord)`
    // for the cross-module verbatim move (see `add_pending_queue_exit_placeholder_clears`).
    pub(in crate::services::discord) async fn remove_pending_queue_exit_placeholder_clears(
        &self,
        channel_id: ChannelId,
        cards: &[(MessageId, MessageId)],
    ) {
        if cards.is_empty() {
            return;
        }
        let persist_lock = self.queued_placeholders_persist_lock(channel_id);
        let _persist_guard = persist_lock.lock().await;
        for (user_msg_id, placeholder_msg_id) in cards {
            let key = (channel_id, *user_msg_id);
            if self
                .queued
                .queue_exit_placeholder_clears
                .get(&key)
                .map(|entry| *entry == *placeholder_msg_id)
                .unwrap_or(false)
            {
                self.queued.queue_exit_placeholder_clears.remove(&key);
            }
        }
        super::queued_placeholders_store::persist_queue_exit_placeholder_clears_channel_from_map(
            &self.queued.queue_exit_placeholder_clears,
            &self.provider,
            &self.token_hash,
            channel_id,
        );
    }

    // #3038 S1: widened from module-private to `pub(in crate::services::discord)`
    // for the cross-module verbatim move (see `add_pending_queue_exit_placeholder_clears`).
    pub(in crate::services::discord) fn pending_queue_exit_placeholder_clears(
        &self,
    ) -> Vec<(ChannelId, MessageId, MessageId)> {
        self.queued
            .queue_exit_placeholder_clears
            .iter()
            .map(|entry| {
                let (channel_id, user_msg_id) = *entry.key();
                (channel_id, user_msg_id, *entry.value())
            })
            .collect()
    }
}

/// #3038 cluster D — session-scoped override / reset-pending state.
///
/// Groups the eight fields that together implement per-channel runtime
/// overrides (model override, native fast mode, Codex goals) and the
/// session-reset bookkeeping they drive: the per-cause `*_session_reset_pending`
/// sets, the aggregated `session_reset_pending` set kept in sync by
/// `commands::config::sync_session_reset_pending`, and the staged `/model`
/// picker selections. Field declarations, docs, and types moved verbatim from
/// `discord/mod.rs`; the members' original `pub(super)` annotations (declared
/// in `discord/mod.rs`, i.e. visible up to `crate::services`) are re-spelled
/// per-field as the semantically identical `pub(in crate::services)` because
/// `pub(super)` written *here* would shrink the scope to
/// `crate::services::discord` — a compile-time-only re-annotation with zero
/// runtime effect.
pub(in crate::services) struct SessionOverrideState {
    /// Per-channel model override, independent of session lifecycle.
    /// Takes priority over role-map model. Cleared via the `/model` picker default option.
    pub(in crate::services) model_overrides: dashmap::DashMap<ChannelId, String>,
    /// Per-channel native fast mode enablement for providers that support it.
    pub(in crate::services) fast_mode_channels: dashmap::DashSet<ChannelId>,
    /// Provider-scoped pending native fast-mode resets, encoded as
    /// `provider:channel_id` strings for mixed-provider dispatch safety.
    pub(in crate::services) fast_mode_session_reset_pending: dashmap::DashSet<String>,
    /// Per-channel Codex goals feature enablement.
    pub(in crate::services) codex_goals_channels: dashmap::DashSet<ChannelId>,
    /// Channels that must restart Codex before the next turn because goals changed.
    pub(in crate::services) codex_goals_session_reset_pending: dashmap::DashSet<ChannelId>,
    /// Channels that must start a fresh provider session on the next turn
    /// because the effective model override changed.
    pub(in crate::services) model_session_reset_pending: dashmap::DashSet<ChannelId>,
    /// Channels that must start a fresh provider session on the next turn
    /// because a persisted runtime execution setting changed.
    pub(in crate::services) session_reset_pending: dashmap::DashSet<ChannelId>,
    /// Per-message staged model picker selection.
    /// Key: picker message id. Value tracks owner, target channel, and staged model until submit.
    pub(in crate::services) model_picker_pending:
        dashmap::DashMap<MessageId, ModelPickerPendingState>,
}

// #3038 cluster D — free-function helpers that exclusively own
// [`SessionOverrideState`]. Moved verbatim from `commands/config.rs` (which
// re-exports them so every `super::config::*` importer and unqualified call
// site is unchanged). The only edits are the per-item visibility
// re-annotations documented inline; bodies, signatures, and the
// `shared.overrides.<field>` access paths are byte-identical to the
// pre-move state of this slice. The settings-coupled writers
// (`update_channel_fast_mode` / `update_channel_codex_goals` /
// `update_channel_model_override`) intentionally stay in config.rs: they mix
// this cluster with `settings` persistence (`save_bot_settings`).

pub(in crate::services::discord) fn fast_mode_reset_pending_key(
    channel_id: serenity::ChannelId,
    provider: &ProviderKind,
) -> String {
    format!("{}:{}", provider.as_str(), channel_id.get())
}

pub(in crate::services::discord) fn parse_fast_mode_reset_pending_entry(
    entry: &str,
) -> Option<(Option<&str>, serenity::ChannelId)> {
    if let Some((provider_id, raw_channel_id)) = entry.split_once(':') {
        let channel_id = raw_channel_id
            .parse::<u64>()
            .ok()
            .map(serenity::ChannelId::new)?;
        return Some((Some(provider_id), channel_id));
    }

    entry
        .parse::<u64>()
        .ok()
        .map(serenity::ChannelId::new)
        .map(|channel_id| (None, channel_id))
}

fn fast_mode_reset_entry_matches_channel(entry: &str, channel_id: serenity::ChannelId) -> bool {
    parse_fast_mode_reset_pending_entry(entry)
        .map(|(_, entry_channel_id)| entry_channel_id == channel_id)
        .unwrap_or(false)
}

// #3038 S2: this helper was module-private in `commands/config.rs`; the
// verbatim move requires widening it to `pub(in crate::services::discord)`
// because one caller (`update_channel_fast_mode`, a settings-coupled writer)
// stays behind in config.rs and resolves it through the re-export there.
// Compile-time-only re-annotation; effective reachability is unchanged.
pub(in crate::services::discord) fn fast_mode_reset_entry_matches_provider(
    entry: &str,
    channel_id: serenity::ChannelId,
    provider: &ProviderKind,
) -> bool {
    parse_fast_mode_reset_pending_entry(entry)
        .map(|(provider_id, entry_channel_id)| {
            entry_channel_id == channel_id
                && provider_id
                    .map(|entry_provider| entry_provider.eq_ignore_ascii_case(provider.as_str()))
                    .unwrap_or(true)
        })
        .unwrap_or(false)
}

pub(in crate::services::discord) fn fast_mode_reset_pending_for_provider(
    shared: &Arc<SharedData>,
    channel_id: serenity::ChannelId,
    provider: &ProviderKind,
) -> bool {
    shared
        .overrides
        .fast_mode_session_reset_pending
        .iter()
        .any(|entry| fast_mode_reset_entry_matches_provider(entry.key(), channel_id, provider))
}

pub(in crate::services::discord) fn any_fast_mode_reset_pending(
    shared: &Arc<SharedData>,
    channel_id: serenity::ChannelId,
) -> bool {
    shared
        .overrides
        .fast_mode_session_reset_pending
        .iter()
        .any(|entry| fast_mode_reset_entry_matches_channel(entry.key(), channel_id))
}

pub(in crate::services::discord) fn clear_fast_mode_reset_pending_for_provider(
    shared: &Arc<SharedData>,
    channel_id: serenity::ChannelId,
    provider: &ProviderKind,
) -> bool {
    let provider_key = fast_mode_reset_pending_key(channel_id, provider);
    shared
        .overrides
        .fast_mode_session_reset_pending
        .remove(&provider_key)
        .is_some()
}

pub(in crate::services::discord) fn clear_fast_mode_reset_pending_for_channel(
    shared: &Arc<SharedData>,
    channel_id: serenity::ChannelId,
) -> bool {
    let keys: Vec<String> = shared
        .overrides
        .fast_mode_session_reset_pending
        .iter()
        .filter_map(|entry| {
            fast_mode_reset_entry_matches_channel(entry.key(), channel_id)
                .then(|| entry.key().clone())
        })
        .collect();

    let had_entries = !keys.is_empty();
    for key in keys {
        shared
            .overrides
            .fast_mode_session_reset_pending
            .remove(&key);
    }
    had_entries
}

pub(in crate::services::discord) fn sync_session_reset_pending(
    shared: &Arc<SharedData>,
    channel_id: serenity::ChannelId,
) {
    if any_fast_mode_reset_pending(shared, channel_id)
        || shared
            .overrides
            .codex_goals_session_reset_pending
            .contains(&channel_id)
        || shared
            .overrides
            .model_session_reset_pending
            .contains(&channel_id)
    {
        shared.overrides.session_reset_pending.insert(channel_id);
    } else {
        shared.overrides.session_reset_pending.remove(&channel_id);
    }
}

pub(in crate::services::discord) fn channel_fast_mode_enabled(
    shared: &Arc<SharedData>,
    channel_id: serenity::ChannelId,
) -> bool {
    shared.overrides.fast_mode_channels.contains(&channel_id)
}

pub(in crate::services::discord) fn channel_codex_goals_enabled(
    shared: &Arc<SharedData>,
    channel_id: serenity::ChannelId,
) -> bool {
    shared.overrides.codex_goals_channels.contains(&channel_id)
}

pub(in crate::services::discord) fn clear_codex_goals_reset_pending_for_channel(
    shared: &Arc<SharedData>,
    channel_id: serenity::ChannelId,
) -> bool {
    shared
        .overrides
        .codex_goals_session_reset_pending
        .remove(&channel_id)
        .is_some()
}
