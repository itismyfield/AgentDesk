//! #3038 S1/S2/S3/S4/S5 — extracted field clusters of [`SharedData`].
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

use super::{
    ModelPickerPendingState, QueueExitVisibleCard, SharedData, placeholder_cleanup,
    placeholder_controller, placeholder_live_events,
};

/// #3038 cluster F — live-placeholder/status-panel state.
///
/// Groups the five contiguous fields that together own the user-visible live
/// placeholder card surface: cleanup tombstones, serialized placeholder edits,
/// the recent live-event/status-panel feed, and the two feature gates that
/// decide whether those events render into placeholder cards or separate
/// status panels. Field declarations, docs, and types moved verbatim from
/// `discord/mod.rs`; the members keep their original
/// `pub(in crate::services::discord)` visibility.
pub(in crate::services::discord) struct PlaceholderState {
    /// Last known placeholder cleanup outcome keyed by provider/channel/message.
    /// This local tombstone lets watcher finalization reason about cleanup
    /// even after the inflight file has already been cleared.
    pub(in crate::services::discord) placeholder_cleanup:
        Arc<placeholder_cleanup::PlaceholderCleanupRegistry>,
    /// Lifecycle FSM + edit coalescer for live-turn placeholder cards (#1255).
    /// Both the `tmux_handed_off` async-dispatch path and the new Monitor /
    /// `Bash run_in_background` live-turn path go through this controller so
    /// that concurrent edits to the same placeholder message_id serialize
    /// instead of racing.
    pub(in crate::services::discord) placeholder_controller:
        Arc<placeholder_controller::PlaceholderController>,
    /// Per-channel recent tool/system events rendered in Active placeholder
    /// cards when `placeholder.live_events_enabled` is enabled.
    pub(in crate::services::discord) placeholder_live_events:
        Arc<placeholder_live_events::PlaceholderLiveEvents>,
    pub(in crate::services::discord) placeholder_live_events_enabled: bool,
    pub(in crate::services::discord) status_panel_v2_enabled: bool,
    /// #3805 P2: two-message panel rollout gate copied from
    /// `placeholder.two_message_panel_enabled` at boot. Default OFF. PR-B wires
    /// the SINK read: when ON the bridge creates the status panel as a NEW
    /// message BELOW the answer (answer-first layout) via
    /// `turn_bridge::two_message_panel`; when OFF the single-message path is
    /// byte-identical. Later stages extend the same gate to re-anchor/recovery.
    pub(in crate::services::discord) two_message_panel_enabled: bool,
}

/// #3038 cluster G — runtime Discord HTTP cache.
///
/// Groups the gateway serenity context and bot-token fallback used by
/// non-gateway Discord REST paths. Field declarations, docs, and types moved
/// verbatim from `discord/mod.rs`; direct readers all stay inside `discord`.
pub(in crate::services::discord) struct RuntimeHttpCache {
    /// Cached serenity context for deferred queue drain (set once during ready event).
    pub(in crate::services::discord) cached_serenity_ctx: tokio::sync::OnceCell<serenity::Context>,
    /// Cached bot token for deferred queue drain.
    pub(in crate::services::discord) cached_bot_token: tokio::sync::OnceCell<String>,
}

/// #3479 cluster — policy runtime capability.
///
/// Groups the shared policy engine used by direct-dispatch finalization. The
/// field, doc, and type moved verbatim from `discord/mod.rs`; direct readers
/// all stay inside `discord` (`recovery_engine` +
/// `turn_bridge::completion_guard`) and reach it via `shared.policy.engine`.
pub(in crate::services::discord) struct PolicyRuntime {
    /// Shared policy engine for direct dispatch finalization.
    pub(in crate::services::discord) engine: Option<crate::engine::PolicyEngine>,
}

impl SharedData {
    /// Phase 5.2 of intake-node-routing (issue #2009): return an `Arc<Http>`
    /// that the response path (tmux watcher, placeholder updates, message
    /// edits) can use to call Discord. On the leader the gateway-attached
    /// runtime caches `cached_serenity_ctx`, and `ctx.http` is preferred so
    /// the Http instance shares the same application_id and connection
    /// pool the gateway already owns. On cluster-standby nodes the
    /// OnceCell is empty (no gateway runtime ever ran), so we fall back to
    /// a freshly constructed `serenity::http::Http` built from the bot
    /// token cached in `cached_bot_token`. Returns `None` only when both
    /// caches are empty — that means the runtime never reached the
    /// "token known" milestone in `run_bot()`, which today only happens
    /// before `bot_settings` finishes loading.
    ///
    /// Callers should treat `None` as a hard failure: they cannot post
    /// to Discord without an Http instance. The current call sites
    /// either propagate the failure (skip the work + warn) or have
    /// their own panic-on-None invariant tied to `cached_bot_token`
    /// being populated at `run_bot()` startup.
    pub(in crate::services::discord) fn serenity_http_or_token_fallback(
        &self,
    ) -> Option<Arc<serenity::http::Http>> {
        if let Some(ctx) = self.http.cached_serenity_ctx.get() {
            return Some(ctx.http.clone());
        }
        if let Some(token) = self.http.cached_bot_token.get() {
            return Some(Arc::new(serenity::http::Http::new(token)));
        }
        None
    }
}

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
    /// Per-channel selected cluster node instance for Discord intake routing.
    pub(in crate::services) node_overrides: dashmap::DashMap<ChannelId, String>,
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

/// #3479 Item 3 — dispatch intake/routing state.
///
/// Groups the three cohesive per-dispatch routing maps that together decide
/// whether an incoming bot message starts a new turn, is deduped, or is routed
/// into an existing dispatch thread / counter-model channel. Field declarations,
/// docs, and types moved verbatim from `discord/mod.rs`; the members keep their
/// original `pub(super)` (== `pub(in crate::services)`) visibility, and call
/// sites use `shared.dispatch.<original field name>`.
pub(in crate::services) struct DispatchRoutingState {
    /// Intake-level dedup cache: prevents the same message from starting two turns
    /// when duplicate bot dispatches arrive nearly simultaneously.
    /// Key: dedup key (dispatch_id or channel+author+text hash).
    /// Value: (first-seen Instant, was_thread_context).
    pub(in crate::services) intake_dedup: dashmap::DashMap<String, (std::time::Instant, bool)>,
    /// Maps parent channel → active dispatch thread channel.
    /// When a dispatch creates a thread, the parent is recorded here so that
    /// subsequent bot messages to the parent are queued instead of starting
    /// a parallel turn.  Cleared when the dispatch thread turn completes.
    pub(in crate::services) thread_parents: dashmap::DashMap<ChannelId, ChannelId>,
    /// Per-thread role/model override for cross-channel dispatch reuse.
    /// When a review dispatch reuses an implementation thread, this maps
    /// thread_channel_id → alt_channel_id so role_binding and model_for_turn
    /// resolve from the counter-model channel instead of the thread's parent.
    /// Cleared when the turn completes.
    pub(in crate::services) role_overrides: dashmap::DashMap<ChannelId, ChannelId>,
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

/// #3038 cluster E — restart-lifecycle state.
///
/// Groups the thirteen fields that together implement the
/// boot-to-shutdown lifecycle of one provider runtime: the per-channel
/// recovery markers and reconcile bookkeeping for the current boot, the
/// restart/shutdown drain flags and restart generation, and the
/// process-global active / finalizing / shutdown counters. Field
/// declarations, docs, and types moved verbatim from `discord/mod.rs`;
/// the members' original `pub(super)` annotations (declared in
/// `discord/mod.rs`, i.e. visible up to `crate::services`) are re-spelled
/// per-field as the semantically identical `pub(in crate::services)`
/// because `pub(super)` written *here* would shrink the scope to
/// `crate::services::discord` — a compile-time-only re-annotation with
/// zero runtime effect.
///
/// INVARIANT (#3038 S3, HANDOFF design): `global_active`,
/// `global_finalizing`, and `shutdown_remaining` are *injected* `Arc`
/// handles shared across every provider's `SharedData` (see
/// `RunBotContext` / `run_bot_build_shared_data`). They MUST stay
/// `Arc`-typed — flattening any of them into a plain atomic would
/// silently fork the process-global counter per provider and break the
/// deferred-restart / shutdown barrier arithmetic.
pub(in crate::services) struct RestartLifecycle {
    /// Per-channel in-flight turn recovery marker (restart resume in progress)
    /// Value is the Instant when recovery started, used for stale-recovery timeout.
    pub(in crate::services) recovering_channels: dashmap::DashMap<ChannelId, std::time::Instant>,
    /// Global shutdown flag — when set, watchers exit quietly via cancel path
    pub(in crate::services) shutting_down: Arc<std::sync::atomic::AtomicBool>,
    /// Provider-local intake tick activity. The deferred-restart poller fences
    /// admissions, waits for this handle to drain, then acknowledges its marker
    /// and consumes this provider's process-global shutdown-barrier slot.
    pub(in crate::services) intake_worker_lifecycle:
        crate::services::cluster::intake_worker::IntakeWorkerLifecycle,
    /// Number of turns currently in finalization phase (response sending + cleanup).
    /// Deferred restart must wait until this reaches 0 to avoid killing mid-send turns.
    pub(in crate::services) finalizing_turns: Arc<std::sync::atomic::AtomicUsize>,
    /// Current restart generation — incremented on each --restart-dcserver.
    /// Used to distinguish old (pre-restart) sessions from fresh ones.
    pub(in crate::services) current_generation: u64,
    /// Set when a `restart_pending` marker is detected. While true, the router
    /// queues new messages instead of starting new turns (drain mode).
    pub(in crate::services) restart_pending: Arc<std::sync::atomic::AtomicBool>,
    /// Set to true after startup reconciliation + recovery is complete (#122).
    /// Until true, the router queues all incoming messages.
    pub(in crate::services) reconcile_done: Arc<std::sync::atomic::AtomicBool>,
    /// Number of queued deferred idle-queue kickoffs waiting to run.
    pub(in crate::services) deferred_hook_backlog: std::sync::atomic::AtomicUsize,
    /// Per-channel live deferred idle-queue kickoff guard. A channel may have at
    /// most one fast/slow deferred drain task active; the task removes its entry
    /// when its backlog guard drops.
    pub(in crate::services) deferred_hook_channels:
        dashmap::DashMap<ChannelId, Arc<tokio::sync::Notify>>,
    /// When this provider started reconcile/recovery for the current boot.
    pub(in crate::services) recovery_started_at: std::time::Instant,
    /// Captured reconcile/recovery duration for the current boot in milliseconds.
    /// Remains 0 until reconcile completes, at which point it is frozen.
    pub(in crate::services) recovery_duration_ms: std::sync::atomic::AtomicU64,
    /// Process-global active turn counter shared across all providers.
    /// Deferred restart checks this instead of provider-local cancel_tokens.len().
    pub(in crate::services) global_active: Arc<std::sync::atomic::AtomicUsize>,
    /// Process-global finalizing turn counter shared across all providers.
    pub(in crate::services) global_finalizing: Arc<std::sync::atomic::AtomicUsize>,
    /// Number of providers still needing to complete shutdown.
    /// The last provider to decrement this to 0 calls `exit(0)`.
    pub(in crate::services) shutdown_remaining: Arc<std::sync::atomic::AtomicUsize>,
    /// Per-provider flag: ensures this provider decrements `shutdown_remaining` at most once,
    /// even if both the deferred restart poll loop and SIGTERM handler run.
    pub(in crate::services) shutdown_counted: std::sync::atomic::AtomicBool,
    /// Whether this provider already consumed its process-wide barrier slot.
    /// Cancellation restores only consumed slots, not merely acquired permits.
    pub(in crate::services) shutdown_slot_consumed: std::sync::atomic::AtomicBool,
}

#[cfg(test)]
mod restart_lifecycle_tests {
    //! #3038 S3 — post-extraction regression pin for the
    //! `check_deferred_restart` fresh-token branch. This branch needs
    //! `restart_pending == true` while `shutdown_counted == false`, a state
    //! only the (unseedable in-process) SIGTERM handler produces without
    //! writing fields directly, so the pre-move characterization suite
    //! (`runtime_bootstrap::restart_lifecycle_characterization_tests`) could
    //! not cover it through the function surface alone. Post-move tests may
    //! seed the group fields freely; together with the unmodified
    //! characterization tests this completes the check_deferred_restart
    //! decision matrix (the final-provider `exit(0)` arm stays untestable —
    //! `shutdown_remaining` is kept above 1 here).

    use std::sync::atomic::Ordering;

    const AGENTDESK_ROOT_DIR_ENV: &str = "AGENTDESK_ROOT_DIR";

    struct EnvGuard;

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe {
                std::env::remove_var(AGENTDESK_ROOT_DIR_ENV);
            }
        }
    }

    #[test]
    fn check_deferred_restart_fresh_token_decrements_once_without_exit() {
        // #3167 B3: crate-wide env serialization (no local Mutex).
        let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var(AGENTDESK_ROOT_DIR_ENV, tmp.path().to_str().unwrap());
        }
        let _env_guard = EnvGuard;

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let shared = super::super::make_shared_data_for_tests();
            // Two providers outstanding → the fetch_sub observes 2 (!= 1)
            // and returns without reaching the final-provider exit arm.
            shared.restart.shutdown_remaining.store(2, Ordering::SeqCst);
            shared.restart.restart_pending.store(true, Ordering::SeqCst);

            super::super::check_deferred_restart(&shared);
            assert!(
                shared.restart.shutdown_counted.load(Ordering::Acquire),
                "fresh token must be consumed by the CAS guard"
            );
            assert_eq!(
                shared.restart.shutdown_remaining.load(Ordering::Acquire),
                1,
                "fresh-token branch must decrement shutdown_remaining exactly once"
            );

            // Second poll tick: the consumed token short-circuits before the
            // barrier — remaining must NOT reach the exit threshold again.
            super::super::check_deferred_restart(&shared);
            assert_eq!(
                shared.restart.shutdown_remaining.load(Ordering::Acquire),
                1,
                "consumed token must block any further decrement"
            );
        });
    }
}
