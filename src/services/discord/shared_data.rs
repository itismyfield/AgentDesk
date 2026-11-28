//! The Discord bot's shared runtime state and its test fixtures.

use super::{
    Arc, CHANNEL_ROSTER_MAX_USERS, CatchUpRetryState, ChannelId, ChannelMailboxHandle,
    DiscordBotSettings, DispatchRoutingState, EngineRuntime, Ordering, PlaceholderState,
    QueuedPlaceholderState, RestartLifecycle, RuntimeHttpCache, SessionOverrideState,
    TmuxRelayCoord, TmuxWatcherRegistry, UserId, UserRecord, answer_flush_barrier,
    delivery_lease_cell, health, inflight, readopted_mailbox_ledger, shared_state,
    turn_completion_events, turn_finalizer, turn_view_reconciler,
};
#[cfg(test)]
use super::{
    ChannelMailboxRegistry, CoreState, PolicyRuntime, ProviderKind, placeholder_cleanup,
    placeholder_controller, placeholder_live_events, router,
};

/// Shared state for the Discord bot — split into independently-lockable groups.
///
/// Phase 2-pre.3 of intake-node-routing: widened from `pub(super)` to
/// `pub(crate)` so the public worker entry point `execute_intake_turn_core`
/// can accept `&Arc<SharedData>` from a non-`services::discord` caller
/// (Phase 3 worker polling loop).
pub(crate) struct SharedData {
    pub(in crate::services) engine: Arc<EngineRuntime>,
    /// Bot settings — mostly reads, rare writes
    pub(in crate::services) settings: tokio::sync::RwLock<DiscordBotSettings>,
    /// Per-channel timestamps of the last Discord API call (for rate limiting)
    pub(in crate::services) api_timestamps: dashmap::DashMap<ChannelId, tokio::time::Instant>,
    /// Active tmux output watchers for terminal→Discord relay.
    pub(in crate::services) tmux_watchers: TmuxWatcherRegistry,
    /// Per-channel relay coordination state. Unlike `tmux_watchers`, this
    /// entry is preserved across watcher-handle replacements so an outgoing
    /// watcher and an incoming watcher share the same emission-slot atomic
    /// and confirmed-offset watermark. See `TmuxRelayCoord`.
    pub(in crate::services) tmux_relay_coords:
        Arc<dashmap::DashMap<ChannelId, Arc<TmuxRelayCoord>>>,
    /// #3038 cluster F — live-placeholder/status-panel state: cleanup tombstones,
    /// edit controller, live-event feed, and live-event/status-panel gates.
    /// Field docs live on `shared_state::PlaceholderState`; call sites use `shared.ui.*`.
    pub(in crate::services::discord) ui: PlaceholderState,
    /// #3038 cluster C — queued-placeholder handoff state (the
    /// `queued_placeholders` mapping, the `queue_exit_placeholder_clears`
    /// sidecar mirror, and the per-channel `queued_placeholders_persist_locks`).
    /// Field declarations, docs, and the round-5 P2 lock-span invariant live on
    /// `shared_state::QueuedPlaceholderState`; access stays via the inherent
    /// `SharedData` methods that own this cluster (no field-path changes at call
    /// sites).
    pub(in crate::services::discord) queued: QueuedPlaceholderState,
    /// #3082 part B — per-channel answer-flush barrier. Set while a multi-chunk
    /// final answer is being delivered (>1 Discord chunk) by
    /// `send_long_message_raw*`, so a queued-turn notice POST
    /// (`send_intake_placeholder`) does NOT interleave between the answer's
    /// chunks. The queued-card POST path waits on this gate with a BOUNDED
    /// timeout and proceeds regardless once it elapses, so a stuck/errored
    /// flush can never permanently suppress the queued card. The gate is
    /// cleared by an RAII guard on every exit path (success, error, panic) so
    /// it never strands set.
    pub(in crate::services::discord) answer_flush_barrier:
        Arc<answer_flush_barrier::AnswerFlushBarrier>,
    /// #3038 cluster E — restart-lifecycle state (the per-channel recovery
    /// markers and reconcile bookkeeping for the current boot, the
    /// restart/shutdown drain flags and restart generation, and the
    /// process-global active/finalizing/shutdown counters shared across all
    /// providers as injected `Arc` handles). Field declarations and docs live
    /// on `shared_state::RestartLifecycle`; call sites access the members via
    /// `shared.restart.<original field name>`.
    pub(in crate::services) restart: RestartLifecycle,
    /// EPIC #3016 — single-authority turn finalizer. The only code path that
    /// owns the four finalize side-effects (inflight clear, mailbox
    /// cancel_token release, `global_active` decrement, trailing terminal
    /// side-effects) as an atomic, exactly-once unit. Bridge/watcher terminals
    /// submit terminal events here instead of finalizing inline.
    pub(in crate::services::discord) turn_finalizer: Arc<turn_finalizer::TurnFinalizer>,
    /// #3479 Item 3 — dispatch intake/routing state: the intake dedup cache, the
    /// parent→dispatch-thread map, and the per-thread role/model override map.
    /// Field declarations and docs live on `shared_state::DispatchRoutingState`;
    /// call sites access the members via `shared.dispatch.<original field name>`.
    pub(in crate::services) dispatch: DispatchRoutingState,
    /// Set to true after Discord gateway ready event fires.
    pub(in crate::services) bot_connected: std::sync::atomic::AtomicBool,
    /// #3038 cluster D — session-scoped override / reset-pending state (the
    /// `model_overrides` map, the fast-mode / Codex-goals enablement sets, the
    /// per-cause `*_session_reset_pending` sets plus the aggregated
    /// `session_reset_pending` set, and the staged `model_picker_pending`
    /// selections). Field declarations and docs live on
    /// `shared_state::SessionOverrideState`; call sites access the members via
    /// `shared.overrides.<original field name>`.
    pub(in crate::services) overrides: SessionOverrideState,
    /// Per-channel last processed message ID — used for startup catch-up polling.
    pub(in crate::services) last_message_ids: dashmap::DashMap<ChannelId, u64>,
    /// Channels where catch-up stopped because the intervention queue was at
    /// capacity. Carries the pinned `after` checkpoint + bounded fetch-failure
    /// count for the next in-process pass, independent of live message
    /// checkpoints that may advance while the queued backlog drains.
    pub(in crate::services) catch_up_retry_pending: dashmap::DashMap<ChannelId, CatchUpRetryState>,
    /// Serializes sweeps and retains retries handed to a busy channel.
    pub(in crate::services) catch_up_scan_in_progress:
        dashmap::DashMap<ChannelId, (bool, Option<CatchUpRetryState>)>,
    /// Per-channel known speakers collected lazily from incoming messages.
    pub(in crate::services) channel_rosters: dashmap::DashMap<ChannelId, Vec<UserRecord>>,
    /// #3038 cluster G — cached Discord HTTP runtime state: the gateway
    /// serenity context plus the bot-token fallback for standby REST sends.
    /// Field docs live on `shared_state::RuntimeHttpCache`; call sites use
    /// `shared.http.*` for direct cache reads and keep
    /// `shared.serenity_http_or_token_fallback()` for the accessor.
    pub(in crate::services::discord) http: RuntimeHttpCache,
    /// SHA-256 hash of the bot token — used to namespace the pending-queue directory
    /// so that multiple bots sharing the same runtime root cannot steal each other's queues.
    pub(in crate::services) token_hash: String,
    /// Weak reference to the process-wide health registry so turn handlers can
    /// reach dedicated Discord bot HTTP clients without creating an Arc cycle.
    pub(in crate::services) health_registry: std::sync::Weak<health::HealthRegistry>,
    /// Set of registered slash command names (populated at framework setup).
    /// Used by the router to distinguish known slash commands from arbitrary
    /// `/`-prefixed user text that should fall through to the AI provider.
    pub(in crate::services) known_slash_commands:
        tokio::sync::OnceCell<std::collections::HashSet<String>>,
    /// #2448: process-wide broadcast of explicit inflight-lifecycle signals.
    /// turn_bridge's `CompletionGuard` publishes `InflightSignal::Completed`
    /// on terminal drop so subscribers (currently `run_standby_relay`) can
    /// exit immediately instead of polling against a 15min wall-clock
    /// timeout. Capacity is intentionally generous so a brief listener
    /// hiccup yields `RecvError::Lagged` rather than dropped channels.
    pub(in crate::services::discord) inflight_signals:
        tokio::sync::broadcast::Sender<inflight::InflightSignal>,
    /// #4048 S3: canonical finalize-completion edge bus for idle-queue drain.
    /// The TurnFinalizer publishes after the mailbox token release point, so this
    /// is not coupled to visible status-panel/footer rendering.
    pub(in crate::services::discord) turn_completion_events:
        tokio::sync::broadcast::Sender<turn_completion_events::TurnCompletionEvent>,
    pub(in crate::services::discord) turn_view_reconciler: turn_view_reconciler::TurnViewReconciler,
    pub(super) readopted_mailbox_ledger: readopted_mailbox_ledger::ReadoptedMailboxLedger, // #4370
    pub(in crate::services::discord) router_ports: shared_state::RouterPorts,
}

impl std::ops::Deref for SharedData {
    type Target = EngineRuntime;

    fn deref(&self) -> &Self::Target {
        &self.engine
    }
}

// Fixtures customize immutable inputs before sharing their sole runtime owner.
#[cfg(test)]
impl std::ops::DerefMut for SharedData {
    fn deref_mut(&mut self) -> &mut Self::Target {
        Arc::get_mut(&mut self.engine).expect("fixture runtime must have a single owner")
    }
}

/// The bot token hash, for tests outside `services`.
#[cfg(test)]
impl SharedData {
    #[cfg(unix)]
    pub(crate) fn set_token_hash_for_test(&mut self, hash: &str) {
        self.token_hash = hash.to_string();
    }

    #[cfg(unix)]
    pub(crate) fn token_hash_for_test(&self) -> &str {
        &self.token_hash
    }
}

impl SharedData {
    pub(in crate::services) fn has_runtime_storage(&self) -> bool {
        self.pg_pool.is_some()
    }

    pub(super) fn mailbox(&self, channel_id: ChannelId) -> ChannelMailboxHandle {
        self.mailboxes.handle(channel_id)
    }

    /// Messages a live mailbox holds as its active, dispatching or queued input, in any channel.
    /// Reads existing actors only, so it never spawns a mailbox for an idle channel.
    pub(super) async fn mailbox_held_messages(&self) -> std::collections::HashSet<u64> {
        let mut held = std::collections::HashSet::new();
        for (_, snapshot) in self.mailboxes.snapshot_all().await {
            let queued = snapshot.intervention_queue.iter().flat_map(|item| {
                std::iter::once(item.message_id).chain(item.source_message_ids.iter().copied())
            });
            let ids = (snapshot.active_user_message_id.into_iter())
                .chain(snapshot.pending_user_dispatch)
                .chain(queued);
            held.extend(ids.map(|message| message.get()));
        }
        held
    }

    pub(super) fn health_registry(&self) -> Option<Arc<health::HealthRegistry>> {
        self.health_registry.upgrade()
    }

    /// #1031: snapshot every active mailbox for the idle-detector pass.
    /// Reduces the per-channel snapshot to the minimal fields the detector
    /// actually consumes — `cancel_token` / `recovery_started_at` /
    /// `turn_started_at` — so the detector module never imports the private
    /// mailbox types.
    pub(in crate::services) async fn mailbox_snapshots_for_idle_detector(
        &self,
    ) -> Vec<(ChannelId, bool, bool, Option<chrono::DateTime<chrono::Utc>>)> {
        self.mailboxes
            .snapshot_all()
            .await
            .into_iter()
            .map(|(channel_id, snapshot)| {
                (
                    channel_id,
                    snapshot.cancel_token.is_some(),
                    snapshot.recovery_started_at.is_some(),
                    snapshot.turn_started_at,
                )
            })
            .collect()
    }

    /// #1031: borrow the same `health_registry()` Arc the rest of the discord
    /// runtime uses. Exposed under a distinct name so the idle detector does
    /// not depend on the un-public method.
    pub(in crate::services) fn health_registry_for_idle_detector(
        &self,
    ) -> Option<Arc<health::HealthRegistry>> {
        self.health_registry()
    }

    /// Fetch the per-channel relay coordination state, creating a fresh one
    /// on first access. Returned Arc is shared across all watcher instances
    /// (outgoing and incoming) for the channel, so they coordinate relay
    /// emission without duplicate-sending the same tmux range.
    pub(in crate::services) fn tmux_relay_coord(
        &self,
        channel_id: ChannelId,
    ) -> Arc<TmuxRelayCoord> {
        delivery_lease_cell::assert_payload_not_held();
        self.tmux_relay_coords
            .entry(channel_id)
            .or_insert_with(|| Arc::new(TmuxRelayCoord::new(channel_id)))
            .clone()
    }

    /// #3041 P1-1 (B3): reclaim any delivery lease whose acquire deadline has
    /// elapsed, force-returning a dead holder's cell to `Unleased` so a later
    /// legitimate acquire can win. Identity-agnostic (deadline-only) by design —
    /// a `Committed` lease is never reclaimed here (it awaits an explicit holder
    /// release). Driven from the finalizer's reconcile tick. Returns the number
    /// of cells reclaimed (for observability/tests).
    pub(in crate::services::discord) fn reclaim_expired_delivery_leases(
        &self,
        now_ms: u64,
    ) -> usize {
        let mut reclaimed = 0usize;
        for coord in self.tmux_relay_coords.iter() {
            if coord.value().delivery_lease.reclaim_if_expired(now_ms) {
                reclaimed += 1;
            }
        }
        reclaimed
    }

    /// #3017 single output-offset authority for the relay-dedup paths — a
    /// read-only snapshot of the authoritative committed relayed offset.
    ///
    /// The per-channel `confirmed_end_offset` is the ONE authoritative "JSONL
    /// byte offset (exclusive) past which output has already been relayed to
    /// Discord". The single committer is the tmux watcher (the primary relay)
    /// via `advance_watcher_confirmed_end`. The inflight-less wake /
    /// idle-background / monitor relay paths (idle-JSONL relay, session-bound
    /// sink) CONSULT this BEFORE relaying so a byte-range the watcher already
    /// delivered is relayed EXACTLY ONCE regardless of which actor observes it
    /// first (the E-13 dedup invariant). They do NOT claim it themselves —
    /// claiming on a secondary path could suppress the primary watcher's own
    /// delivery on a failed forward and drop the response. It is the
    /// cross-actor generalization of the watcher's process-local
    /// `last_relayed_offset`.
    pub(in crate::services) fn committed_relay_offset(&self, channel_id: ChannelId) -> u64 {
        self.tmux_relay_coord(channel_id)
            .confirmed_end_offset
            .load(Ordering::Acquire)
    }

    /// #4181: `true` while some watcher is actively emitting a relay for this
    /// channel — the `relay_slot` holds the in-progress emission's
    /// `data_start_offset` (non-zero). A single relay POST can be in-flight for
    /// longer than the stall grace under extreme rate-limiting, freezing the
    /// committed offset without the turn being stalled. Redrive consults this so
    /// it does not re-drive an already-in-flight emission (a duplicate, not a
    /// loss).
    pub(in crate::services) fn relay_emission_in_flight(&self, channel_id: ChannelId) -> bool {
        self.tmux_relay_coord(channel_id)
            .relay_slot
            .load(Ordering::Acquire)
            != 0
    }

    /// Record a recovery/reattach watcher spawn and purge the channel footer so the
    /// dead prior generation's task/subagent slots don't linger as zombies (#3436, #964).
    pub(in crate::services) fn record_tmux_watcher_reconnect(&self, channel_id: ChannelId) {
        self.tmux_relay_coord(channel_id)
            .reconnect_count
            .fetch_add(1, Ordering::AcqRel);
        self.ui.placeholder_live_events.clear_channel(channel_id);
    }

    pub(in crate::services) fn record_channel_speaker(
        &self,
        channel_id: ChannelId,
        user_id: UserId,
        user_name: &str,
        is_dm: bool,
    ) {
        let record = UserRecord::new(user_id, user_name);
        if is_dm {
            self.channel_rosters.insert(channel_id, vec![record]);
            return;
        }

        match self.channel_rosters.entry(channel_id) {
            dashmap::mapref::entry::Entry::Occupied(mut entry) => {
                let roster = entry.get_mut();
                if let Some(existing) = roster.iter_mut().find(|user| user.id == user_id) {
                    existing.name = record.name;
                } else if roster.len() < CHANNEL_ROSTER_MAX_USERS {
                    roster.push(record);
                }
            }
            dashmap::mapref::entry::Entry::Vacant(entry) => {
                entry.insert(vec![record]);
            }
        }
    }

    pub(in crate::services) fn channel_roster(
        &self,
        channel_id: ChannelId,
        fallback_user_id: UserId,
        fallback_user_name: &str,
    ) -> Vec<UserRecord> {
        self.channel_rosters
            .get(&channel_id)
            .map(|entry| entry.clone())
            .filter(|users| !users.is_empty())
            .unwrap_or_else(|| vec![UserRecord::new(fallback_user_id, fallback_user_name)])
    }

    // #3038 S1: the queued-placeholder cluster methods
    // (queued_placeholders_persist_lock, insert/remove_queued_placeholder*,
    // queued_placeholder_still_owned, add/remove_pending_queue_exit_placeholder_clear*,
    // pending_queue_exit_placeholder_clears) moved verbatim to the
    // `shared_state` sibling module alongside `QueuedPlaceholderState`.
}

#[cfg(test)]
pub(crate) fn make_shared_data_for_tests() -> Arc<SharedData> {
    make_shared_data_for_tests_with_storage(None)
}

#[cfg(test)]
pub(crate) fn make_shared_data_for_tests_with_storage(
    pg_pool: Option<sqlx::PgPool>,
) -> Arc<SharedData> {
    make_shared_data_for_tests_with_barrier(
        pg_pool,
        Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    )
}

/// One process's providers share `shutdown_remaining`; a test of that barrier injects it.
#[cfg(test)]
pub(in crate::services) fn make_shared_data_for_tests_with_barrier(
    pg_pool: Option<sqlx::PgPool>,
    shutdown_remaining: Arc<std::sync::atomic::AtomicUsize>,
) -> Arc<SharedData> {
    let tmux_relay_coords = Arc::new(dashmap::DashMap::new());
    let mut prepared_bot_state = None;
    let engine_runtime = EngineRuntime::new(
        ProviderKind::Claude,
        9,
        pg_pool,
        PolicyRuntime { engine: None },
        Vec::new(),
        || {
            let core = tokio::sync::Mutex::new(CoreState {
                sessions: std::collections::HashMap::new(),
            });
            let mailboxes = ChannelMailboxRegistry::default();
            let settings = tokio::sync::RwLock::new(DiscordBotSettings::default());
            let api_timestamps = dashmap::DashMap::new();
            let registry =
                Arc::new(crate::services::engine_runtime::relay_state::RegistryState::new());
            prepared_bot_state = Some((settings, api_timestamps));
            (core, mailboxes, registry)
        },
    );
    let (settings, api_timestamps) =
        prepared_bot_state.expect("bootstrap storage initialized once");
    let tmux_watchers = TmuxWatcherRegistry::from_storage(
        Arc::clone(&engine_runtime.watcher_registry),
        Arc::clone(&tmux_relay_coords),
    );
    Arc::new(SharedData {
        engine: engine_runtime,
        settings,
        api_timestamps,
        tmux_watchers,
        tmux_relay_coords,
        ui: PlaceholderState {
            placeholder_cleanup: Arc::new(
                placeholder_cleanup::PlaceholderCleanupRegistry::default(),
            ),
            placeholder_controller: Arc::new(
                placeholder_controller::PlaceholderController::default(),
            ),
            placeholder_live_events: Arc::new(
                placeholder_live_events::PlaceholderLiveEvents::default(),
            ),
            two_message_panel_enabled: false,
        },
        queued: QueuedPlaceholderState {
            queued_placeholders: dashmap::DashMap::new(),
            queue_exit_placeholder_clears: dashmap::DashMap::new(),
            queued_placeholders_persist_locks: dashmap::DashMap::new(),
        },
        answer_flush_barrier: Arc::new(answer_flush_barrier::AnswerFlushBarrier::default()),
        // #3038 S3: wrapped at the first-member position (evaluation-order
        // preserved — the three members hoisted above the spawn calls are
        // side-effect-free constructors; see run_bot_build_shared_data).
        restart: RestartLifecycle {
            replay_restart: std::sync::OnceLock::new(),
            recovering_channels: dashmap::DashMap::new(),
            shutting_down: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            intake_worker_lifecycle: Default::default(),
            finalizing_turns: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            current_generation: 0,
            restart_pending: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            reconcile_done: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            deferred_hook_backlog: std::sync::atomic::AtomicUsize::new(0),
            deferred_hook_channels: dashmap::DashMap::new(),
            recovery_started_at: std::time::Instant::now(),
            recovery_duration_ms: std::sync::atomic::AtomicU64::new(0),
            global_active: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            global_finalizing: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            shutdown_remaining,
            shutdown_counted: std::sync::atomic::AtomicBool::new(false),
            shutdown_slot_consumed: std::sync::atomic::AtomicBool::new(false),
            gateway_waiter_cancellation: std::sync::Mutex::new(None),
            gateway_lease_diagnostics: Default::default(),
            gateway_handback_health: Arc::default(),
        },
        turn_finalizer: turn_finalizer::TurnFinalizer::spawn(),
        dispatch: DispatchRoutingState {
            intake_dedup: dashmap::DashMap::new(),
            thread_parents: dashmap::DashMap::new(),
            role_overrides: dashmap::DashMap::new(),
        },
        bot_connected: std::sync::atomic::AtomicBool::new(false),
        overrides: SessionOverrideState {
            model_overrides: dashmap::DashMap::new(),
            fast_mode_channels: dashmap::DashSet::new(),
            fast_mode_session_reset_pending: dashmap::DashSet::new(),
            codex_goals_channels: dashmap::DashSet::new(),
            codex_goals_session_reset_pending: dashmap::DashSet::new(),
            node_overrides: dashmap::DashMap::new(),
            model_session_reset_pending: dashmap::DashSet::new(),
            session_reset_pending: dashmap::DashSet::new(),
            model_picker_pending: dashmap::DashMap::new(),
        },
        last_message_ids: dashmap::DashMap::new(),
        catch_up_retry_pending: dashmap::DashMap::new(),
        catch_up_scan_in_progress: dashmap::DashMap::new(),
        channel_rosters: dashmap::DashMap::new(),
        http: RuntimeHttpCache {
            cached_serenity_ctx: tokio::sync::OnceCell::new(),
            cached_bot_token: tokio::sync::OnceCell::new(),
        },
        token_hash: "test-token-hash".to_string(),
        health_registry: std::sync::Weak::new(),
        known_slash_commands: tokio::sync::OnceCell::new(),
        inflight_signals: tokio::sync::broadcast::channel(256).0,
        turn_completion_events: turn_completion_events::turn_completion_event_bus(),
        turn_view_reconciler: turn_view_reconciler::TurnViewReconciler::default(),
        readopted_mailbox_ledger: readopted_mailbox_ledger::ReadoptedMailboxLedger::default(),
        router_ports: router::ROUTER_PORTS,
    })
}
