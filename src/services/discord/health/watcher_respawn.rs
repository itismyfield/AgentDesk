//! #3410 — STALL-WATCHDOG forced-cleanup follow-through: respawn the tmux
//! watcher and release stale mailbox ownership after a force-clean cancels
//! the watcher.
//!
//! ## The dead-zone this closes
//!
//! `run_stall_watchdog_pass`'s desynced force-clean calls
//! `apply_watchdog_orphan_token_cleanup`, which **unconditionally cancels the
//! channel watcher** (`tmux_watchers.remove` + `cancel.store(true)`) for the
//! `StallWatchdog` source and then asks `relay_recovery` to clear the mailbox
//! token — but ONLY when the relay state resolves to `OrphanPendingToken`.
//!
//! The real incident (#3410) fired on `relay_stall_state =
//! "tmux_alive_relay_dead"`, where `plan_relay_recovery` returns
//! `ReattachWatcher`, not `ClearOrphanPendingToken`. So the auto-heal call was
//! `skipped` (action-not-allowed), the mailbox's stale `active_user_message`
//! ownership was NEVER released, and the watcher stayed dead with no respawn.
//! Result: 85 minutes of permanent live-relay death — every later TUI-direct
//! turn got `skipping TUI-direct synthetic inflight; mailbox already owns a
//! different turn`.
//!
//! `recovery_engine`/`relay_recovery` deliberately step aside with
//! `observe_only` when they see "live turn evidence", because their contract
//! is to never disturb a healthy turn. The STALL-WATCHDOG, by contrast, has
//! ALREADY adjudicated this turn dead (it passed the force-clean predicate after
//! the liveness gate allowed cleanup). Once force-clean commits, the
//! cleanup is authoritative and therefore OWNS the recovery: it must release
//! the stale mailbox ownership AND respawn the watcher so the still-alive tmux
//! session keeps relaying. That is the resolution of the observe_only/cleanup
//! contradiction (#3410 requirement 4): **the path that kills the watcher
//! respawns it**; the existing relay-recovery observe_only behaviour is left
//! untouched for its own (non-force-clean) callers.
//!
//! Respawn reuses the canonical spawn mechanism (`registry.rebind_inflight`
//! → `recovery_engine::rebind_inflight_for_channel` →
//! `claim_or_reuse_watcher` + `spawn_observed_tmux_watcher`), so no new spawn
//! site is introduced. A respawn that fails (e.g. transient tmux probe miss) is
//! retried on a later tick under an exponential backoff, and the dead-man
//! switch escalates to ERROR if a watcher that should exist stays absent. After
//! [`WATCHER_RESPAWN_MAX_ATTEMPTS`] consecutive failures the channel is parked
//! for an operator rather than retried forever.

use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::atomic::Ordering;
use std::time::Instant;

use poise::serenity_prelude::ChannelId;

use super::HealthRegistry;
use super::snapshot::WatcherStateSnapshot;
use crate::services::discord::{self as discord, SharedData};
use crate::services::provider::ProviderKind;

/// A watcher absent for longer than this (despite a live AgentDesk tmux
/// session that should own one) escalates from WARN to ERROR — the dead-man
/// switch. Independent of the force-clean path: it fires for ANY cause of a
/// missing watcher (crashed task, failed respawn, manual kill).
pub(super) const WATCHER_ABSENCE_DEADMAN_SECS: u64 = 180;

/// TTL after which an absence entry nobody is still observing is garbage
/// collected (channel deleted, provider rebound, …). Anchored on the LAST
/// observation, not the first: an entry the sweep re-observes every tick must
/// never be dropped, because the sweep would immediately re-arm it with a fresh
/// `escalated=false` and the dead-man would re-fire once per TTL forever.
const WATCHER_ABSENCE_STATE_TTL_SECS: u64 = 1800;

/// Consecutive failed respawns after which the retry queue stops re-attempting
/// this channel. Without a cap an unrepairable channel re-attempts (and WARNs)
/// every tick for as long as its tmux lives.
const WATCHER_RESPAWN_MAX_ATTEMPTS: u32 = 8;

/// Delay before the first retry; doubles per consecutive failure up to
/// [`WATCHER_RESPAWN_BACKOFF_CEILING_SECS`].
const WATCHER_RESPAWN_BACKOFF_BASE_SECS: u64 = 30;
const WATCHER_RESPAWN_BACKOFF_CEILING_SECS: u64 = 960;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct WatcherAbsenceKey {
    provider: String,
    channel_id: u64,
}

#[derive(Clone, Debug)]
struct WatcherAbsenceState {
    first_seen_unix_secs: i64,
    last_seen_unix_secs: i64,
    escalated: bool,
    minimum_initial_offset: Option<u64>,
    failed_attempts: u32,
    next_attempt_unix_secs: i64,
    retirement_logged: bool,
}

impl WatcherAbsenceState {
    fn newly_absent(now_unix_secs: i64) -> Self {
        Self {
            first_seen_unix_secs: now_unix_secs,
            last_seen_unix_secs: now_unix_secs,
            escalated: false,
            minimum_initial_offset: None,
            failed_attempts: 0,
            next_attempt_unix_secs: now_unix_secs,
            retirement_logged: false,
        }
    }
}

/// Delay before the next respawn attempt after `failed_attempts` consecutive
/// failures.
fn respawn_backoff_secs(failed_attempts: u32) -> u64 {
    let doublings = failed_attempts.saturating_sub(1).min(5);
    (WATCHER_RESPAWN_BACKOFF_BASE_SECS << doublings).min(WATCHER_RESPAWN_BACKOFF_CEILING_SECS)
}

static WATCHER_ABSENCE: LazyLock<dashmap::DashMap<WatcherAbsenceKey, WatcherAbsenceState>> =
    LazyLock::new(dashmap::DashMap::new);

/// #3410 finalizer-reason tag used when the force-clean follow-through cancels
/// the stale mailbox token.
const FORCE_CLEAN_FINALIZER_REASON: &str = "3410_stall_watchdog_force_clean_respawn";

/// Pure decision: after a desynced force-clean, does this channel still have a
/// live AgentDesk tmux session that must keep relaying (and therefore needs a
/// respawned watcher)?
///
/// This is exactly the "live turn evidence" signature that previously made
/// `relay_recovery` step aside into observe_only. Here it is the trigger for
/// the cleanup-owns-respawn handoff.
pub(super) fn force_clean_should_respawn_watcher(snapshot: &WatcherStateSnapshot) -> bool {
    snapshot.tmux_session_alive == Some(true)
        && is_agentdesk_tmux_session(snapshot.tmux_session.as_deref())
}

pub(super) fn force_clean_respawn_offset_floor(
    snapshot: &WatcherStateSnapshot,
    committed_frontier_for_current_generation: Option<u64>,
) -> Option<u64> {
    // `snapshot.last_relay_offset` is health telemetry sourced from the raw
    // in-memory relay coord. It is not generation-fenced, so after a wrapper
    // restart it can name the old coordinate space even when the new file has
    // grown past that byte count. Respawn floors must only use the same-generation
    // frontier from `committed_frontier_for_current_generation`.
    let _unfenced_snapshot_frontier = snapshot.last_relay_offset;
    committed_frontier_for_current_generation.filter(|offset| *offset > 0)
}

#[cfg(unix)]
fn generation_fenced_respawn_frontier(
    shared: &SharedData,
    channel_id: ChannelId,
    snapshot: &WatcherStateSnapshot,
) -> Option<u64> {
    let tmux_session = snapshot.tmux_session.as_deref()?;
    crate::services::discord::tmux::committed_frontier_for_current_generation(
        shared,
        channel_id,
        tmux_session,
    )
}

// `discord::tmux` (and the on-disk generation fence it reads) is unix-only;
// without it there is no provable same-generation frontier, so respawn floors
// stay disabled rather than falling back to the unfenced snapshot value.
#[cfg(not(unix))]
fn generation_fenced_respawn_frontier(
    _shared: &SharedData,
    _channel_id: ChannelId,
    _snapshot: &WatcherStateSnapshot,
) -> Option<u64> {
    None
}

fn is_agentdesk_tmux_session(tmux_session: Option<&str>) -> bool {
    tmux_session.is_some_and(|session| session.starts_with("AgentDesk-"))
}

/// #3410 force-clean follow-through: release the stale mailbox ownership the
/// cleanup left and respawn the watcher on the still-live tmux session, then
/// feed the result into the dead-man switch.
///
/// The unconditional mailbox release means only a caller that has already
/// committed a force-clean may call this. No production path holds that
/// commitment since #4460 retired the destructive branch-4 cleanup; absence is
/// armed by `observe_watcher_absence_for_unwatched_work` instead (#5957). It is
/// kept (and tested) as the entry point a future destructive cleanup would have
/// to call to stay wired to the dead-man switch.
pub(super) async fn complete_force_clean_watcher_recovery(
    registry: &HealthRegistry,
    provider: &ProviderKind,
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    snapshot: &WatcherStateSnapshot,
    now_unix_secs: i64,
    repair_started_at: Instant,
) {
    release_stale_mailbox_ownership_after_force_clean(
        shared,
        provider,
        channel_id,
        snapshot.mailbox_active_user_msg_id,
        snapshot.mailbox_active_turn_nonce.clone(),
        repair_started_at,
    )
    .await;
    if !force_clean_should_respawn_watcher(snapshot) {
        return;
    }
    let minimum_initial_offset = force_clean_respawn_offset_floor(
        snapshot,
        generation_fenced_respawn_frontier(shared.as_ref(), channel_id, snapshot),
    );
    let watcher_spawned = respawn_watcher_after_force_clean(
        registry,
        provider,
        channel_id,
        snapshot.tmux_session.as_deref(),
        minimum_initial_offset,
        None,
    )
    .await;
    // Dead-man switch: if the respawn did not (re)establish a watcher for a
    // still-live AgentDesk tmux session, the absence is tracked and escalates
    // to ERROR after WATCHER_ABSENCE_DEADMAN_SECS.
    detect_and_escalate_watcher_absence(
        provider,
        channel_id,
        snapshot,
        shared.tmux_watchers.contains_key(&channel_id),
        now_unix_secs,
    );
    if !watcher_spawned {
        remember_watcher_absence_offset_floor(provider, channel_id, minimum_initial_offset);
    }
}

/// Arm the absences the stall watchdog's watcher-derived candidate loop cannot
/// reach (#5957), then drive every tracked absence — including one whose
/// respawn failed on an earlier tick (#3410) — on the same tick.
pub(super) async fn sweep_and_retry_absences(
    registry: &HealthRegistry,
    provider: &ProviderKind,
    runtimes: &[Arc<SharedData>],
    watcher_derived: &std::collections::HashSet<ChannelId>,
    now_unix_secs: i64,
) {
    observe_watcher_absence_for_unwatched_work(
        registry,
        provider,
        runtimes,
        watcher_derived,
        now_unix_secs,
    )
    .await;
    retry_pending_watcher_respawns(registry, provider, runtimes, now_unix_secs).await;
}

/// Re-attempt a respawn for every channel still tracked as watcher-absent whose
/// backoff has elapsed (the durable cross-tick retry queue). A channel whose
/// respawn failed dropped out of the watcher-derived candidate loop, so this is
/// the path that carries the retry.
///
/// #3410 P2: multi-bot deployments register several runtimes under one provider
/// name. The absence map is keyed by provider+channel (runtime-agnostic), but a
/// watcher is bound to exactly ONE runtime's `tmux_watchers`. Scanning
/// per-runtime `contains_key` made a NON-owning runtime observe "no watcher"
/// and re-insert a false absence even though the OWNING runtime's watcher is
/// alive. So watcher presence is resolved REGISTRY-WIDE (any runtime owns it ⇒
/// present) and the retry is scoped to the owning runtime.
///
/// Returns how many channels a respawn was actually attempted for; entries held
/// back by [`respawn_backoff_secs`] or past [`WATCHER_RESPAWN_MAX_ATTEMPTS`] are
/// not counted.
pub(super) async fn retry_pending_watcher_respawns(
    registry: &HealthRegistry,
    provider: &ProviderKind,
    runtimes: &[Arc<SharedData>],
    now_unix_secs: i64,
) -> usize {
    let mut attempted = 0usize;
    for channel_id in pending_absent_channels(provider) {
        if retry_pending_watcher_respawn(registry, provider, runtimes, channel_id, now_unix_secs)
            .await
        {
            attempted += 1;
        }
    }
    attempted
}

/// #5957: record watcher absence for channels the watchdog pass can no longer
/// see. `run_stall_watchdog_pass` derives its candidates from `tmux_watchers`,
/// so a cancelled watcher takes its channel out of the pass entirely and
/// nothing arms [`WATCHER_ABSENCE`] — leaving both the dead-man switch and
/// [`retry_pending_watcher_respawns`] with nothing to act on.
///
/// Candidates come from relay work still owed (a mailbox entry or a persisted
/// inflight row), never from the watcher stop event, so a deliberate
/// `cancel=true` shutdown of a finished channel is not resurrected:
/// `detect_and_escalate_watcher_absence` additionally requires a live
/// AgentDesk tmux session and `watcher_ownership_expected`.
///
/// Returns the number of channels tracked as absent after this observation.
pub(super) async fn observe_watcher_absence_for_unwatched_work(
    registry: &HealthRegistry,
    provider: &ProviderKind,
    runtimes: &[Arc<SharedData>],
    watcher_derived: &std::collections::HashSet<ChannelId>,
    now_unix_secs: i64,
) -> usize {
    let mut absent = 0usize;
    for (channel_id, shared) in
        unwatched_relay_work_candidates(provider, runtimes, watcher_derived).await
    {
        let Some(snapshot) = registry
            .snapshot_watcher_state_for_shared(provider, shared, channel_id.get())
            .await
        else {
            continue;
        };
        detect_and_escalate_watcher_absence(provider, channel_id, &snapshot, false, now_unix_secs);
        if WATCHER_ABSENCE.contains_key(&WatcherAbsenceKey::new(provider, channel_id)) {
            absent += 1;
        }
    }
    absent
}

/// Channels that are owed relay work but hold no watcher in any same-provider
/// runtime, excluding the ones the caller already covered from `tmux_watchers`.
/// Each is paired with the runtime to re-snapshot it from.
///
/// Work is read from the two authorities that outlive a watcher: the in-memory
/// mailbox and the persisted inflight row.
async fn unwatched_relay_work_candidates(
    provider: &ProviderKind,
    runtimes: &[Arc<SharedData>],
    watcher_derived: &std::collections::HashSet<ChannelId>,
) -> Vec<(ChannelId, Arc<SharedData>)> {
    let mut seen: std::collections::HashSet<ChannelId> = watcher_derived.clone();
    let mut candidates: Vec<(ChannelId, Arc<SharedData>)> = Vec::new();
    for shared in runtimes {
        for (channel_id, mailbox) in shared.mailboxes.snapshot_all().await {
            if !mailbox_owes_relay_work(&mailbox) {
                continue;
            }
            if seen.insert(channel_id) {
                candidates.push((channel_id, shared.clone()));
            }
        }
    }
    // Inflight rows are keyed provider+channel, so the runtime they are paired
    // with here is only a snapshot vehicle for the liveness read, never an
    // ownership claim: the respawn re-resolves the owning runtime itself.
    if let Some(fallback) = runtimes.first() {
        for (state, _) in discord::inflight::load_inflight_states_for_sweep(provider) {
            if state.channel_id == 0 {
                continue;
            }
            let channel_id = ChannelId::new(state.channel_id);
            if seen.insert(channel_id) {
                candidates.push((channel_id, fallback.clone()));
            }
        }
    }
    // Registry-wide presence (#3410 P2): a watcher owned by a sibling runtime
    // is still a live watcher.
    candidates.retain(|(channel_id, _)| runtime_owning_watcher(runtimes, *channel_id).is_none());
    candidates
}

/// The mailbox half of "relay work still owed": an active turn, or inbound
/// waiting behind one.
///
/// Deliberately the same two mailbox terms [`watcher_ownership_expected`] arms
/// on. A candidate that cannot satisfy that gate is swept, snapshotted and
/// dropped every tick for nothing, so a cancel token or a pending dispatch on
/// its own does not qualify a channel here.
fn mailbox_owes_relay_work(
    mailbox: &crate::services::turn_orchestrator::ChannelMailboxSnapshot,
) -> bool {
    mailbox.active_user_message_id.is_some() || !mailbox.intervention_queue.is_empty()
}

/// The runtime that currently owns `channel_id`'s watcher, if any runtime does
/// (the registry-wide presence signal for #3410 P2). When no runtime owns a
/// live watcher, returns `None` so the caller can pick a fallback runtime to
/// re-snapshot the inflight/tmux state from.
fn runtime_owning_watcher<'a>(
    runtimes: &'a [Arc<SharedData>],
    channel_id: ChannelId,
) -> Option<&'a Arc<SharedData>> {
    runtimes
        .iter()
        .find(|runtime| runtime.tmux_watchers.contains_key(&channel_id))
}

/// Release the stale `active_user_message` ownership the desynced force-clean
/// left behind. `apply_watchdog_orphan_token_cleanup` only clears the mailbox
/// token on the `OrphanPendingToken` relay-recovery branch; on the
/// `tmux_alive_relay_dead` branch (the #3410 incident) the auto-heal is skipped
/// and the ownership leaks, jamming every subsequent synthetic inflight.
///
/// Gated on the caller having already committed force-clean (the snapshot
/// passed the desynced + stall predicate after the liveness gate allowed cleanup), so
/// this never steals a genuinely live turn. `stale_user_msg_id` is the mailbox
/// owner captured AT the stall-confirmation snapshot; the release is
/// episode-, identity- and start-guarded
/// (`mailbox_finish_turn_if_matches_episode_started_before`, #4595)
/// so that if a NEW turn claimed the mailbox in the await-gap between the old
/// `ClearOrphanPendingToken` path (`relay_recovery.rs`) and this call, we no-op
/// rather than cancel the new turn's token + wrongly decrement `global_active`
/// — including a retry that reuses the same user message id under a fresh
/// episode nonce. Reuses that
/// finalizer's token-cancel / `global_active`-decrement invariants so every
/// turn-end path stays consistent.
///
/// `None` (no owner at the snapshot) means there is nothing stale to release —
/// any token present now is a NEW turn, so we leave it alone.
///
/// Returns `true` when a stale token was actually removed.
pub(super) async fn release_stale_mailbox_ownership_after_force_clean(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    stale_user_msg_id: Option<u64>,
    stale_turn_nonce: Option<String>,
    repair_started_at: Instant,
) -> bool {
    let Some(stale_user_msg_id) = stale_user_msg_id else {
        return false;
    };
    let expected = poise::serenity_prelude::MessageId::new(stale_user_msg_id);
    let finish = discord::mailbox_finish_turn_if_matches_episode_started_before(
        shared,
        provider,
        channel_id,
        expected,
        stale_turn_nonce,
        repair_started_at,
    )
    .await;
    let Some(token) = finish.removed_token.as_ref() else {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            channel_id = channel_id.get(),
            provider = provider.as_str(),
            stale_user_msg_id,
            reason = FORCE_CLEAN_FINALIZER_REASON,
            "  [{ts}] ℹ STALL-WATCHDOG: skipped stale mailbox release for channel {channel_id} — a new turn already owns the mailbox (identity/start mismatch), leaving it untouched",
        );
        return false;
    };
    token.cancelled.store(true, Ordering::Relaxed);
    let counter_decremented = discord::saturating_decrement_global_active(shared);
    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!(
        channel_id = channel_id.get(),
        provider = provider.as_str(),
        reason = FORCE_CLEAN_FINALIZER_REASON,
        global_active_decremented = counter_decremented,
        had_pending_queue = finish.has_pending,
        "  [{ts}] 🔄 STALL-WATCHDOG: released stale mailbox ownership after force cleanup for channel {channel_id} so the next turn's synthetic inflight can claim it",
    );
    true
}

/// Respawn the watcher that the force-clean cancelled, reusing the canonical
/// `rebind_inflight` spawn path. Logs an INFO on success and a WARN (naming the
/// next-tick retry) on failure. A failure is non-fatal: the next watchdog tick
/// re-attempts, and the dead-man switch escalates if the gap persists.
///
/// Returns `true` when a watcher was spawned by this call.
///
/// `expected_episode` pins the inflight row the caller observed so the rebind
/// adopts it; `None` leaves the rebind free to re-mint a row, which only a
/// caller that has already committed a force-clean may ask for.
pub(super) async fn respawn_watcher_after_force_clean(
    registry: &HealthRegistry,
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_session: Option<&str>,
    minimum_initial_offset: Option<u64>,
    expected_episode: Option<&discord::inflight::InflightEpisodePin>,
) -> bool {
    let ts = chrono::Local::now().format("%H:%M:%S");
    match registry
        .rebind_inflight_after_force_clean(
            provider,
            channel_id.get(),
            tmux_session.map(str::to_string),
            minimum_initial_offset,
            expected_episode,
        )
        .await
    {
        Some(Ok(outcome)) => {
            tracing::info!(
                channel_id = channel_id.get(),
                provider = provider.as_str(),
                tmux_session = outcome.tmux_session,
                initial_offset = outcome.initial_offset,
                minimum_initial_offset = minimum_initial_offset.unwrap_or(0),
                watcher_spawned = outcome.watcher_spawned,
                watcher_replaced = outcome.watcher_replaced,
                reason = FORCE_CLEAN_FINALIZER_REASON,
                "  [{ts}] 👁 STALL-WATCHDOG: respawned tmux watcher after force cleanup cancelled it for channel {channel_id} (live tmux session must keep relaying)",
            );
            if outcome.watcher_spawned {
                clear_watcher_absence(provider, channel_id);
            }
            outcome.watcher_spawned
        }
        Some(Err(error)) => {
            tracing::warn!(
                channel_id = channel_id.get(),
                provider = provider.as_str(),
                tmux_session = ?tmux_session,
                error = %error,
                reason = FORCE_CLEAN_FINALIZER_REASON,
                "  [{ts}] ⚠ STALL-WATCHDOG: failed to respawn tmux watcher after force cleanup for channel {channel_id}; will retry on the next watchdog tick",
            );
            false
        }
        None => {
            tracing::warn!(
                channel_id = channel_id.get(),
                provider = provider.as_str(),
                tmux_session = ?tmux_session,
                reason = FORCE_CLEAN_FINALIZER_REASON,
                "  [{ts}] ⚠ STALL-WATCHDOG: cannot respawn tmux watcher — provider runtime not yet resolvable for channel {channel_id}; will retry on the next watchdog tick",
            );
            false
        }
    }
}

/// Dead-man switch: a channel whose live AgentDesk tmux session should own a
/// watcher but currently has none in the registry is tracked from the first
/// pass that observes the gap. After [`WATCHER_ABSENCE_DEADMAN_SECS`] the gap
/// escalates from WARN to a single ERROR so the absence is loud regardless of
/// WHY the watcher is missing (failed respawn, crashed task, manual kill).
///
/// `watcher_present` is the authoritative registry signal for THIS pass.
/// Returns `true` on the pass that escalates to ERROR (for test assertions).
pub(super) fn detect_and_escalate_watcher_absence(
    provider: &ProviderKind,
    channel_id: ChannelId,
    snapshot: &WatcherStateSnapshot,
    watcher_present: bool,
    now_unix_secs: i64,
) -> bool {
    let should_have_watcher = snapshot.tmux_session_alive == Some(true)
        && is_agentdesk_tmux_session(snapshot.tmux_session.as_deref())
        && watcher_ownership_expected(snapshot);
    if watcher_present || !should_have_watcher {
        clear_watcher_absence(provider, channel_id);
        return false;
    }

    let key = WatcherAbsenceKey::new(provider, channel_id);
    let mut entry = WATCHER_ABSENCE
        .entry(key)
        .or_insert_with(|| WatcherAbsenceState::newly_absent(now_unix_secs));
    entry.last_seen_unix_secs = now_unix_secs;
    let absence_secs = saturating_age_secs(entry.first_seen_unix_secs, now_unix_secs);
    let ts = chrono::Local::now().format("%H:%M:%S");
    if absence_secs >= WATCHER_ABSENCE_DEADMAN_SECS && !entry.escalated {
        entry.escalated = true;
        tracing::error!(
            channel_id = channel_id.get(),
            provider = provider.as_str(),
            tmux_session = ?snapshot.tmux_session,
            absence_secs,
            deadman_secs = WATCHER_ABSENCE_DEADMAN_SECS,
            "  [{ts}] ☠ STALL-WATCHDOG dead-man switch: channel {channel_id} has a live AgentDesk tmux session but NO watcher for {absence_secs}s — live relay is dead",
        );
        return true;
    }
    if !entry.escalated {
        tracing::warn!(
            channel_id = channel_id.get(),
            provider = provider.as_str(),
            tmux_session = ?snapshot.tmux_session,
            absence_secs,
            deadman_secs = WATCHER_ABSENCE_DEADMAN_SECS,
            "  [{ts}] ⚠ STALL-WATCHDOG: channel {channel_id} watcher absent for {absence_secs}s with a live tmux session (escalates to ERROR at {WATCHER_ABSENCE_DEADMAN_SECS}s)",
        );
    }
    false
}

/// A watcher is "expected" for a channel when the mailbox still owns a turn,
/// inflight state is present, or there is queued work — i.e. there is relay
/// work that only a watcher can carry. A genuinely idle channel (no active
/// turn, no inflight, no queue) legitimately has no watcher, so it must NOT
/// trip the dead-man switch.
fn watcher_ownership_expected(snapshot: &WatcherStateSnapshot) -> bool {
    snapshot.mailbox_active_user_msg_id.is_some()
        || snapshot.inflight_state_present
        || snapshot.has_pending_queue
}

pub(super) fn clear_watcher_absence(provider: &ProviderKind, channel_id: ChannelId) {
    WATCHER_ABSENCE.remove(&WatcherAbsenceKey::new(provider, channel_id));
}

fn remember_watcher_absence_offset_floor(
    provider: &ProviderKind,
    channel_id: ChannelId,
    minimum_initial_offset: Option<u64>,
) {
    let Some(offset) = minimum_initial_offset else {
        return;
    };
    let key = WatcherAbsenceKey::new(provider, channel_id);
    if let Some(mut state) = WATCHER_ABSENCE.get_mut(&key) {
        state.minimum_initial_offset = Some(state.minimum_initial_offset.unwrap_or(0).max(offset));
    }
}

fn remembered_watcher_absence_offset_floor(
    provider: &ProviderKind,
    channel_id: ChannelId,
) -> Option<u64> {
    WATCHER_ABSENCE
        .get(&WatcherAbsenceKey::new(provider, channel_id))
        .and_then(|state| state.minimum_initial_offset)
}

fn max_offset_floor(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.max(right)),
        (Some(left), None) => Some(left),
        (None, Some(right)) => Some(right),
        (None, None) => None,
    }
}

fn retry_minimum_initial_offset_floor(
    remembered_floor: Option<u64>,
    snapshot_floor: Option<u64>,
    snapshot_available: bool,
) -> Option<u64> {
    // If a current snapshot exists but its committed frontier fails the
    // generation fence, any remembered floor is also unproven in the current
    // coordinate space. Snapshotless retries keep the remembered floor because
    // there is no current-generation evidence to evaluate yet.
    if snapshot_available && snapshot_floor.is_none() {
        return None;
    }
    max_offset_floor(remembered_floor, snapshot_floor)
}

/// Channels currently tracked as watcher-absent for `provider`. The absence map
/// is the durable cross-tick retry queue: a channel whose respawn FAILED stays
/// here (its tmux is still alive, just watcher-less) so a later tick re-attempts
/// it (#3410 requirement 1), subject to [`respawn_attempt_due`]. A successful
/// respawn calls [`clear_watcher_absence`], removing it from this queue.
fn pending_absent_channels(provider: &ProviderKind) -> Vec<ChannelId> {
    WATCHER_ABSENCE
        .iter()
        .filter(|entry| entry.key().provider == provider.as_str())
        .map(|entry| ChannelId::new(entry.key().channel_id))
        .collect()
}

/// Re-attempt a respawn for a channel previously recorded as watcher-absent.
/// Called once per watchdog tick for each [`pending_absent_channels`] entry,
/// re-snapshotting to confirm the live-tmux/no-watcher condition still holds
/// before retrying (the watcher may have come back on its own, or the tmux may
/// have exited, in which case the absence is cleared instead). Returns whether a
/// respawn was attempted.
///
/// #3410 P2: watcher presence is resolved REGISTRY-WIDE across `runtimes`, not
/// against a single runtime's `tmux_watchers`. The inflight/tmux snapshot is
/// taken from the OWNING runtime when one holds a watcher; otherwise from any
/// runtime (the global provider+channel inflight is identical across runtimes),
/// so a non-owning runtime can never re-insert a false absence over a live
/// owner's watcher.
///
/// #3410 P1 (r2): a `None` snapshot must NOT clear the absence. When no runtime
/// owns the watcher (the exact bug being healed) and force-clean has already
/// deleted the inflight file, the fallback `first()` runtime — which is not the
/// channel's owner — can return `None`. Clearing on `None` stranded the channel
/// before the channel-aware respawn (`rebind_inflight`, which resolves its OWN
/// owning runtime via `resolve_direct_meeting_runtime`) was ever attempted, so
/// the next zero-candidate pass permanently lost the watcher. The invariant:
/// the absence entry survives until a watcher is CONFIRMED present or a respawn
/// SUCCEEDS. `None` means "ownership/liveness not yet resolvable" — keep
/// retrying. We still drive the channel-aware respawn on `None`, because
/// `rebind_inflight` re-resolves the channel's true owning runtime regardless of
/// which runtime (if any) we could snapshot from.
async fn retry_pending_watcher_respawn(
    registry: &HealthRegistry,
    provider: &ProviderKind,
    runtimes: &[Arc<SharedData>],
    channel_id: ChannelId,
    now_unix_secs: i64,
) -> bool {
    let owner = runtime_owning_watcher(runtimes, channel_id);
    if owner.is_some() {
        // A runtime already owns a live watcher — nothing to respawn. Clear the
        // absence (registry-wide presence is confirmed) without re-snapshotting.
        clear_watcher_absence(provider, channel_id);
        return false;
    }
    // Backoff/cap gate ahead of the snapshot so a parked channel costs no
    // mailbox round-trip and no inflight read either.
    if !respawn_attempt_due(provider, channel_id, now_unix_secs) {
        return false;
    }
    // No owner: snapshot from any runtime to read the provider+channel-global
    // inflight/tmux liveness. A `None` snapshot does NOT clear the absence — it
    // is "not yet resolvable", and the channel-aware respawn below still runs.
    let snapshot_runtime = runtimes.first().cloned();
    let snapshot = match snapshot_runtime.as_ref() {
        Some(snapshot_runtime) => {
            registry
                .snapshot_watcher_state_for_shared(
                    provider,
                    snapshot_runtime.clone(),
                    channel_id.get(),
                )
                .await
        }
        None => None,
    };
    // Tmux gone / not an AgentDesk session ⇒ nothing to relay, retire the entry.
    // Only a CONFIRMED dead/foreign tmux (a real snapshot saying so) clears the
    // absence; a `None` snapshot leaves it alive for the next tick.
    if snapshot
        .as_ref()
        .is_some_and(|snap| !force_clean_should_respawn_watcher(snap))
    {
        detect_and_escalate_watcher_absence(
            provider,
            channel_id,
            snapshot.as_ref().expect("just checked Some"),
            false,
            now_unix_secs,
        );
        return false;
    }
    // Either a live-tmux snapshot or an unresolved `None` snapshot: drive the
    // channel-aware respawn. `rebind_inflight` resolves the channel's owning
    // runtime itself, so the `first()`-runtime snapshot miss does not block it.
    // A successful respawn calls `clear_watcher_absence`; a failure leaves the
    // entry for the next tick (never stranded). The tmux override falls back to
    // the persisted inflight state inside `rebind_inflight` when the snapshot is
    // `None`.
    let snapshot_floor = match (snapshot_runtime.as_ref(), snapshot.as_ref()) {
        (Some(snapshot_runtime), Some(snapshot)) => force_clean_respawn_offset_floor(
            snapshot,
            generation_fenced_respawn_frontier(snapshot_runtime.as_ref(), channel_id, snapshot),
        ),
        _ => None,
    };
    // Remembered retry floors are replayed only as `minimum_initial_offset`; the
    // manual rebind path ignores that value when a forced initial offset resets
    // coordinates (for example Codex-TUI truncate rebuilds).
    let minimum_initial_offset = retry_minimum_initial_offset_floor(
        remembered_watcher_absence_offset_floor(provider, channel_id),
        snapshot_floor,
        snapshot.is_some(),
    );
    // Pin the row we are recovering FOR. Without a pin the rebind's
    // `WatcherReattach` arm clears the canonical row and re-mints a synthetic
    // one; pinned, the same arm adopts it, and a row that changed underneath us
    // fails the rebind instead of being overwritten.
    let expected_episode = discord::inflight::load_inflight_state(provider, channel_id.get())
        .as_ref()
        .map(discord::inflight::InflightEpisodePin::from_state);
    let spawned = respawn_watcher_after_force_clean(
        registry,
        provider,
        channel_id,
        snapshot
            .as_ref()
            .and_then(|snap| snap.tmux_session.as_deref()),
        minimum_initial_offset,
        expected_episode.as_ref(),
    )
    .await;
    if !spawned {
        record_failed_respawn_attempt(provider, channel_id, now_unix_secs);
    }
    // Re-resolve registry-wide presence after the respawn attempt. If a snapshot
    // was available, feed it to the dead-man switch; on a `None` snapshot we do
    // NOT escalate (we have no liveness evidence), but the absence entry remains
    // so the next tick re-snapshots and retries.
    if let Some(snapshot) = snapshot.as_ref() {
        detect_and_escalate_watcher_absence(
            provider,
            channel_id,
            snapshot,
            runtime_owning_watcher(runtimes, channel_id).is_some(),
            now_unix_secs,
        );
    }
    true
}

/// Backoff and give-up gate for the retry queue. Due when the channel is still
/// under [`WATCHER_RESPAWN_MAX_ATTEMPTS`] and its backoff deadline has passed.
///
/// An exhausted channel logs once and then stays parked rather than being
/// dropped, so a watcher that does come back still clears it through
/// [`clear_watcher_absence`] and a later absence starts from a clean count.
fn respawn_attempt_due(provider: &ProviderKind, channel_id: ChannelId, now_unix_secs: i64) -> bool {
    let Some(mut state) = WATCHER_ABSENCE.get_mut(&WatcherAbsenceKey::new(provider, channel_id))
    else {
        return false;
    };
    if state.failed_attempts >= WATCHER_RESPAWN_MAX_ATTEMPTS {
        if !state.retirement_logged {
            state.retirement_logged = true;
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::error!(
                channel_id = channel_id.get(),
                provider = provider.as_str(),
                failed_attempts = state.failed_attempts,
                "  [{ts}] ☠ STALL-WATCHDOG: giving up automatic watcher respawn for channel {channel_id} after {} failed attempts — needs an operator",
                state.failed_attempts,
            );
        }
        return false;
    }
    now_unix_secs >= state.next_attempt_unix_secs
}

/// Charge a failed respawn against the channel's attempt budget and push the
/// next attempt out by [`respawn_backoff_secs`]. A success never lands here: it
/// clears the entry outright, which also resets the count.
fn record_failed_respawn_attempt(
    provider: &ProviderKind,
    channel_id: ChannelId,
    now_unix_secs: i64,
) {
    let Some(mut state) = WATCHER_ABSENCE.get_mut(&WatcherAbsenceKey::new(provider, channel_id))
    else {
        return;
    };
    state.failed_attempts = state.failed_attempts.saturating_add(1);
    state.next_attempt_unix_secs =
        now_unix_secs.saturating_add(respawn_backoff_secs(state.failed_attempts) as i64);
}

pub(super) fn gc_watcher_absence_state(now_unix_secs: i64) {
    WATCHER_ABSENCE.retain(|_, state| {
        saturating_age_secs(state.last_seen_unix_secs, now_unix_secs)
            <= WATCHER_ABSENCE_STATE_TTL_SECS
    });
}

fn saturating_age_secs(anchor_unix_secs: i64, now_unix_secs: i64) -> u64 {
    now_unix_secs.saturating_sub(anchor_unix_secs).max(0) as u64
}

impl WatcherAbsenceKey {
    fn new(provider: &ProviderKind, channel_id: ChannelId) -> Self {
        Self {
            provider: provider.as_str().to_string(),
            channel_id: channel_id.get(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use poise::serenity_prelude::{ChannelId, MessageId, UserId};

    use crate::services::discord::health::STALL_WATCHDOG_INTERVAL_SECS;
    use crate::services::discord::relay_health::{
        RelayActiveTurn, RelayHealthSnapshot, RelayStallState,
    };
    use crate::services::provider::{CancelToken, ProviderKind};

    use super::super::HealthRegistry;
    use super::super::snapshot::WatcherStateSnapshot;
    use super::*;

    fn snapshot(
        channel_id: u64,
        tmux_session: Option<&str>,
        tmux_alive: Option<bool>,
        inflight_present: bool,
        mailbox_active: Option<u64>,
    ) -> WatcherStateSnapshot {
        WatcherStateSnapshot {
            provider: ProviderKind::Codex.as_str().to_string(),
            attached: false,
            tmux_session: tmux_session.map(str::to_string),
            watcher_owner_channel_id: Some(channel_id),
            last_relay_offset: 10,
            inflight_state_present: inflight_present,
            last_relay_ts_ms: 1_700_000_000_000,
            last_capture_offset: Some(20),
            capture_coordinate: crate::services::discord::health::liveness_authority::CaptureCoordinateObservation {
                offset: Some(20),
                path_hash: 0,
                file_id: None,
                status: crate::services::discord::health::liveness_authority::CoordinateStatus::Observed,
            },
            unread_bytes: Some(10),
            desynced: true,
            reconnect_count: 0,
            inflight_started_at: Some("2026-06-12 00:00:00".to_string()),
            inflight_updated_at: Some("2026-06-12 00:00:00".to_string()),
            inflight_user_msg_id: Some(9001),
            inflight_current_msg_id: Some(9002),
            tmux_session_alive: tmux_alive,
            has_pending_queue: false,
            mailbox_active_user_msg_id: mailbox_active,
            mailbox_active_turn_nonce: None,
            bound_output_path: None,
            bound_session_id: None,
            transcript_binding_stall: "none",
            inflight_terminal_delivery_committed: false,
            inflight_identity: None,
            inflight_finalizer_turn_id: None,
            inflight_output_path: tmux_session.map(|tmux| format!("/tmp/{tmux}.jsonl")),
            #[cfg(unix)]
            reachability_observation: None,
            relay_stall_state: RelayStallState::TmuxAliveRelayDead,
            relay_health: RelayHealthSnapshot {
                provider: ProviderKind::Codex.as_str().to_string(),
                channel_id,
                active_turn: RelayActiveTurn::Foreground,
                tmux_session: tmux_session.map(str::to_string),
                tmux_alive,
                watcher_attached: false,
                watcher_attached_stale: false,
                watcher_owner_channel_id: Some(channel_id),
                watcher_owns_live_relay: false,
                bridge_inflight_present: inflight_present,
                bridge_current_msg_id: Some(9002),
                mailbox_has_cancel_token: mailbox_active.is_some(),
                mailbox_active_user_msg_id: mailbox_active,
                mailbox_turn_started_at_ms: None,
                mailbox_turn_age_secs: None,
                queue_depth: 0,
                pending_discord_callback_msg_id: Some(9002),
                pending_thread_proof: false,
                parent_channel_id: None,
                thread_channel_id: None,
                last_relay_ts_ms: Some(1_700_000_000_000),
                last_relay_age_secs: None,
                last_outbound_activity_ms: None,
                last_capture_offset: Some(20),
                last_relay_offset: 10,
                unread_bytes: Some(10),
                desynced: true,
                stale_thread_proof: false,
                unpaired_active_token_reconfirmed: false,
            },
        }
    }

    #[test]
    fn respawn_decision_requires_live_agentdesk_tmux() {
        assert!(force_clean_should_respawn_watcher(&snapshot(
            1,
            Some("AgentDesk-codex-live"),
            Some(true),
            true,
            Some(9001),
        )));
        // Dead tmux: nothing to relay, no respawn.
        assert!(!force_clean_should_respawn_watcher(&snapshot(
            2,
            Some("AgentDesk-codex-dead"),
            Some(false),
            true,
            Some(9001),
        )));
        // Non-AgentDesk session: not ours to respawn.
        assert!(!force_clean_should_respawn_watcher(&snapshot(
            3,
            Some("scratch-session"),
            Some(true),
            true,
            Some(9001),
        )));
    }

    #[test]
    fn force_clean_respawn_offset_floor_uses_generation_fenced_frontier() {
        let mut snap = snapshot(
            4,
            Some("AgentDesk-claude-adk-cc"),
            Some(true),
            true,
            Some(9001),
        );
        snap.last_relay_offset = 13_400_000;
        assert_eq!(
            force_clean_respawn_offset_floor(&snap, Some(14_930_326)),
            Some(14_930_326),
            "same-generation committed frontier is the only respawn floor source"
        );
        assert_eq!(
            force_clean_respawn_offset_floor(&snap, Some(0)),
            None,
            "offset zero is not a useful floor"
        );
    }

    #[cfg(unix)] // exercises the unix-only `discord::tmux` generation fence
    #[test]
    fn force_clean_respawn_offset_floor_ignores_stale_prior_generation_frontier() {
        let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
        let _env = EnvRootGuard(std::env::var_os("AGENTDESK_ROOT_DIR"));
        let tmp = tempfile::tempdir().expect("temp runtime root");
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", tmp.path()) };

        let provider = ProviderKind::Codex;
        let channel = ChannelId::new(4_140_002);
        let tmux_session = "AgentDesk-codex-stale-generation-floor";
        let generation_path = std::path::PathBuf::from(
            crate::services::tmux_common::session_temp_path(tmux_session, "generation"),
        );
        std::fs::create_dir_all(
            generation_path
                .parent()
                .expect("generation marker has a parent directory"),
        )
        .expect("create runtime session directory");
        std::fs::write(&generation_path, b"new-generation").expect("write generation marker");
        let current_generation =
            crate::services::discord::tmux::read_generation_file_mtime_ns(tmux_session);
        assert_ne!(current_generation, 0, "generation marker must be readable");

        let shared = crate::services::discord::make_shared_data_for_tests();
        let coord = shared.tmux_relay_coord(channel);
        coord
            .confirmed_end_offset
            .store(13_400_000, Ordering::Release);
        coord
            .confirmed_end_generation_mtime_ns
            .store(current_generation.saturating_sub(1), Ordering::Release);

        let mut snap = snapshot(
            channel.get(),
            Some(tmux_session),
            Some(true),
            true,
            Some(9001),
        );
        snap.last_relay_offset = 13_400_000;
        snap.relay_health.last_relay_offset = 13_400_000;
        snap.last_capture_offset = Some(14_930_326);
        snap.relay_health.last_capture_offset = Some(14_930_326);

        let committed_frontier =
            generation_fenced_respawn_frontier(shared.as_ref(), channel, &snap);
        assert_eq!(
            committed_frontier, None,
            "prior-generation committed offset must not survive the generation fence"
        );
        assert_eq!(
            force_clean_respawn_offset_floor(&snap, committed_frontier),
            None,
            "a stale prior-generation snapshot frontier must not floor a new-generation file that already grew past it"
        );
        assert_eq!(
            retry_minimum_initial_offset_floor(Some(13_400_000), committed_frontier, true),
            None,
            "a remembered prior-generation floor must be dropped when a current snapshot fails the generation fence"
        );
        clear_watcher_absence(&provider, channel);
    }

    #[test]
    fn remembered_absence_offset_floor_survives_snapshotless_retry() {
        let provider = ProviderKind::Codex;
        let channel = ChannelId::new(4_140_001);
        clear_watcher_absence(&provider, channel);
        WATCHER_ABSENCE.insert(
            WatcherAbsenceKey::new(&provider, channel),
            WatcherAbsenceState {
                minimum_initial_offset: Some(13_400_000),
                ..WatcherAbsenceState::newly_absent(1_000_000)
            },
        );

        assert_eq!(
            retry_minimum_initial_offset_floor(
                remembered_watcher_absence_offset_floor(&provider, channel),
                None,
                false,
            ),
            Some(13_400_000),
            "retry must preserve the force-clean offset floor when re-snapshot returns None"
        );
        clear_watcher_absence(&provider, channel);
    }

    struct EnvRootGuard(Option<std::ffi::OsString>);

    impl Drop for EnvRootGuard {
        fn drop(&mut self) {
            match self.0.take() {
                Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
                None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
            }
        }
    }

    #[test]
    fn release_stale_mailbox_ownership_unjams_next_synthetic_inflight() {
        let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
        let _env = EnvRootGuard(std::env::var_os("AGENTDESK_ROOT_DIR"));
        let tmp = tempfile::tempdir().expect("temp runtime root");
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", tmp.path()) };

        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime")
            .block_on(async {
                let provider = ProviderKind::Codex;
                let shared = crate::services::discord::make_shared_data_for_tests();
                let channel = ChannelId::new(3_410_201);
                let token = std::sync::Arc::new(CancelToken::new());
                let started = crate::services::discord::mailbox_try_start_turn(
                    &shared,
                    channel,
                    token.clone(),
                    UserId::new(7),
                    MessageId::new(1_515_137_342_367_862_825),
                )
                .await;
                assert!(
                    started,
                    "seed a stale mailbox owner mirroring the #3410 jam"
                );
                shared.restart.global_active.store(1, Ordering::Relaxed);

                // A DIFFERENT later turn id cannot claim while the stale owner holds it
                // — this is the `skipping TUI-direct synthetic inflight` rejection.
                let blocked = crate::services::discord::mailbox_try_start_turn(
                    &shared,
                    channel,
                    std::sync::Arc::new(CancelToken::new()),
                    UserId::new(8),
                    MessageId::new(9_999_999),
                )
                .await;
                assert!(!blocked, "stale ownership must block the next turn pre-fix");

                let mut rx =
                    crate::services::discord::turn_completion_events::subscribe_turn_completion_events(
                        shared.as_ref(),
                    );
                let released = release_stale_mailbox_ownership_after_force_clean(
                    &shared,
                    &provider,
                    channel,
                    Some(1_515_137_342_367_862_825),
                    token.turn_nonce().map(str::to_owned),
                    Instant::now(),
                )
                .await;
                assert!(
                    released,
                    "force-clean follow-through must release ownership"
                );
                let event = rx
                    .try_recv()
                    .expect("force-clean stale mailbox release must publish a completion event");
                assert_eq!(event.channel_id, channel);
                assert!(token.cancelled.load(Ordering::Relaxed));
                assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 0);
                assert!(
                    crate::services::discord::mailbox_snapshot(&shared, channel)
                        .await
                        .active_user_message_id
                        .is_none(),
                    "mailbox must be free for the next synthetic inflight"
                );

                // Now the next turn's synthetic inflight can claim the channel.
                let now_allowed = crate::services::discord::mailbox_try_start_turn(
                    &shared,
                    channel,
                    std::sync::Arc::new(CancelToken::new()),
                    UserId::new(8),
                    MessageId::new(9_999_999),
                )
                .await;
                assert!(now_allowed, "next turn must be able to claim after release");
            });
    }

    #[tokio::test]
    async fn release_is_noop_when_mailbox_already_idle() {
        let provider = ProviderKind::Codex;
        let shared = crate::services::discord::make_shared_data_for_tests();
        let channel = ChannelId::new(3_410_202);
        let released = release_stale_mailbox_ownership_after_force_clean(
            &shared,
            &provider,
            channel,
            Some(7_777),
            None,
            Instant::now(),
        )
        .await;
        assert!(!released, "idle mailbox release must be a no-op");
    }

    /// #3410 P1-b: between stall-confirmation and the release, a NEW turn can
    /// claim the mailbox (the old `ClearOrphanPendingToken` path may have freed
    /// it) while reusing the same message ID. The episode guard must leave that
    /// successor ALONE — releasing it would cancel a live turn's token and
    /// wrongly decrement `global_active`.
    #[tokio::test]
    async fn release_skips_same_message_id_successor_with_new_episode() {
        let provider = ProviderKind::Codex;
        let shared = crate::services::discord::make_shared_data_for_tests();
        let channel = ChannelId::new(3_410_206);
        // A NEW episode now owns the same message anchor. The durable save for
        // this successor is intentionally not modeled: the stale snapshot still
        // carries the predecessor nonce, which is the ABA window under test.
        let new_token = std::sync::Arc::new(CancelToken::new());
        let started = crate::services::discord::mailbox_try_start_turn(
            &shared,
            channel,
            new_token.clone(),
            UserId::new(9),
            MessageId::new(1_111_111),
        )
        .await;
        assert!(started, "seed the NEW live turn owner");
        shared.restart.global_active.store(1, Ordering::Relaxed);

        // Stall-confirmation captured the predecessor episode for this same ID.
        let released = release_stale_mailbox_ownership_after_force_clean(
            &shared,
            &provider,
            channel,
            Some(1_111_111),
            Some("stale-episode".to_string()),
            Instant::now(),
        )
        .await;
        assert!(!released, "identity mismatch must skip the release");
        assert!(
            !new_token.cancelled.load(Ordering::Relaxed),
            "the new turn's token must NOT be cancelled"
        );
        assert_eq!(
            shared.restart.global_active.load(Ordering::Relaxed),
            1,
            "global_active must not be wrongly decremented for a live turn"
        );
        assert_eq!(
            crate::services::discord::mailbox_snapshot(&shared, channel)
                .await
                .active_user_message_id,
            Some(MessageId::new(1_111_111)),
            "the new turn must still own the mailbox"
        );

        // Same channel, but now the stale id matches the current owner → release.
        let released_match = release_stale_mailbox_ownership_after_force_clean(
            &shared,
            &provider,
            channel,
            Some(1_111_111),
            new_token.turn_nonce().map(str::to_owned),
            Instant::now(),
        )
        .await;
        assert!(released_match, "matching stale identity must release");
        assert!(new_token.cancelled.load(Ordering::Relaxed));
        assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 0);
    }

    /// #3410 P1-b: a `None` snapshot owner (no turn at stall-confirmation) is a
    /// no-op even if a token appears later — that token is a new turn.
    #[tokio::test]
    async fn release_is_noop_when_snapshot_had_no_owner() {
        let provider = ProviderKind::Codex;
        let shared = crate::services::discord::make_shared_data_for_tests();
        let channel = ChannelId::new(3_410_207);
        let released = release_stale_mailbox_ownership_after_force_clean(
            &shared,
            &provider,
            channel,
            None,
            None,
            Instant::now(),
        )
        .await;
        assert!(!released, "no snapshot owner ⇒ nothing stale to release");
    }

    #[tokio::test]
    async fn respawn_reports_provider_unavailable_without_panicking() {
        // No runtime registered for the provider → rebind_inflight returns None,
        // and the follow-through reports failure (retry next tick) rather than
        // permanently giving up.
        let registry = HealthRegistry::new();
        let provider = ProviderKind::Codex;
        let channel = ChannelId::new(3_410_203);
        let spawned = respawn_watcher_after_force_clean(
            &registry,
            &provider,
            channel,
            Some("AgentDesk-codex-no-runtime"),
            None,
            None,
        )
        .await;
        assert!(!spawned, "missing runtime must not report a spawn");
    }

    fn test_watcher_handle(tmux_session_name: &str) -> crate::services::discord::TmuxWatcherHandle {
        use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64};
        crate::services::discord::TmuxWatcherHandle {
            tmux_session_name: tmux_session_name.to_string(),
            output_path: format!("/tmp/agentdesk-{tmux_session_name}.jsonl"),
            paused: std::sync::Arc::new(AtomicBool::new(false)),
            resume_offset: std::sync::Arc::new(std::sync::Mutex::new(None)),
            cancel: std::sync::Arc::new(AtomicBool::new(false)),
            pause_epoch: std::sync::Arc::new(AtomicU64::new(0)),
            turn_delivered: std::sync::Arc::new(AtomicBool::new(false)),
            // Fresh heartbeat so the slot is not stale.
            last_heartbeat_ts_ms: std::sync::Arc::new(AtomicI64::new(
                crate::services::discord::tmux_watcher_now_ms(),
            )),
        }
    }

    /// #3410 P2: a channel's watcher lives in exactly ONE runtime's registry.
    /// Multi-runtime retry must resolve presence REGISTRY-WIDE: a non-owning
    /// runtime (no watcher of its own) must NOT re-insert a false absence over
    /// the owning runtime's live watcher.
    #[tokio::test]
    async fn retry_does_not_reinsert_false_absence_when_an_owning_runtime_has_the_watcher() {
        let provider = ProviderKind::Codex;
        let channel = ChannelId::new(3_410_208);
        clear_watcher_absence(&provider, channel);

        let owner = crate::services::discord::make_shared_data_for_tests();
        let non_owner = crate::services::discord::make_shared_data_for_tests();
        // The OWNING runtime holds a live watcher for the channel.
        owner
            .tmux_watchers
            .insert(channel, test_watcher_handle("AgentDesk-codex-p2-owner"));
        assert!(owner.tmux_watchers.contains_key(&channel));
        assert!(!non_owner.tmux_watchers.contains_key(&channel));

        // `runtime_owning_watcher` must find the owner regardless of order.
        let runtimes = vec![non_owner.clone(), owner.clone()];
        assert!(
            runtime_owning_watcher(&runtimes, channel).is_some(),
            "registry-wide presence must see the owning runtime's watcher"
        );

        // Seed an absence as if a prior tick recorded one, then run the retry.
        WATCHER_ABSENCE.insert(
            WatcherAbsenceKey::new(&provider, channel),
            WatcherAbsenceState::newly_absent(1_000_000),
        );
        let registry = HealthRegistry::new();
        retry_pending_watcher_respawns(
            &registry,
            &provider,
            &runtimes,
            1_000_000 + WATCHER_ABSENCE_DEADMAN_SECS as i64 + 100,
        )
        .await;

        // The live owner watcher means the absence is CLEARED, never escalated.
        assert!(
            !WATCHER_ABSENCE.contains_key(&WatcherAbsenceKey::new(&provider, channel)),
            "a live owning watcher must clear the absence, not re-insert/escalate it"
        );

        // The escalation hinge P2 fixes: with a LIVE tmux snapshot that would
        // otherwise trip the dead-man, the registry-wide presence signal
        // (`runtime_owning_watcher(..).is_some()` ⇒ `true`) must SUPPRESS the
        // dead-man — whereas the buggy per-runtime `contains_key` on the
        // non-owning runtime would feed `false` and wrongly escalate.
        let live = snapshot(
            channel.get(),
            Some("AgentDesk-codex-p2-live"),
            Some(true),
            true,
            Some(9001),
        );
        let owner_present = runtime_owning_watcher(&runtimes, channel).is_some();
        assert!(
            owner_present,
            "owner runtime must be detected registry-wide"
        );
        clear_watcher_absence(&provider, channel);
        assert!(
            !detect_and_escalate_watcher_absence(
                &provider,
                channel,
                &live,
                owner_present,
                1_000_000 + WATCHER_ABSENCE_DEADMAN_SECS as i64 + 100,
            ),
            "registry-wide presence (true) must suppress the dead-man for a served channel"
        );

        // Contrast: the buggy per-runtime signal on the non-owning runtime
        // would feed `false`, escalating the SAME served channel — the exact
        // false-absence regression P2 fixes.
        clear_watcher_absence(&provider, channel);
        let warned_then_escalates = detect_and_escalate_watcher_absence(
            &provider,
            channel,
            &live,
            false,
            1_000_000 + WATCHER_ABSENCE_DEADMAN_SECS as i64 + 100,
        );
        assert!(
            !warned_then_escalates,
            "first false observation only WARNs (records absence)"
        );
        assert!(
            WATCHER_ABSENCE.contains_key(&WatcherAbsenceKey::new(&provider, channel)),
            "the buggy `false` signal records a false absence on a served channel"
        );
        clear_watcher_absence(&provider, channel);
    }

    /// #3410 P1 (r2): the no-owner retry fallback. When NO runtime owns the
    /// watcher (the exact bug being healed) and force-clean has already deleted
    /// the inflight file + released the mailbox, the `first()` fallback runtime
    /// — which is not the channel's owner — returns a `None` snapshot (idle
    /// channel). Pre-fix, that `None` UNCONDITIONALLY cleared the absence,
    /// stranding the channel before the channel-aware `rebind_inflight` respawn
    /// was ever attempted; the next zero-candidate pass then permanently lost
    /// the watcher. The invariant: a `None` snapshot must NOT clear the absence
    /// — it is "not yet resolvable", so the entry survives and the respawn is
    /// re-attempted on the next tick.
    #[tokio::test]
    async fn retry_keeps_absence_alive_when_no_owner_and_snapshot_is_none() {
        let provider = ProviderKind::Codex;
        let channel = ChannelId::new(3_410_210);
        clear_watcher_absence(&provider, channel);

        // A fresh runtime with NO watcher, idle mailbox, and no inflight file:
        // `snapshot_watcher_state_for_shared` returns `None` for this channel
        // (no attachment / relay-coord / inflight / mailbox / thread-proof). It
        // is NOT the channel's owner — it is the `first()` fallback.
        let non_owner = crate::services::discord::make_shared_data_for_tests();
        assert!(!non_owner.tmux_watchers.contains_key(&channel));
        let runtimes = vec![non_owner.clone()];
        assert!(
            runtime_owning_watcher(&runtimes, channel).is_none(),
            "no runtime owns the channel's watcher — the bug scenario"
        );

        // Seed the absence as a prior force-clean tick would, anchored at the
        // current time so the TTL GC does not retire it before the retry runs.
        let now = chrono::Utc::now().timestamp();
        WATCHER_ABSENCE.insert(
            WatcherAbsenceKey::new(&provider, channel),
            WatcherAbsenceState::newly_absent(now),
        );

        // Empty registry ⇒ `rebind_inflight` returns `None` ⇒ respawn reports
        // failure (no spawn). The absence MUST survive for the next tick.
        let registry = HealthRegistry::new();
        retry_pending_watcher_respawns(&registry, &provider, &runtimes, now).await;

        assert!(
            WATCHER_ABSENCE.contains_key(&WatcherAbsenceKey::new(&provider, channel)),
            "a None snapshot must NOT clear the absence — pre-fix this stranded \
             the channel by clearing before the channel-aware respawn ran"
        );

        // A second tick re-attempts the respawn against the still-live absence
        // (it was never stranded). Presence is still false, snapshot still None,
        // so the entry persists for continued retry rather than being lost.
        retry_pending_watcher_respawns(&registry, &provider, &runtimes, now + 1).await;
        assert!(
            WATCHER_ABSENCE.contains_key(&WatcherAbsenceKey::new(&provider, channel)),
            "the absence survives across retries until respawn succeeds or a \
             watcher is confirmed present"
        );

        clear_watcher_absence(&provider, channel);
    }

    /// #3410 P1-a: a force-clean that kills the LAST channel's watcher leaves
    /// zero watcher-derived candidates next tick. The retry + dead-man path is
    /// keyed on absence, so `run_stall_watchdog_pass` must still drive it even
    /// when `candidate_channels.is_empty()`. Pre-fix the early return stranded
    /// the absent channel forever (the dead-relay bug #3410 fixes).
    #[cfg(unix)]
    #[tokio::test]
    async fn stall_watchdog_pass_drives_retry_with_zero_watcher_candidates() {
        use super::super::recovery::run_stall_watchdog_pass;
        let provider = ProviderKind::Codex;
        let channel = ChannelId::new(3_410_209);
        clear_watcher_absence(&provider, channel);
        let _tmux = FakeTmux::answering_alive(true);

        let registry = HealthRegistry::new();
        let shared = crate::services::discord::make_shared_data_for_tests();
        // Engage the mailbox so the absent channel produces a real snapshot
        // (an idle channel returns `None` and is unconditionally cleared).
        engage_mailbox_bound_to_tmux(&shared, channel, 424_242, "AgentDesk-codex-3410-zero").await;
        // No watcher inserted: candidate_channels will be empty this pass.
        assert!(!shared.tmux_watchers.contains_key(&channel));
        registry
            .register(provider.as_str().to_string(), shared.clone())
            .await;

        // Seed an absence that the retry path must reach despite zero
        // candidates. `run_stall_watchdog_pass` stamps its own `now` from the
        // wall clock, so anchor the entry at the current time.
        let now = chrono::Utc::now().timestamp();
        WATCHER_ABSENCE.insert(
            WatcherAbsenceKey::new(&provider, channel),
            WatcherAbsenceState::newly_absent(now),
        );

        run_stall_watchdog_pass(&registry, &provider).await;

        // Only the retry path charges an attempt, so a non-zero count is proof
        // it ran. Pre-#3410 the early return skipped it on a zero-candidate
        // pass and the channel was stranded at zero attempts forever.
        assert_eq!(
            failed_attempts(&provider, channel),
            Some(1),
            "retry must run on a zero-candidate pass"
        );
        clear_watcher_absence(&provider, channel);
    }

    fn failed_attempts(provider: &ProviderKind, channel: ChannelId) -> Option<u32> {
        WATCHER_ABSENCE
            .get(&WatcherAbsenceKey::new(provider, channel))
            .map(|state| state.failed_attempts)
    }

    /// A `tmux` on PATH that answers every liveness probe the same way, so a
    /// test can drive the real snapshot → probe → arming chain instead of
    /// hand-building a `WatcherStateSnapshot`.
    ///
    /// `runtime_path_entries` puts the process PATH first, so the fake shadows
    /// a real tmux. Both fields are drop-ordered: PATH is restored before the
    /// directory holding the script goes away.
    #[cfg(unix)]
    struct FakeTmux {
        _path: crate::config::TestEnvVarGuard,
        _dir: tempfile::TempDir,
    }

    #[cfg(unix)]
    impl FakeTmux {
        fn answering_alive(alive: bool) -> Self {
            use std::os::unix::fs::PermissionsExt;

            let dir = tempfile::TempDir::new().expect("temp dir");
            // `tmux_command` prepends global flags (`-u`) before the
            // subcommand; drop them so the body can match positionally.
            let body = if alive {
                "case \"$1\" in list-panes) echo 0 ;; esac; exit 0"
            } else {
                "echo \"can't find session: test\" >&2; exit 1"
            };
            let script = dir.path().join("tmux");
            std::fs::write(
                &script,
                format!("#!/bin/sh\nwhile [ \"${{1#-}}\" != \"$1\" ]; do shift; done\n{body}\n"),
            )
            .expect("write fake tmux");
            let mut perms = std::fs::metadata(&script).expect("stat").permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script, perms).expect("chmod");

            let joined = std::env::join_paths(std::iter::once(dir.path().to_path_buf()).chain(
                std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
            ))
            .expect("join PATH");
            let path = crate::config::TestEnvVarGuard::set_path(
                "PATH",
                std::path::Path::new(joined.as_os_str()),
            );
            Self {
                _path: path,
                _dir: dir,
            }
        }
    }

    /// Claim the mailbox's active-turn slot with the tmux binding a real turn
    /// carries, so `authoritative_tmux_session` can name a probe target.
    async fn engage_mailbox_bound_to_tmux(
        shared: &crate::services::discord::SharedData,
        channel: ChannelId,
        user_message_id: u64,
        tmux_session: &str,
    ) {
        let token = std::sync::Arc::new(CancelToken::new());
        token.bind_unmanaged_session_name(tmux_session);
        assert!(
            crate::services::discord::mailbox_try_start_turn(
                shared,
                channel,
                token,
                UserId::new(11),
                MessageId::new(user_message_id),
            )
            .await,
            "test fixture must own the turn"
        );
    }

    /// Claim the mailbox's active-turn slot: the "relay work still owed" signal
    /// that outlives the watcher.
    async fn engage_mailbox(
        shared: &crate::services::discord::SharedData,
        channel: ChannelId,
        user_message_id: u64,
    ) {
        assert!(
            crate::services::discord::mailbox_try_start_turn(
                shared,
                channel,
                std::sync::Arc::new(CancelToken::new()),
                UserId::new(11),
                MessageId::new(user_message_id),
            )
            .await,
            "test fixture must own the turn"
        );
    }

    async fn candidate_channels(
        provider: &ProviderKind,
        runtimes: &[std::sync::Arc<crate::services::discord::SharedData>],
        watcher_derived: &std::collections::HashSet<ChannelId>,
    ) -> Vec<ChannelId> {
        unwatched_relay_work_candidates(provider, runtimes, watcher_derived)
            .await
            .into_iter()
            .map(|(channel_id, _)| channel_id)
            .collect()
    }

    /// #5957: the watchdog pass derives candidates from `tmux_watchers`, so the
    /// channel whose watcher was just cancelled is invisible to every branch.
    /// A mailbox that still owns a turn is the proof that relay work is owed,
    /// and it outlives the watcher — so that channel must become a candidate.
    #[tokio::test]
    async fn unwatched_candidates_include_a_mailbox_channel_whose_watcher_was_cancelled() {
        let provider = ProviderKind::Codex;
        let channel = ChannelId::new(5_957_001);
        let shared = crate::services::discord::make_shared_data_for_tests();
        engage_mailbox(&shared, channel, 5_957_101).await;
        assert!(!shared.tmux_watchers.contains_key(&channel));

        let runtimes = vec![shared.clone()];
        let empty = std::collections::HashSet::new();
        assert!(
            candidate_channels(&provider, &runtimes, &empty)
                .await
                .contains(&channel),
            "a mailbox-engaged channel with no watcher must be swept"
        );

        // The watcher-derived loop already covered it this tick: no double work.
        let covered = std::collections::HashSet::from([channel]);
        assert!(
            !candidate_channels(&provider, &runtimes, &covered)
                .await
                .contains(&channel),
            "a channel the watcher-derived loop already handled must be skipped"
        );

        // A live watcher of its own retires the channel from the sweep.
        shared
            .tmux_watchers
            .insert(channel, test_watcher_handle("AgentDesk-codex-5957-own"));
        assert!(
            !candidate_channels(&provider, &runtimes, &empty)
                .await
                .contains(&channel),
            "a channel with a live watcher is not absent"
        );
    }

    /// #3410 P2 invariant, preserved by the #5957 sweep: presence is resolved
    /// registry-wide, so a sibling runtime's watcher must not be reported absent
    /// just because the runtime we enumerated from does not hold it.
    #[tokio::test]
    async fn unwatched_candidates_exclude_a_channel_served_by_a_sibling_runtime() {
        let provider = ProviderKind::Codex;
        let channel = ChannelId::new(5_957_002);
        let non_owner = crate::services::discord::make_shared_data_for_tests();
        let owner = crate::services::discord::make_shared_data_for_tests();
        engage_mailbox(&non_owner, channel, 5_957_102).await;
        owner
            .tmux_watchers
            .insert(channel, test_watcher_handle("AgentDesk-codex-5957-sibling"));

        let runtimes = vec![non_owner, owner];
        assert!(
            !candidate_channels(&provider, &runtimes, &std::collections::HashSet::new())
                .await
                .contains(&channel),
            "a sibling runtime's live watcher must suppress the absence"
        );
    }

    /// #5957: a dead tmux session is not a recovery case. The sweep must arm
    /// nothing there, so an intentional `cancel=true` shutdown is never
    /// resurrected by the respawn retry the sweep feeds.
    ///
    /// The channel carries a NAMED `AgentDesk-` session whose probe answers
    /// dead. Naming it is the point: with no name at all the probe is never
    /// reached, and the test would pass without exercising the liveness gate it
    /// is here to pin.
    #[cfg(unix)]
    #[tokio::test]
    async fn sweep_arms_nothing_for_a_channel_whose_tmux_is_dead() {
        let provider = ProviderKind::Codex;
        let channel = ChannelId::new(5_957_003);
        clear_watcher_absence(&provider, channel);
        let _tmux = FakeTmux::answering_alive(false);
        let shared = crate::services::discord::make_shared_data_for_tests();
        engage_mailbox_bound_to_tmux(&shared, channel, 5_957_103, "AgentDesk-codex-5957-dead")
            .await;
        let registry = HealthRegistry::new();
        let absent = observe_watcher_absence_for_unwatched_work(
            &registry,
            &provider,
            &[shared],
            &std::collections::HashSet::new(),
            chrono::Utc::now().timestamp(),
        )
        .await;
        assert_eq!(absent, 0, "a dead AgentDesk tmux session ⇒ nothing to heal");
        assert!(
            !WATCHER_ABSENCE.contains_key(&WatcherAbsenceKey::new(&provider, channel)),
            "a dead tmux session must not enter the respawn retry queue"
        );
    }

    /// #5957 P0: the whole chain in one test — the sweep arms a live-tmux
    /// channel with no watcher, the retry queue picks it up and attempts one
    /// respawn, and the failed attempt buys a backoff window instead of
    /// re-attempting on every 30s tick.
    ///
    /// Respawn fails here because the test registry resolves no Discord http,
    /// which is exactly the failure shape the backoff exists for.
    #[cfg(unix)]
    #[tokio::test]
    async fn absence_sweep_arms_then_drives_one_respawn_attempt_and_backs_off() {
        let provider = ProviderKind::Codex;
        let channel = ChannelId::new(5_957_010);
        clear_watcher_absence(&provider, channel);
        let _tmux = FakeTmux::answering_alive(true);
        let registry = HealthRegistry::new();
        let shared = crate::services::discord::make_shared_data_for_tests();
        engage_mailbox_bound_to_tmux(&shared, channel, 5_957_110, "AgentDesk-codex-5957-live")
            .await;
        registry
            .register(provider.as_str().to_string(), shared.clone())
            .await;
        let runtimes = vec![shared.clone()];
        let t0 = chrono::Utc::now().timestamp();

        observe_watcher_absence_for_unwatched_work(
            &registry,
            &provider,
            &runtimes,
            &std::collections::HashSet::new(),
            t0,
        )
        .await;
        assert_eq!(
            failed_attempts(&provider, channel),
            Some(0),
            "a live AgentDesk tmux with relay work owed and no watcher must arm"
        );

        // `>=`, not `==`: the count is process-wide and sibling tests share the
        // absence map.
        assert!(
            retry_pending_watcher_respawns(&registry, &provider, &runtimes, t0).await >= 1,
            "the armed channel must reach the respawn path"
        );
        assert_eq!(
            failed_attempts(&provider, channel),
            Some(1),
            "the failed respawn must be charged to this channel"
        );

        retry_pending_watcher_respawns(&registry, &provider, &runtimes, t0).await;
        assert_eq!(
            failed_attempts(&provider, channel),
            Some(1),
            "no second attempt on the same tick"
        );
        retry_pending_watcher_respawns(
            &registry,
            &provider,
            &runtimes,
            t0 + WATCHER_RESPAWN_BACKOFF_BASE_SECS as i64 - 1,
        )
        .await;
        assert_eq!(
            failed_attempts(&provider, channel),
            Some(1),
            "no attempt inside the backoff window"
        );
        retry_pending_watcher_respawns(
            &registry,
            &provider,
            &runtimes,
            t0 + WATCHER_RESPAWN_BACKOFF_BASE_SECS as i64,
        )
        .await;
        assert_eq!(
            failed_attempts(&provider, channel),
            Some(2),
            "the attempt is due again once the backoff elapses"
        );

        clear_watcher_absence(&provider, channel);
    }

    /// #5957 P0: an unrepairable channel must stop consuming ticks. It stays
    /// tracked while parked so a watcher that does come back still clears it.
    #[cfg(unix)]
    #[tokio::test]
    async fn absence_retry_parks_the_channel_after_the_attempt_cap() {
        let provider = ProviderKind::Codex;
        let channel = ChannelId::new(5_957_011);
        clear_watcher_absence(&provider, channel);
        let _tmux = FakeTmux::answering_alive(true);
        let registry = HealthRegistry::new();
        let shared = crate::services::discord::make_shared_data_for_tests();
        engage_mailbox_bound_to_tmux(&shared, channel, 5_957_111, "AgentDesk-codex-5957-cap").await;
        registry
            .register(provider.as_str().to_string(), shared.clone())
            .await;
        let runtimes = vec![shared.clone()];
        let t0 = chrono::Utc::now().timestamp();
        WATCHER_ABSENCE.insert(
            WatcherAbsenceKey::new(&provider, channel),
            WatcherAbsenceState::newly_absent(t0),
        );

        // Step past the ceiling every time, so only the cap — never the backoff
        // — can stop the retries.
        let mut attempts = 0usize;
        for step in 0..(2 * WATCHER_RESPAWN_MAX_ATTEMPTS as i64) {
            let now = t0 + step * WATCHER_RESPAWN_BACKOFF_CEILING_SECS as i64;
            if retry_pending_watcher_respawn(&registry, &provider, &runtimes, channel, now).await {
                attempts += 1;
            }
        }
        assert_eq!(
            attempts, WATCHER_RESPAWN_MAX_ATTEMPTS as usize,
            "the retry queue must stop at the cap instead of attempting forever"
        );
        assert!(
            WATCHER_ABSENCE.contains_key(&WatcherAbsenceKey::new(&provider, channel)),
            "a parked channel stays tracked so a returning watcher clears it"
        );

        clear_watcher_absence(&provider, channel);
    }

    /// #5957 P0: the respawn the retry drives must pin the inflight row it is
    /// recovering for, or the rebind's `WatcherReattach` arm clears that row and
    /// re-mints a synthetic one.
    ///
    /// Pinned at the source because the behaviour is unreachable from here under
    /// test: the rebind resolves its own runtime and returns before touching
    /// inflight when no Discord http is registered.
    #[test]
    fn retry_respawn_call_site_pins_the_inflight_episode() {
        let body = include_str!("watcher_respawn.rs")
            .split_once("async fn retry_pending_watcher_respawn(")
            .expect("single-channel retry")
            .1;
        // Bounded to the argument list: an unbounded slice runs to EOF and this
        // assertion's own needle satisfies it.
        let args = body
            .split_once("respawn_watcher_after_force_clean(")
            .expect("respawn call")
            .1
            .split_once("\n    )\n    .await")
            .expect("call terminator")
            .0;
        assert!(
            args.contains("expected_episode.as_ref()"),
            "the retry must pass its episode pin to the respawn, got: {args}"
        );
    }

    /// #5957: the candidate gate must read the same mailbox terms the arming
    /// gate does. A term that can never satisfy `watcher_ownership_expected`
    /// only buys a snapshot round-trip per tick and is dropped again.
    #[test]
    fn relay_work_terms_cannot_qualify_a_channel_the_arming_gate_rejects() {
        let mut mailbox = crate::services::turn_orchestrator::ChannelMailboxSnapshot::default();
        assert!(
            !mailbox_owes_relay_work(&mailbox),
            "an idle mailbox owes nothing"
        );

        mailbox.cancel_token = Some(std::sync::Arc::new(CancelToken::new()));
        mailbox.pending_user_dispatch = Some(MessageId::new(5_957_200));
        assert!(
            !mailbox_owes_relay_work(&mailbox),
            "neither term reaches `watcher_ownership_expected`"
        );

        mailbox.active_user_message_id = Some(MessageId::new(5_957_201));
        assert!(
            mailbox_owes_relay_work(&mailbox),
            "an active turn owes relay work"
        );
    }

    #[test]
    fn respawn_backoff_doubles_then_holds_at_the_ceiling() {
        assert_eq!(respawn_backoff_secs(1), WATCHER_RESPAWN_BACKOFF_BASE_SECS);
        assert_eq!(
            respawn_backoff_secs(2),
            2 * WATCHER_RESPAWN_BACKOFF_BASE_SECS
        );
        assert_eq!(
            respawn_backoff_secs(3),
            4 * WATCHER_RESPAWN_BACKOFF_BASE_SECS
        );
        assert_eq!(
            respawn_backoff_secs(6),
            WATCHER_RESPAWN_BACKOFF_CEILING_SECS
        );
        assert_eq!(
            respawn_backoff_secs(u32::MAX),
            WATCHER_RESPAWN_BACKOFF_CEILING_SECS,
            "a saturated attempt count must not shift past the ceiling"
        );
    }

    /// #5957 P0: the sweep is only useful if the watchdog pass calls it. This
    /// dies if the `observe_watcher_absence_for_unwatched_work` call is removed
    /// from `run_stall_watchdog_pass`: nothing else arms an absence for a
    /// channel that never had a watcher.
    #[cfg(unix)]
    #[tokio::test]
    async fn stall_watchdog_pass_arms_absence_for_an_unwatched_mailbox_channel() {
        use super::super::recovery::run_stall_watchdog_pass;
        let provider = ProviderKind::Codex;
        let channel = ChannelId::new(5_957_012);
        clear_watcher_absence(&provider, channel);
        let _tmux = FakeTmux::answering_alive(true);
        let registry = HealthRegistry::new();
        let shared = crate::services::discord::make_shared_data_for_tests();
        engage_mailbox_bound_to_tmux(&shared, channel, 5_957_112, "AgentDesk-codex-5957-pass")
            .await;
        assert!(!shared.tmux_watchers.contains_key(&channel));
        registry
            .register(provider.as_str().to_string(), shared.clone())
            .await;

        run_stall_watchdog_pass(&registry, &provider).await;

        assert_eq!(
            failed_attempts(&provider, channel),
            Some(1),
            "the pass must arm the absence and drive its first respawn attempt"
        );

        clear_watcher_absence(&provider, channel);
    }

    /// #5957 incident fixture — `AgentDesk-claude-adk-cc`, watcher cancelled at
    /// 16:17:20Z with the tmux session still alive, then 5h14m of silence. Every
    /// 30s tick of that window must keep the channel in the respawn retry queue,
    /// and the dead-man must fire once for the episode rather than once per tick
    /// or once per TTL.
    #[cfg(unix)]
    #[test]
    fn watcher_absence_feeds_the_retry_queue_for_the_whole_5h_gap() {
        let provider = ProviderKind::Codex;
        let channel = ChannelId::new(5_957_004);
        clear_watcher_absence(&provider, channel);
        // Held for the shared env mutex alone: the watchdog-pass tests GC the
        // absence map against the wall clock, which retires this fixture's
        // incident-anchored entry mid-loop if they run alongside it.
        let _serialized = FakeTmux::answering_alive(true);
        let live = snapshot(
            channel.get(),
            Some("AgentDesk-codex-5957-gap"),
            Some(true),
            true,
            Some(9001),
        );
        let t0 = 1_789_575_440; // 2026-09-16T16:17:20Z
        let gap_secs = 5 * 3600 + 14 * 60;
        let tick = STALL_WATCHDOG_INTERVAL_SECS as i64;

        assert!(
            !detect_and_escalate_watcher_absence(&provider, channel, &live, false, t0),
            "the first observation only WARNs"
        );
        assert!(
            pending_absent_channels(&provider).contains(&channel),
            "the WARN must queue the channel for respawn"
        );

        // Production tick order: GC first, then the sweep's observation.
        let mut escalations = 0;
        let mut now = t0 + tick;
        while now <= t0 + gap_secs {
            gc_watcher_absence_state(now);
            if detect_and_escalate_watcher_absence(&provider, channel, &live, false, now) {
                escalations += 1;
            }
            assert!(
                pending_absent_channels(&provider).contains(&channel),
                "the channel must stay queued for respawn at +{}s",
                now - t0
            );
            now += tick;
        }
        assert_eq!(
            escalations, 1,
            "one ERROR for the episode — an entry the sweep keeps observing must \
             never be GC'd and re-armed with a fresh escalation flag"
        );

        // The TTL still retires what nobody observes: stop sweeping and the
        // entry ages out of the retry queue.
        gc_watcher_absence_state(now + WATCHER_ABSENCE_STATE_TTL_SECS as i64);
        assert!(
            !pending_absent_channels(&provider).contains(&channel),
            "an unobserved absence expires"
        );

        // The watcher coming back retires the entry.
        detect_and_escalate_watcher_absence(&provider, channel, &live, false, now);
        assert!(!detect_and_escalate_watcher_absence(
            &provider, channel, &live, true, now,
        ));
        assert!(!pending_absent_channels(&provider).contains(&channel));
    }

    #[test]
    fn deadman_switch_escalates_after_threshold_then_clears_on_watcher_return() {
        let provider = ProviderKind::Codex;
        let channel = ChannelId::new(3_410_204);
        clear_watcher_absence(&provider, channel);
        let snap = snapshot(
            channel.get(),
            Some("AgentDesk-codex-deadman"),
            Some(true),
            true,
            Some(9001),
        );
        let t0 = 1_000_000;

        // First observation: WARN only, no escalation.
        assert!(!detect_and_escalate_watcher_absence(
            &provider, channel, &snap, false, t0,
        ));
        // Still inside the window.
        assert!(!detect_and_escalate_watcher_absence(
            &provider,
            channel,
            &snap,
            false,
            t0 + (WATCHER_ABSENCE_DEADMAN_SECS as i64) - 1,
        ));
        // Past the deadman threshold: escalate exactly once.
        assert!(detect_and_escalate_watcher_absence(
            &provider,
            channel,
            &snap,
            false,
            t0 + WATCHER_ABSENCE_DEADMAN_SECS as i64,
        ));
        // Idempotent: no second ERROR.
        assert!(!detect_and_escalate_watcher_absence(
            &provider,
            channel,
            &snap,
            false,
            t0 + WATCHER_ABSENCE_DEADMAN_SECS as i64 + 5,
        ));

        // Watcher returns → absence state cleared; a fresh gap starts over.
        assert!(!detect_and_escalate_watcher_absence(
            &provider,
            channel,
            &snap,
            true,
            t0 + 1000,
        ));
        assert!(!detect_and_escalate_watcher_absence(
            &provider,
            channel,
            &snap,
            false,
            t0 + 1001,
        ));
        clear_watcher_absence(&provider, channel);
    }

    #[test]
    fn deadman_switch_ignores_idle_channel_without_relay_work() {
        let provider = ProviderKind::Codex;
        let channel = ChannelId::new(3_410_205);
        clear_watcher_absence(&provider, channel);
        // Live tmux but no mailbox owner / inflight / queue → legitimately
        // watcher-free, must not trip the dead-man switch.
        let idle = snapshot(
            channel.get(),
            Some("AgentDesk-codex-idle"),
            Some(true),
            false,
            None,
        );
        assert!(!detect_and_escalate_watcher_absence(
            &provider,
            channel,
            &idle,
            false,
            1_000_000 + WATCHER_ABSENCE_DEADMAN_SECS as i64 + 100,
        ));
        clear_watcher_absence(&provider, channel);
    }
}
