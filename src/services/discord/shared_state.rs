//! #3038 — named sub-structs that group cohesive `SharedData` fields together
//! with the inherent `impl SharedData` methods that exclusively own those
//! fields, keeping `SharedData`'s public surface and every call site
//! unchanged.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use poise::serenity_prelude as serenity;
use poise::serenity_prelude::{ChannelId, MessageId};

use crate::services::provider::ProviderKind;

use super::{
    ModelPickerPendingState, QueueExitVisibleCard, SharedData, placeholder_cleanup,
    placeholder_controller, placeholder_live_events,
};

/// #3038 cluster F — live-placeholder/status-panel state: cleanup
/// tombstones, serialized placeholder edits, the recent live-event/
/// status-panel feed, and the feature gates deciding whether those events
/// render into placeholder cards or separate status panels.
pub(in crate::services::discord) struct PlaceholderState {
    /// Last known placeholder cleanup outcome keyed by provider/channel/message.
    /// This local tombstone lets watcher finalization reason about cleanup
    /// even after the inflight file has already been cleared.
    pub(in crate::services::discord) placeholder_cleanup:
        Arc<placeholder_cleanup::PlaceholderCleanupRegistry>,
    /// Lifecycle FSM + edit coalescer for live-turn placeholder cards (#1255).
    /// Serializes concurrent edits to the same placeholder message_id across
    /// both the async-dispatch and live-turn call paths.
    pub(in crate::services::discord) placeholder_controller:
        Arc<placeholder_controller::PlaceholderController>,
    /// Per-channel recent tool/system events rendered in Active placeholder
    /// cards when `placeholder.live_events_enabled` is enabled.
    pub(in crate::services::discord) placeholder_live_events:
        Arc<placeholder_live_events::PlaceholderLiveEvents>,
    pub(in crate::services::discord) placeholder_live_events_enabled: bool,
    pub(in crate::services::discord) status_panel_v2_enabled: bool,
    /// Two-message panel rollout gate (default OFF, #3805). When ON, the
    /// status panel renders as a separate message below the answer instead
    /// of the single-message layout.
    pub(in crate::services::discord) two_message_panel_enabled: bool,
}

/// #3038 cluster G — runtime Discord HTTP cache: gateway serenity context and
/// bot-token fallback used by non-gateway Discord REST paths.
pub(in crate::services::discord) struct RuntimeHttpCache {
    /// Cached serenity context for deferred queue drain (set once during ready event).
    pub(in crate::services::discord) cached_serenity_ctx: tokio::sync::OnceCell<serenity::Context>,
    /// Cached bot token for deferred queue drain.
    pub(in crate::services::discord) cached_bot_token: tokio::sync::OnceCell<String>,
}

/// #3479 — shared policy engine used by direct-dispatch finalization
/// (`recovery_engine`, `turn_bridge::completion_guard`), reached via
/// `shared.policy.engine`.
pub(in crate::services::discord) struct PolicyRuntime {
    pub(in crate::services::discord) engine: Option<crate::engine::PolicyEngine>,
}

impl SharedData {
    /// Returns an `Arc<Http>` for posting to Discord outside the gateway
    /// event loop (tmux watcher, placeholder updates, message edits).
    ///
    /// Prefers `ctx.http` from the cached gateway context so it shares the
    /// gateway's application_id and connection pool; on cluster-standby nodes
    /// (no gateway ever ran) falls back to a fresh `Http` built from the
    /// cached bot token. Returns `None` only if neither cache is populated —
    /// i.e. before `bot_settings` finishes loading during `run_bot()`.
    ///
    /// Callers must treat `None` as a hard failure: they cannot post to
    /// Discord without an `Http` instance.
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

/// #3038 cluster C — queued-placeholder handoff state: the `📬 메시지 대기 중`
/// card mapping, its queue-exit clear sidecar mirror, and the per-channel
/// persistence mutexes serializing ownership-coupled mutations (see field
/// docs for the lock-span invariant).
pub(in crate::services::discord) struct QueuedPlaceholderState {
    /// Per-channel mapping from a mailbox-queued user message id to the
    /// Discord placeholder message id showing the `📬 메시지 대기 중` card.
    /// Populated on start-turn race loss; consumed on dequeue so the card
    /// transitions to `Active` instead of leaking a duplicate.
    pub(in crate::services::discord) queued_placeholders:
        dashmap::DashMap<(ChannelId, MessageId), MessageId>,
    /// Queue-exit placeholder cards removed from `queued_placeholders` while
    /// `cached_serenity_ctx` was not ready. Mirrored to a sidecar so ready-time
    /// drain can delete the stale `📬` cards once the HTTP client exists.
    pub(in crate::services::discord) queue_exit_placeholder_clears:
        dashmap::DashMap<(ChannelId, MessageId), MessageId>,
    /// Per-channel mutex guarding `queued_placeholders` snapshot writes and
    /// any Discord PATCH asserting queued ownership, so a stale snapshot can
    /// never overwrite a newer mapping and the same Discord message is never
    /// written by both the queued-placeholder render and the
    /// dispatch/queue-exit cleanup paths.
    ///
    /// Invariant: hold this lock across the ownership recheck, the PATCH,
    /// and the persistence write that follows. Only ownership-coupled
    /// mutations are serialized — the map fast-path stays lock-free. Async
    /// so it can be held across `.await` points.
    pub(in crate::services::discord) queued_placeholders_persist_locks:
        dashmap::DashMap<ChannelId, Arc<tokio::sync::Mutex<()>>>,
}

/// #3038 cluster C — inherent methods that exclusively own
/// [`QueuedPlaceholderState`].
impl SharedData {
    /// Fetch (or create) the per-channel persistence mutex. Stored as
    /// `Arc<tokio::sync::Mutex<()>>` so callers can clone it out of the
    /// `DashMap` and release the shard lock before acquiring the channel
    /// mutex, avoiding a deadlock between DashMap shard locks and the
    /// persistence mutex. `tokio::sync::Mutex` so the lock can be held across
    /// `.await` points (e.g. the `ensure_queued` Discord PATCH in the
    /// race-loss render path).
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

    /// Insert variant that assumes the caller already holds the per-channel
    /// persistence mutex, so the race-loss render path can span the ownership
    /// recheck, PATCH, and persistence write under one lock acquisition.
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

    /// Write-through remove for the `queued_placeholders` mapping. Returns
    /// the removed placeholder message id, if any, under the per-channel
    /// persistence mutex.
    pub(super) async fn remove_queued_placeholder(
        &self,
        channel_id: ChannelId,
        user_msg_id: MessageId,
    ) -> Option<MessageId> {
        let persist_lock = self.queued_placeholders_persist_lock(channel_id);
        let _persist_guard = persist_lock.lock().await;
        self.remove_queued_placeholder_locked(channel_id, user_msg_id)
    }

    /// Remove variant that assumes the caller already holds the per-channel
    /// persistence mutex, for the race-loss render path's rollback branch.
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

    /// Atomic ownership recheck for the race-loss render path: the active
    /// turn may finish concurrently and consume our mapping before we get
    /// here, promoting our placeholder to the live response card. Returns
    /// `true` only when the mapping still points at our exact
    /// `placeholder_msg_id`; callers MUST exit without touching Discord
    /// state if this returns `false`.
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

    /// Enqueues a single deferred placeholder-clear when an inline
    /// `delete_message` from a non-queue-exit path (e.g.
    /// `render_visible_queued_ack`) fails, so the retry survives a restart
    /// and is drained by `drain_pending_queue_exit_placeholder_clears`.
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

/// #3038 cluster D — session-scoped override / reset-pending state:
/// per-channel model override, native fast mode, and Codex goals, plus the
/// session-reset bookkeeping they drive (the per-cause
/// `*_session_reset_pending` sets, the aggregated `session_reset_pending` set
/// kept in sync by `commands::config::sync_session_reset_pending`, and the
/// staged `/model` picker selections).
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

/// #3479 — per-dispatch routing maps deciding whether an incoming bot message
/// starts a new turn, is deduped, or is routed into an existing dispatch
/// thread / counter-model channel.
pub(in crate::services) struct DispatchRoutingState {
    /// Intake-level dedup cache: prevents the same message from starting two turns
    /// when duplicate bot dispatches arrive nearly simultaneously.
    /// Key: dedup key (dispatch_id or channel+author+text hash).
    /// Value: (first-seen Instant, was_thread_context).
    pub(in crate::services) intake_dedup: dashmap::DashMap<String, (std::time::Instant, bool)>,
    /// Maps parent channel → active dispatch thread channel, so subsequent
    /// bot messages to the parent are queued instead of starting a parallel
    /// turn. Cleared when the dispatch thread turn completes.
    pub(in crate::services) thread_parents: dashmap::DashMap<ChannelId, ChannelId>,
    /// Per-thread role/model override for cross-channel dispatch reuse: maps
    /// thread_channel_id → alt_channel_id so role_binding and model_for_turn
    /// resolve from the counter-model channel instead of the thread's parent.
    /// Cleared when the turn completes.
    pub(in crate::services) role_overrides: dashmap::DashMap<ChannelId, ChannelId>,
}

// Free-function helpers over `SessionOverrideState`. The settings-coupled
// writers (`update_channel_fast_mode` / `update_channel_codex_goals` /
// `update_channel_model_override`) intentionally stay in config.rs since they
// mix this cluster with `settings` persistence (`save_bot_settings`).

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

/// #3038 cluster E — restart-lifecycle state: per-channel recovery/reconcile
/// bookkeeping for the current boot, restart/shutdown drain flags, and the
/// process-global active/finalizing/shutdown counters.
///
/// INVARIANT: `global_active`, `global_finalizing`, and `shutdown_remaining`
/// are *injected* `Arc` handles shared across every provider's `SharedData`
/// (see `RunBotContext`). They MUST stay `Arc`-typed — flattening any into a
/// plain atomic would silently fork the process-global counter per provider
/// and break the deferred-restart / shutdown barrier arithmetic.
pub(in crate::services) struct RestartLifecycle {
    /// Per-channel restart-resume marker: Instant when recovery started, for
    /// stale-recovery timeout.
    pub(in crate::services) recovering_channels: dashmap::DashMap<ChannelId, std::time::Instant>,
    /// Global shutdown flag — when set, watchers exit quietly via cancel path
    pub(in crate::services) shutting_down: Arc<std::sync::atomic::AtomicBool>,
    /// Provider-local intake tick activity; the deferred-restart poller uses
    /// it to fence admissions before consuming this provider's shutdown slot.
    pub(in crate::services) intake_worker_lifecycle:
        crate::services::cluster::intake_worker::IntakeWorkerLifecycle,
    /// Number of turns currently in finalization phase (response sending + cleanup).
    /// Deferred restart must wait until this reaches 0 to avoid killing mid-send turns.
    pub(in crate::services) finalizing_turns: Arc<std::sync::atomic::AtomicUsize>,
    /// Immutable process epoch allocated once when this dcserver boots.
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
    /// Per-channel deferred idle-queue kickoff guard: one drain task active
    /// per channel, removed when its backlog guard drops.
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

/// #5485 — read-only view of the process-global shutdown flag for workers
/// that only *observe* shutdown (intake poll loop, voice
/// sensitivity/progress/rejoin workers), so they cannot flip the flag for the
/// whole process.
///
/// [`ShutdownReader::load`] and `Clone` are the entire surface — no `Deref`,
/// `AsRef`, `From<Arc<_>>`, `store`, or `swap`. Readers see the same
/// allocation the writer stores into; cloning yields another reader, not the
/// wrapped `Arc`.
#[derive(Clone)]
pub(in crate::services) struct ShutdownReader(Arc<std::sync::atomic::AtomicBool>);

impl ShutdownReader {
    /// Read the live shutdown flag. `order` stays the caller's choice so the
    /// migrated observers keep the orderings they already had.
    pub(in crate::services) fn load(&self, order: Ordering) -> bool {
        self.0.load(order)
    }
}

impl RestartLifecycle {
    /// Hand out a read-only view of the shutdown flag. The only constructor
    /// of [`ShutdownReader`]: its wrapped handle is a private tuple field, so
    /// nothing outside can forge observer capability or unwrap it into a
    /// writer.
    pub(in crate::services) fn shutdown_reader(&self) -> ShutdownReader {
        ShutdownReader(self.shutting_down.clone())
    }

    /// Deferred-restart poller admission-fence publish (`begin_deferred_restart`).
    pub(in crate::services::discord) fn legacy_deferred_begin(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
    }

    /// Deferred-restart poller health-visible ack (`prepare_deferred_restart`).
    pub(in crate::services::discord) fn legacy_deferred_ack(&self) {
        self.restart_pending.store(true, Ordering::SeqCst);
    }

    /// Deferred-restart rollback: clears in the original order, shutdown flag
    /// first, acknowledgement second.
    pub(in crate::services::discord) fn legacy_deferred_rollback(&self) {
        self.shutting_down.store(false, Ordering::SeqCst);
        self.restart_pending.store(false, Ordering::SeqCst);
    }

    /// Standby promotion fence, applied to every provider runtime.
    pub(in crate::services::discord) fn legacy_promotion_fence(&self) {
        self.restart_pending.store(true, Ordering::SeqCst);
    }

    /// Standby promotion unfence (`gateway_lease_recovery::unfence_runtimes`).
    pub(in crate::services::discord) fn legacy_promotion_unfence(&self) {
        self.restart_pending.store(false, Ordering::SeqCst);
    }

    /// Gateway lease loss self-fence: shuts down and leaves the restart
    /// request behind so launchd brings the process back.
    pub(in crate::services::discord) fn legacy_lease_lost(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
        self.restart_pending.store(true, Ordering::SeqCst);
    }

    /// SIGTERM handler.
    pub(in crate::services::discord) fn legacy_sigterm(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
        // Drain mode: no new queue/checkpoint mutations during shutdown.
        self.restart_pending.store(true, Ordering::SeqCst);
    }
}

#[cfg(test)]
pub(in crate::services::discord) mod restart_lifecycle_tests {
    //! Regression pin for the `check_deferred_restart` fresh-token branch: it
    //! needs `restart_pending == true` while `shutdown_counted == false`, a
    //! state only the SIGTERM handler produces without direct field writes,
    //! so it can't be driven through the public function surface alone (the
    //! final-provider `exit(0)` arm stays untestable — `shutdown_remaining`
    //! is kept above 1 here).

    use std::sync::atomic::Ordering;

    const AGENTDESK_ROOT_DIR_ENV: &str = "AGENTDESK_ROOT_DIR";

    /// #5485 S2a: stop a test-spawned voice worker through owner state — the
    /// PCM harness holds a `ShutdownReader`, never a writable `Arc` of its own.
    pub(in crate::services::discord) fn stop_pcm_worker_for_test(r: &super::RestartLifecycle) {
        r.shutting_down.store(true, Ordering::Relaxed);
    }

    /// #5485 S2a A1: `shutdown_reader()` is a view of the live flag, never a
    /// snapshot, and every clone shares that one allocation.
    #[tokio::test]
    async fn shutdown_reader_observes_live_writer_and_clone() {
        let shared = super::super::make_shared_data_for_tests();
        let reader = shared.restart.shutdown_reader();
        let handles = [reader.clone(), reader, shared.restart.shutdown_reader()];
        let flag = &shared.restart.shutting_down;
        for expected in [false, true, false, true, false] {
            flag.store(expected, Ordering::SeqCst);
            for (index, handle) in handles.iter().enumerate() {
                let seen = handle.load(Ordering::Acquire);
                assert_eq!(seen, expected, "handle {index} must observe {expected}");
            }
        }
    }

    /// #5485 S2a A2: each legacy adapter writes exactly the flags its original
    /// call site wrote, leaves the other one alone, and is idempotent.
    #[tokio::test]
    async fn legacy_actor_store_matrix_preserves_unwritten_flags() {
        // `stores` is "<shutting_down><restart_pending>" for the flags the
        // original call site wrote: '1' true, '0' false, '-' left untouched.
        type Row = (&'static str, fn(&super::RestartLifecycle), &'static str);
        let rows: [Row; 7] = [
            ("begin", |r| r.legacy_deferred_begin(), "1-"),
            ("ack", |r| r.legacy_deferred_ack(), "-1"),
            ("rollback", |r| r.legacy_deferred_rollback(), "00"),
            ("fence", |r| r.legacy_promotion_fence(), "-1"),
            ("unfence", |r| r.legacy_promotion_unfence(), "-0"),
            ("lease_lost", |r| r.legacy_lease_lost(), "11"),
            ("sigterm", |r| r.legacy_sigterm(), "11"),
        ];
        let want = |spec: &str, index: usize, seed: bool| match spec.as_bytes()[index] {
            b'1' => true,
            b'0' => false,
            _ => seed,
        };

        let shared = super::super::make_shared_data_for_tests();
        for (name, apply, stores) in rows {
            for seed in [false, true] {
                shared.restart.shutting_down.store(seed, Ordering::SeqCst);
                shared.restart.restart_pending.store(seed, Ordering::SeqCst);
                let expected = (want(stores, 0, seed), want(stores, 1, seed));
                for repeat in 0..2 {
                    apply(&shared.restart);
                    let shutdown = shared.restart.shutting_down.load(Ordering::SeqCst);
                    let pending = shared.restart.restart_pending.load(Ordering::SeqCst);
                    let label = format!("{name} seed={seed} repeat={repeat}");
                    assert_eq!((shutdown, pending), expected, "{label}");
                }
            }
        }

        // Both stores of a pair land before the adapter returns, so the matrix
        // cannot see their order. Pin it at the source instead.
        let source = include_str!("shared_state.rs");
        for name in "legacy_deferred_rollback legacy_lease_lost legacy_sigterm".split(' ') {
            let start = source.find(&format!("fn {name}(&self) {{")).expect("fn");
            let body = &source[start..start + source[start..].find("\n    }\n").expect("end")];
            let shutdown_at = body.find("self.shutting_down.store").expect("shutdown");
            let pending_at = body.find("self.restart_pending.store").expect("pending");
            assert!(shutdown_at < pending_at, "{name} stores shutdown first");
        }
    }

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
