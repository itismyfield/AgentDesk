use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use poise::serenity_prelude as serenity;
use serde::Serialize;
use serenity::{ChannelId, MessageId};

use crate::services::discord::session_identity::tmux_name_from_session_key;
use crate::services::discord::turn_view_reconciler::note_intake_turn_cleared_via_shared as tv_clear;
use crate::services::discord::{self as discord, SharedData};
use crate::services::provider::{CancelToken, ProviderKind};

use super::HealthRegistry;
use super::rebind_request::{ParsedRebindRequest, parse_rebind_body};
use super::snapshot::WatcherStateSnapshot;
use super::{relay_auto_heal, relay_dead_reattach, stall_liveness, watcher_respawn};

mod leak_recovery_ledger;
mod watchdog_decisions;

use leak_recovery_ledger::{
    LeakRecoveryLedgerIdentity, leak_recovery_clear_chunk_ledger,
    leak_recovery_confirmed_chunk_count, leak_recovery_confirmed_prefix_from_ledger,
    leak_recovery_fetch_continuation_contents, leak_recovery_message_matches_chunk,
    leak_recovery_record_confirmed_chunk, leak_recovery_unrelayed_range,
    render_leak_recovery_delivery,
};
pub(crate) use watchdog_decisions::{
    STALL_WATCHDOG_INITIAL_DELAY_SECS, STALL_WATCHDOG_INTERVAL_SECS,
    STALL_WATCHDOG_LIVENESS_FRESHNESS_SECS, STALL_WATCHDOG_THRESHOLD_SECS,
    completed_stale_no_answer_orphan_should_clean, inflight_completed_stale_leak_detected,
    stale_idle_foreground_queue_detected, stall_watchdog_should_force_clean,
    stall_watchdog_should_force_clean_orphan_explicit_background_work,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RuntimeTurnStopResult {
    pub lifecycle_path: &'static str,
    pub had_active_turn: bool,
    pub queue_depth: usize,
    pub persistent_inflight_cleared: bool,
    pub termination_recorded: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IdleTmuxStaleTurnRepairResult {
    pub had_active_turn: bool,
    pub has_pending_queue: bool,
    pub persistent_inflight_cleared: bool,
    pub runtime_session_cleared: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct IdleTmuxStaleTurnInflightPin {
    identity: discord::inflight::InflightTurnIdentity,
    finalizer_turn_id: u64,
    updated_at: String,
    save_generation: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct ProviderMailboxState {
    pub channel_id: u64,
    pub has_cancel_token: bool,
    pub queue_depth: usize,
    pub recovery_started: bool,
}

/// Resolve the runtime that owns `channel_id` for `provider`. Channel-aware
/// so multi-bot deployments (several runtimes under one provider name) stop,
/// drain, and snapshot the runtime that actually handles the channel rather
/// than whichever registered first. Single-bot deployments are unaffected.
async fn shared_for_provider(
    registry: &HealthRegistry,
    provider: &ProviderKind,
    channel_id: ChannelId,
) -> Option<Arc<SharedData>> {
    registry
        .shared_for_provider_on_channel(provider, channel_id)
        .await
}

async fn owning_runtime_http_for_channel(
    registry: &HealthRegistry,
    provider: &ProviderKind,
    channel_id: ChannelId,
) -> Option<Arc<serenity::http::Http>> {
    shared_for_provider(registry, provider, channel_id)
        .await
        .and_then(|shared| shared.serenity_http_or_token_fallback())
}

fn idle_tmux_repair_ready_for_input(
    provider: &ProviderKind,
    channel_id: u64,
    tmux_session: &str,
) -> bool {
    super::super::relay_recovery::idle_tmux_repair_ready_for_input(
        provider,
        channel_id,
        tmux_session,
    )
}

#[cfg(test)]
type IdleTmuxStaleTurnInflightCandidateHook =
    Arc<dyn Fn(&discord::inflight::InflightTurnState) + Send + Sync>;

#[cfg(test)]
type IdleTmuxStaleTurnPostClearHook =
    Arc<dyn Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> + Send + Sync>;

#[cfg(test)]
fn idle_tmux_stale_turn_inflight_candidate_hook()
-> &'static std::sync::Mutex<Option<IdleTmuxStaleTurnInflightCandidateHook>> {
    static HOOK: std::sync::OnceLock<
        std::sync::Mutex<Option<IdleTmuxStaleTurnInflightCandidateHook>>,
    > = std::sync::OnceLock::new();
    HOOK.get_or_init(|| std::sync::Mutex::new(None))
}

#[cfg(test)]
fn idle_tmux_stale_turn_post_clear_hook()
-> &'static std::sync::Mutex<Option<IdleTmuxStaleTurnPostClearHook>> {
    static HOOK: std::sync::OnceLock<std::sync::Mutex<Option<IdleTmuxStaleTurnPostClearHook>>> =
        std::sync::OnceLock::new();
    HOOK.get_or_init(|| std::sync::Mutex::new(None))
}

fn load_idle_tmux_stale_turn_inflight_clear_candidate(
    provider: &ProviderKind,
    channel_id: u64,
) -> Option<discord::inflight::InflightTurnState> {
    let state = discord::inflight::load_inflight_state(provider, channel_id)?;
    if !discord::inflight::inflight_state_allows_idle_tmux_repair_state(&state) {
        return None;
    }
    #[cfg(test)]
    if let Some(hook) = idle_tmux_stale_turn_inflight_candidate_hook()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .clone()
    {
        hook(&state);
    }
    Some(state)
}

fn capture_idle_tmux_stale_turn_inflight_pin(
    state: &discord::inflight::InflightTurnState,
) -> Option<IdleTmuxStaleTurnInflightPin> {
    let finalizer_turn_id = state.effective_finalizer_turn_id();
    (finalizer_turn_id != 0).then(|| IdleTmuxStaleTurnInflightPin {
        identity: discord::inflight::InflightTurnIdentity::from_state(state),
        finalizer_turn_id,
        updated_at: state.updated_at.clone(),
        save_generation: state.save_generation,
    })
}

fn clear_idle_tmux_stale_turn_inflight_if_pinned(
    provider: &ProviderKind,
    channel_id: u64,
    pin: Option<&IdleTmuxStaleTurnInflightPin>,
) -> discord::inflight::GuardedClearOutcome {
    let Some(pin) = pin else {
        return discord::inflight::GuardedClearOutcome::Missing;
    };
    let outcome = discord::inflight::clear_inflight_state_if_matches_identity_generation(
        provider,
        channel_id,
        &pin.identity,
        pin.finalizer_turn_id,
        &pin.updated_at,
        pin.save_generation,
    );
    match outcome {
        discord::inflight::GuardedClearOutcome::Cleared => {}
        other => {
            let current = discord::inflight::load_inflight_state(provider, channel_id);
            tracing::warn!(
                provider = %provider.as_str(),
                channel_id,
                clear_outcome = ?other,
                expected_user_msg_id = pin.identity.user_msg_id,
                expected_finalizer_turn_id = pin.finalizer_turn_id,
                expected_updated_at = %pin.updated_at,
                expected_save_generation = pin.save_generation,
                current_user_msg_id = current.as_ref().map(|state| state.user_msg_id).unwrap_or(0),
                current_finalizer_turn_id = current
                    .as_ref()
                    .map(|state| state.effective_finalizer_turn_id())
                    .unwrap_or(0),
                current_updated_at = %current
                    .as_ref()
                    .map(|state| state.updated_at.as_str())
                    .unwrap_or("<missing>"),
                current_save_generation = current.as_ref().map(|state| state.save_generation).unwrap_or(0),
                "idle tmux stale-turn repair skipped persistent inflight clear because the readiness-time pin was not cleared"
            );
        }
    }
    outcome
}

fn preserve_cancel_should_skip_provider_interrupt_for_idle_tui(
    provider: &ProviderKind,
    cleanup_policy: discord::TmuxCleanupPolicy,
    tmux_ready_for_input: bool,
    inflight_safe_to_clear: bool,
) -> bool {
    !cleanup_policy.should_cleanup_tmux()
        && matches!(provider, ProviderKind::Claude)
        && tmux_ready_for_input
        && inflight_safe_to_clear
}

fn cancel_token_tmux_session(token: &Arc<CancelToken>) -> Option<String> {
    token
        .tmux_session
        .lock()
        .ok()
        .and_then(|guard| guard.clone())
        .filter(|session| !session.trim().is_empty())
}

fn preserve_cancel_can_skip_provider_interrupt_for_idle_tui(
    provider: &ProviderKind,
    channel_id: ChannelId,
    token: &Arc<CancelToken>,
    cleanup_policy: discord::TmuxCleanupPolicy,
) -> bool {
    let Some(tmux_session) = cancel_token_tmux_session(token) else {
        return false;
    };
    let tmux_ready_for_input =
        idle_tmux_repair_ready_for_input(provider, channel_id.get(), &tmux_session);
    let inflight_safe_to_clear =
        discord::inflight_state_allows_idle_tmux_repair_for_channel(provider, channel_id.get())
            .unwrap_or(false);
    preserve_cancel_should_skip_provider_interrupt_for_idle_tui(
        provider,
        cleanup_policy,
        tmux_ready_for_input,
        inflight_safe_to_clear,
    )
}

async fn wait_for_turn_end(
    shared: &SharedData,
    channel_id: ChannelId,
    timeout: std::time::Duration,
) -> bool {
    let start = tokio::time::Instant::now();
    while shared.mailbox(channel_id).has_active_turn().await {
        if start.elapsed() >= timeout {
            return false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    true
}

fn runtime_stop_wait_timeout() -> std::time::Duration {
    std::time::Duration::from_secs(3)
}

fn clear_persistent_inflight_for_stop(
    provider: &ProviderKind,
    channel_id: ChannelId,
    was_present_at_stop_start: bool,
) -> bool {
    let removed_now = discord::clear_inflight_state(provider, channel_id.get());
    let disappeared_during_stop = was_present_at_stop_start
        && !discord::inflight::inflight_state_file_exists(provider, channel_id.get());
    removed_now || disappeared_during_stop
}

pub(crate) async fn stop_provider_channel_runtime_with_policy(
    registry: &HealthRegistry,
    provider_name: &str,
    channel_id: ChannelId,
    reason: &str,
    cleanup_policy: discord::TmuxCleanupPolicy,
) -> Option<RuntimeTurnStopResult> {
    let provider = ProviderKind::from_str(provider_name)?;
    let shared = shared_for_provider(registry, &provider, channel_id).await?;
    let cleanup_requested = cleanup_policy.should_cleanup_tmux();
    let should_clear_persistent_inflight = cleanup_policy.should_clear_inflight();
    let persistent_inflight_was_present = should_clear_persistent_inflight
        && discord::inflight::inflight_state_file_exists(&provider, channel_id.get());
    let result = discord::mailbox_cancel_active_turn(&shared, channel_id).await;
    let mut skipped_idle_provider_interrupt = false;

    if let Some(token) = result.token.as_ref() {
        let skip_provider_interrupt = preserve_cancel_can_skip_provider_interrupt_for_idle_tui(
            &provider,
            channel_id,
            token,
            cleanup_policy,
        );
        let termination_recorded = if skip_provider_interrupt {
            tracing::info!(
                provider = provider.as_str(),
                channel_id = channel_id.get(),
                reason,
                "preserve cancel skipped provider interrupt for idle Claude TUI turn"
            );
            skipped_idle_provider_interrupt = true;
            false
        } else if !result.already_stopping || cleanup_requested {
            discord::turn_bridge::stop_active_turn(&provider, token, cleanup_policy, reason).await
        } else {
            false
        };
        if wait_for_turn_end(&shared, channel_id, runtime_stop_wait_timeout()).await {
            let snapshot = shared.mailbox(channel_id).snapshot().await;
            let idle_inflight_cleared = if skipped_idle_provider_interrupt {
                discord::clear_inflight_state(&provider, channel_id.get())
            } else {
                false
            };
            return Some(RuntimeTurnStopResult {
                lifecycle_path: "canonical",
                had_active_turn: true,
                queue_depth: snapshot.intervention_queue.len(),
                persistent_inflight_cleared: idle_inflight_cleared
                    || (should_clear_persistent_inflight
                        && clear_persistent_inflight_for_stop(
                            &provider,
                            channel_id,
                            persistent_inflight_was_present,
                        )),
                termination_recorded,
            });
        }
    }

    let owned_role_override =
        discord::turn_finalizer::cleanup::snapshot_role_override(&shared, channel_id);
    let finish = discord::mailbox_finish_turn(&shared, &provider, channel_id).await;
    let mut termination_recorded = false;
    if let Some(token) = finish.removed_token.as_ref() {
        let skip_provider_interrupt = skipped_idle_provider_interrupt
            || preserve_cancel_can_skip_provider_interrupt_for_idle_tui(
                &provider,
                channel_id,
                token,
                cleanup_policy,
            );
        if skip_provider_interrupt {
            tracing::info!(
                provider = provider.as_str(),
                channel_id = channel_id.get(),
                reason,
                "runtime fallback skipped provider interrupt for idle Claude TUI turn"
            );
            skipped_idle_provider_interrupt = true;
        } else {
            termination_recorded =
                discord::turn_bridge::stop_active_turn(&provider, token, cleanup_policy, reason)
                    .await;
        }
    }
    apply_runtime_hard_stop_cleanup(
        &shared,
        &provider,
        channel_id,
        &finish,
        owned_role_override,
        "runtime_stop_fallback",
        cleanup_requested,
    )
    .await;
    let queue_depth = shared
        .mailbox(channel_id)
        .snapshot()
        .await
        .intervention_queue
        .len();
    discord::mailbox_clear_recovery_marker(&shared, channel_id).await;
    let idle_inflight_cleared = if skipped_idle_provider_interrupt {
        discord::clear_inflight_state(&provider, channel_id.get())
    } else {
        false
    };
    let persistent_inflight_cleared = idle_inflight_cleared
        || if should_clear_persistent_inflight {
            clear_persistent_inflight_for_stop(
                &provider,
                channel_id,
                persistent_inflight_was_present,
            )
        } else {
            false
        };

    Some(RuntimeTurnStopResult {
        lifecycle_path: "runtime-fallback",
        had_active_turn: finish.removed_token.is_some(),
        queue_depth,
        persistent_inflight_cleared,
        termination_recorded,
    })
}

pub async fn force_kill_provider_channel_runtime(
    registry: &HealthRegistry,
    provider_name: &str,
    channel_id: ChannelId,
    reason: &str,
    termination_reason_code: &'static str,
) -> Option<RuntimeTurnStopResult> {
    stop_provider_channel_runtime_with_policy(
        registry,
        provider_name,
        channel_id,
        reason,
        discord::TmuxCleanupPolicy::CleanupSession {
            termination_reason_code: Some(termination_reason_code),
        },
    )
    .await
}

/// #1672: Snapshot the per-channel pending-queue state from both the
/// in-memory mailbox and the disk-backed `discord_pending_queue` file.
///
/// Used by the cancel API + text-stop helpers to verify their
/// "pending_queue must be preserved across cancel" invariant *after*
/// the cancel completes, instead of asserting it via a hardcoded
/// `queue_preserved=true`.
///
/// Returns `None` only when the registered shared runtime cannot be
/// resolved for `provider_name`. A missing channel mailbox or absent
/// disk file are reported as `(0, false)` rather than `None`.
pub async fn snapshot_pending_queue_state(
    registry: &HealthRegistry,
    provider_name: &str,
    channel_id: ChannelId,
) -> Option<PendingQueueSnapshot> {
    let provider = ProviderKind::from_str(provider_name)?;
    let shared = shared_for_provider(registry, &provider, channel_id).await?;
    Some(snapshot_pending_queue_state_for_shared(&shared, &provider, channel_id).await)
}

#[derive(Clone, Debug, Default)]
pub struct PendingQueueSnapshot {
    pub queue_depth: usize,
    pub disk_present: bool,
    // #3034: populated for diagnostics/observability; not read by the queue
    // preservation check (which only compares depth + presence).
    #[allow(dead_code)]
    pub disk_path: Option<std::path::PathBuf>,
}

async fn snapshot_pending_queue_state_for_shared(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
) -> PendingQueueSnapshot {
    let queue_depth = shared
        .mailbox(channel_id)
        .snapshot()
        .await
        .intervention_queue
        .len();
    let disk_path = discord::runtime_store::discord_pending_queue_root().map(|root| {
        root.join(provider.as_str())
            .join(&shared.token_hash)
            .join(format!("{}.json", channel_id.get()))
    });
    let disk_present = disk_path
        .as_ref()
        .map(|path| path.exists())
        .unwrap_or(false);
    PendingQueueSnapshot {
        queue_depth,
        disk_present,
        disk_path,
    }
}

/// #1672: After a cancel that left the channel idle, kick the deferred
/// idle-queue drain so any survived `pending_queue` items are picked up
/// without requiring the next user message to arrive first.
///
/// Returns `true` when the drain was scheduled (registered shared runtime
/// found and at least one item is queued in memory or on disk), `false`
/// otherwise.
///
/// codex review round-3 P2: when the in-memory mailbox is empty but the
/// disk-backed `discord_pending_queue/<provider>/<token>/<channel>.json`
/// file is still present, hydrate the mailbox from disk before
/// scheduling the drain. Without this, the cancel response correctly
/// reports `queue_disk_present_after=true` but the queued items remain
/// stranded — the drain helper sees an empty mailbox and bails out, and
/// the next `mailbox_enqueue_intervention` may overwrite the disk file
/// before the items are ever absorbed.
pub async fn schedule_pending_queue_drain_after_cancel(
    registry: &HealthRegistry,
    provider_name: &str,
    channel_id: ChannelId,
    reason: &'static str,
) -> PostCancelDrainOutcome {
    let Some(provider) = ProviderKind::from_str(provider_name) else {
        return PostCancelDrainOutcome::skipped();
    };
    let Some(shared) = shared_for_provider(registry, &provider, channel_id).await else {
        return PostCancelDrainOutcome::skipped();
    };
    let snapshot = snapshot_pending_queue_state_for_shared(&shared, &provider, channel_id).await;
    // codex review round-4 P2-1 (#1672): hydrate from disk *whenever*
    // the disk file is present, not just when the in-memory queue is
    // empty. If a concurrent `mailbox_enqueue_intervention` slipped a
    // fresh message in between the cancel and this helper running, we
    // still need to merge whatever the disk holds. Actor-local hydrate
    // dedupes by `message_id` and prepends disk items so neither the
    // surviving disk payload nor the live racer is dropped.
    let post_depth = if snapshot.disk_present {
        let hydrate_result = hydrate_pending_queue_from_disk(&shared, &provider, channel_id).await;
        let _absorbed = hydrate_result.absorbed;
        hydrate_result.queue_len_after
    } else {
        snapshot.queue_depth
    };
    if post_depth == 0 {
        return PostCancelDrainOutcome {
            scheduled: false,
            queue_depth_after: 0,
        };
    }
    discord::schedule_deferred_idle_queue_kickoff(shared.clone(), provider, channel_id, reason);
    PostCancelDrainOutcome {
        scheduled: true,
        queue_depth_after: post_depth,
    }
}

/// codex review round-4 P2-2 (#1672): return value of
/// `schedule_pending_queue_drain_after_cancel`. The cancel response
/// builders use `queue_depth_after` as the source of truth for
/// `queued_remaining` so the API contract reflects the post-hydrate
/// state, not the (typically zero) snapshot taken before disk
/// hydration ran.
#[derive(Clone, Copy, Debug, Default)]
pub struct PostCancelDrainOutcome {
    // #3034: outcome flag retained for diagnostics; callers consume
    // `queue_depth_after` as the API source of truth (see doc above).
    #[allow(dead_code)]
    pub scheduled: bool,
    pub queue_depth_after: usize,
}

impl PostCancelDrainOutcome {
    fn skipped() -> Self {
        Self::default()
    }
}

/// codex review round-3 P2 (#1672): load the disk-backed pending queue
/// for `channel_id` and merge it into the in-memory mailbox. Restores
/// the matching `dispatch_role_override` alongside the queue so
/// requeued items target the same destination channel as the original
/// `mailbox_enqueue_intervention` call.
///
/// codex review round-4 P2-1 (#1672): the merge runs through the
/// mailbox actor, so a concurrent `mailbox_enqueue_intervention`
/// racing with this hydrate is preserved rather than clobbered. Disk
/// items are inserted at the head of the queue and any `message_id`
/// already present is skipped to keep the merge idempotent on retry.
///
/// #1683: the disk read also runs inside the actor message. A pending
/// dequeue can no longer remove the queue file after an out-of-actor
/// stale read and then have that stale payload reinserted by hydrate.
///
/// Returns the post-hydrate queue depth plus any restored role override.
async fn hydrate_pending_queue_from_disk(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
) -> crate::services::turn_orchestrator::HydratePendingQueueResult {
    let result =
        discord::mailbox_hydrate_pending_queue_from_disk(shared, provider, channel_id).await;
    if let Some(alt_channel_id) = result.restored_override {
        shared
            .dispatch
            .role_overrides
            .insert(channel_id, alt_channel_id);
    }
    result
}

/// #1672: Resolve a usable tmux session name for cancel observability.
///
/// Order: live tmux watcher binding → persistent inflight state file →
/// `discord_session.channel_name` rendered through the provider's tmux
/// naming convention. Returns `None` when none of those sources knows
/// about the channel — at which point cancel observability falls back
/// to whatever the caller passed in (typically empty).
pub async fn resolve_tmux_session_for_cancel(
    registry: &HealthRegistry,
    provider_name: &str,
    channel_id: ChannelId,
) -> Option<String> {
    let provider = ProviderKind::from_str(provider_name)?;
    let shared = shared_for_provider(registry, &provider, channel_id).await?;
    if let Some(binding) = shared.tmux_watchers.channel_binding(&channel_id) {
        return Some(binding.tmux_session_name);
    }
    if let Some(state) = discord::inflight::load_inflight_state(&provider, channel_id.get())
        && let Some(session) = state.tmux_session_name
    {
        return Some(session);
    }
    let data = shared.core.lock().await;
    data.sessions
        .get(&channel_id)
        .and_then(|session| session.channel_name.as_ref())
        .map(|channel_name| provider.build_tmux_session_name(channel_name))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HardStopRuntimeResult {
    pub cleanup_path: &'static str,
    pub had_active_turn: bool,
    pub has_pending_queue: bool,
    pub runtime_session_cleared: bool,
}

impl Default for HardStopRuntimeResult {
    fn default() -> Self {
        Self {
            cleanup_path: "runtime_unavailable_fallback",
            had_active_turn: false,
            has_pending_queue: false,
            runtime_session_cleared: false,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FinishCancelledMailboxResult {
    pub cleared_active_turn: bool,
    pub global_active_decremented: bool,
    pub has_pending_queue: bool,
    pub runtime_session_cleared: bool,
}

struct RuntimeChannelMatch {
    provider: ProviderKind,
    shared: Arc<SharedData>,
    channel_id: ChannelId,
}

#[derive(Clone, Copy)]
enum RuntimeChannelOwnershipMode {
    AllowProcessGlobalMailboxFallback,
    StrictPerRuntime,
}

async fn has_local_mailbox_ownership_evidence(
    shared: &SharedData,
    channel_id: ChannelId,
    ownership_mode: RuntimeChannelOwnershipMode,
) -> bool {
    let Some(handle) = shared.mailbox_peek(channel_id) else {
        return false;
    };

    if matches!(
        ownership_mode,
        RuntimeChannelOwnershipMode::AllowProcessGlobalMailboxFallback
    ) {
        return true;
    }

    let snapshot = handle.snapshot().await;
    // Snapshot/observation paths can materialize empty per-runtime mailboxes as
    // a side effect, so bare local-handle existence is not ownership evidence.
    snapshot.cancel_token.is_some() || !snapshot.intervention_queue.is_empty()
}

async fn find_runtime_channel_match(
    registry: &HealthRegistry,
    provider_name: Option<&str>,
    channel_id: Option<ChannelId>,
    tmux_name: Option<&str>,
    ownership_mode: RuntimeChannelOwnershipMode,
) -> Option<RuntimeChannelMatch> {
    let preferred_provider = provider_name.and_then(ProviderKind::from_str);
    let providers: Vec<_> = registry
        .providers
        .lock()
        .await
        .iter()
        .filter_map(|entry| {
            let provider = ProviderKind::from_str(&entry.name)?;
            if preferred_provider
                .as_ref()
                .is_some_and(|preferred| preferred != &provider)
            {
                return None;
            }
            Some((provider, entry.shared.clone()))
        })
        .collect();

    for (provider, shared) in providers {
        if let Some(channel_id) = channel_id {
            let has_session = {
                let data = shared.core.lock().await;
                data.sessions.contains_key(&channel_id)
            };
            let has_local_mailbox =
                has_local_mailbox_ownership_evidence(&shared, channel_id, ownership_mode).await;
            let has_process_global_mailbox =
                matches!(
                    ownership_mode,
                    RuntimeChannelOwnershipMode::AllowProcessGlobalMailboxFallback
                ) && discord::ChannelMailboxRegistry::global_handle(channel_id).is_some();
            if has_session || has_local_mailbox || has_process_global_mailbox {
                return Some(RuntimeChannelMatch {
                    provider,
                    shared,
                    channel_id,
                });
            }
            continue;
        }

        let Some(tmux_name) = tmux_name else {
            continue;
        };
        let matched_channel_id = {
            let data = shared.core.lock().await;
            data.sessions
                .iter()
                .find_map(|(candidate_channel_id, session)| {
                    session.channel_name.as_ref().and_then(|channel_name| {
                        let expected_tmux_name = provider.build_tmux_session_name(channel_name);
                        (expected_tmux_name == tmux_name).then_some(*candidate_channel_id)
                    })
                })
        };
        if let Some(channel_id) = matched_channel_id {
            return Some(RuntimeChannelMatch {
                provider,
                shared,
                channel_id,
            });
        }
    }

    None
}

async fn apply_runtime_hard_stop_cleanup(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    finish: &discord::FinishTurnResult,
    owned_role_override: Option<ChannelId>,
    stop_source: &'static str,
    stop_watcher: bool,
) -> bool {
    if let Some(token) = finish.removed_token.as_ref() {
        token.cancelled.store(true, Ordering::Relaxed);
        discord::saturating_decrement_global_active(shared);
    }

    discord::turn_finalizer::cleanup::clear_watchdog_and_kick_thread_parents_after_turn_release(
        shared, provider, channel_id,
    )
    .await;
    shared.restart.recovering_channels.remove(&channel_id);
    shared.turn_start_times.remove(&channel_id);

    if !finish.has_pending {
        discord::turn_finalizer::cleanup::remove_owned_role_override(
            shared,
            channel_id,
            owned_role_override,
        );
    }

    if stop_watcher && let Some((_, watcher)) = shared.tmux_watchers.remove(&channel_id) {
        watcher.cancel.store(true, Ordering::Relaxed);
    }

    let runtime_session_cleared = {
        let mut data = shared.core.lock().await;
        if let Some(session) = data.sessions.get_mut(&channel_id) {
            session.clear_provider_session();
            true
        } else {
            false
        }
    };

    if finish.mailbox_online && finish.has_pending {
        discord::schedule_deferred_idle_queue_kickoff(
            shared.clone(),
            provider.clone(),
            channel_id,
            stop_source,
        );
    }

    runtime_session_cleared
}

async fn apply_runtime_hard_stop_finalizer_cleanup_pre_release(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    has_pending: bool,
    owned_role_override: Option<ChannelId>,
    pinned_tmux_session_name: Option<&str>,
    stop_watcher: bool,
) -> bool {
    discord::turn_finalizer::cleanup::clear_watchdog_and_kick_thread_parents_after_turn_release(
        shared, provider, channel_id,
    )
    .await;
    shared.restart.recovering_channels.remove(&channel_id);
    shared.turn_start_times.remove(&channel_id);

    if !has_pending {
        discord::turn_finalizer::cleanup::remove_owned_role_override(
            shared,
            channel_id,
            owned_role_override,
        );
    }

    if stop_watcher {
        stop_hard_stop_cleanup_watcher_if_current(shared, channel_id, pinned_tmux_session_name);
    }

    {
        let mut data = shared.core.lock().await;
        if let Some(session) = data.sessions.get_mut(&channel_id) {
            session.clear_provider_session();
            true
        } else {
            false
        }
    }
}

fn stop_hard_stop_cleanup_watcher_if_current(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    pinned_tmux_session_name: Option<&str>,
) -> bool {
    let Some(pinned_tmux_session_name) = pinned_tmux_session_name else {
        tracing::info!(
            channel_id = channel_id.get(),
            "STALL-WATCHDOG: skipped hard-stop watcher cleanup because pinned tmux session is missing"
        );
        return false;
    };

    let guard = discord::lock_tmux_watcher_registry();
    let current_tmux_session_name = shared
        .tmux_watchers
        .channel_binding(&channel_id)
        .map(|binding| binding.tmux_session_name);
    if current_tmux_session_name.as_deref() != Some(pinned_tmux_session_name) {
        tracing::info!(
            channel_id = channel_id.get(),
            pinned_tmux_session = pinned_tmux_session_name,
            current_tmux_session = current_tmux_session_name.as_deref(),
            "STALL-WATCHDOG: skipped hard-stop watcher cleanup because channel watcher identity changed"
        );
        return false;
    }
    let Some((_, watcher)) = shared.tmux_watchers.remove_locked(&guard, &channel_id) else {
        return false;
    };
    watcher.cancel.store(true, Ordering::Relaxed);
    true
}

async fn cleanup_then_submit_explicit_background_watchdog_cancel<Submit, SubmitFuture>(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    revalidated: &ExplicitBackgroundWatchdogClear,
    pinned_tmux_session_name: Option<&str>,
    submit_terminal: Submit,
) -> bool
where
    Submit: FnOnce() -> SubmitFuture,
    SubmitFuture: std::future::Future<Output = discord::turn_finalizer::FinalizeOutcome>,
{
    debug_assert_eq!(
        revalidated.clear_outcome,
        discord::inflight::GuardedClearOutcome::Cleared
    );
    let owned_role_override =
        discord::turn_finalizer::cleanup::snapshot_role_override(shared, channel_id);
    let pre_release_has_pending = !shared
        .mailbox(channel_id)
        .snapshot()
        .await
        .intervention_queue
        .is_empty();
    let _runtime_session_cleared = apply_runtime_hard_stop_finalizer_cleanup_pre_release(
        shared,
        provider,
        channel_id,
        pre_release_has_pending,
        owned_role_override,
        pinned_tmux_session_name,
        true,
    )
    .await;
    let outcome = submit_terminal().await;
    match outcome {
        discord::turn_finalizer::FinalizeOutcome::Finalized { has_pending, .. } => has_pending,
        discord::turn_finalizer::FinalizeOutcome::AlreadyFinalized
        | discord::turn_finalizer::FinalizeOutcome::Deferred => pre_release_has_pending,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ExplicitBackgroundWatchdogClear {
    clear_outcome: discord::inflight::GuardedClearOutcome,
    pending_hourglass_user_msg_id: Option<u64>,
    finalizer_turn_id: u64,
}

fn revalidate_and_clear_explicit_background_inflight(
    provider: &ProviderKind,
    channel_id: ChannelId,
    snapshot_identity: Option<&discord::inflight::InflightTurnIdentity>,
    snapshot_finalizer_turn_id: Option<u64>,
) -> ExplicitBackgroundWatchdogClear {
    let Some(snapshot_identity) = snapshot_identity else {
        return ExplicitBackgroundWatchdogClear {
            clear_outcome: discord::inflight::GuardedClearOutcome::Missing,
            pending_hourglass_user_msg_id: None,
            finalizer_turn_id: 0,
        };
    };
    let finalizer_turn_id = snapshot_finalizer_turn_id.unwrap_or(snapshot_identity.user_msg_id);
    if finalizer_turn_id == 0 {
        return ExplicitBackgroundWatchdogClear {
            clear_outcome: discord::inflight::GuardedClearOutcome::UserMsgMismatch,
            pending_hourglass_user_msg_id: None,
            finalizer_turn_id,
        };
    }
    let clear_outcome = discord::inflight::clear_inflight_state_if_matches_identity(
        provider,
        channel_id.get(),
        snapshot_identity,
    );
    ExplicitBackgroundWatchdogClear {
        clear_outcome,
        pending_hourglass_user_msg_id: (snapshot_identity.user_msg_id != 0)
            .then_some(snapshot_identity.user_msg_id),
        finalizer_turn_id,
    }
}

pub async fn hard_stop_runtime_turn(
    registry: Option<&HealthRegistry>,
    provider_name: Option<&str>,
    channel_id: Option<u64>,
    tmux_name: Option<&str>,
    stop_source: &'static str,
) -> HardStopRuntimeResult {
    runtime_turn_cleanup_by_lookup(
        registry,
        provider_name,
        channel_id,
        tmux_name,
        stop_source,
        true,
        RuntimeChannelOwnershipMode::AllowProcessGlobalMailboxFallback,
    )
    .await
}

pub async fn clear_idle_tmux_stale_turn(
    registry: &HealthRegistry,
    provider_name: &str,
    channel_id: u64,
    tmux_session: &str,
    stop_source: &'static str,
) -> Option<IdleTmuxStaleTurnRepairResult> {
    let repair_started_at = Instant::now();
    let provider = ProviderKind::from_str(provider_name)?;
    let inflight_clear_state =
        load_idle_tmux_stale_turn_inflight_clear_candidate(&provider, channel_id)?;
    if !crate::services::discord::relay_recovery::idle_tmux_repair_state_ready_for_input(
        &provider,
        channel_id,
        tmux_session,
        &inflight_clear_state,
    ) {
        return None;
    }
    if crate::services::discord::relay_recovery::idle_tmux_repair_has_unrelayed_tail_answer(
        &inflight_clear_state,
    ) {
        tracing::warn!(
            provider = %provider.as_str(),
            channel_id,
            tmux_session,
            "idle tmux stale-turn repair skipped mailbox/runtime teardown because helper-load found an unrelayed tail answer"
        );
        return None;
    }
    let inflight_pin = capture_idle_tmux_stale_turn_inflight_pin(&inflight_clear_state);

    let channel_id = ChannelId::new(channel_id);
    let shared = shared_for_provider(registry, &provider, channel_id).await?;
    let inflight_clear_outcome = clear_idle_tmux_stale_turn_inflight_if_pinned(
        &provider,
        channel_id.get(),
        inflight_pin.as_ref(),
    );
    if inflight_clear_outcome != discord::inflight::GuardedClearOutcome::Cleared {
        tracing::warn!(
            provider = %provider.as_str(),
            channel_id = channel_id.get(),
            clear_outcome = ?inflight_clear_outcome,
            "idle tmux stale-turn repair skipped mailbox/runtime teardown because guarded persistent inflight clear did not succeed"
        );
        return None;
    }
    let owned_role_override =
        discord::turn_finalizer::cleanup::snapshot_role_override(&shared, channel_id);
    #[cfg(test)]
    {
        // Bind the cloned hook first so the mutex guard (a non-Send temporary
        // in an `if let` scrutinee would live across the await) drops here.
        let hook = idle_tmux_stale_turn_post_clear_hook()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone();
        if let Some(hook) = hook {
            hook().await;
        }
    }
    let expected_user_msg_id = inflight_pin
        .as_ref()
        .map(|pin| pin.identity.user_msg_id)
        .unwrap_or(0);
    let finish = if expected_user_msg_id != 0 {
        discord::mailbox_finish_turn_if_matches_started_before(
            &shared,
            &provider,
            channel_id,
            MessageId::new(expected_user_msg_id),
            repair_started_at,
        )
        .await
    } else {
        let snapshot = discord::mailbox_snapshot(&shared, channel_id).await;
        discord::FinishTurnResult {
            removed_token: None,
            has_pending: !snapshot.intervention_queue.is_empty(),
            mailbox_online: shared.mailbox_peek(channel_id).is_some(),
            queue_exit_events: Vec::new(),
            persistence_error: None,
        }
    };
    let runtime_session_cleared = if finish.removed_token.is_some() {
        apply_runtime_hard_stop_cleanup(
            &shared,
            &provider,
            channel_id,
            &finish,
            owned_role_override,
            stop_source,
            false,
        )
        .await
    } else {
        false
    };

    Some(IdleTmuxStaleTurnRepairResult {
        had_active_turn: finish.removed_token.is_some(),
        has_pending_queue: finish.has_pending,
        persistent_inflight_cleared: true,
        runtime_session_cleared,
    })
}

pub async fn provider_channel_mailbox_state(
    registry: &HealthRegistry,
    provider_name: &str,
    channel_id: u64,
) -> Option<ProviderMailboxState> {
    let provider = ProviderKind::from_str(provider_name)?;
    let channel = ChannelId::new(channel_id);
    let shared = shared_for_provider(registry, &provider, channel).await?;
    // #3293 (c): peek, never create — the previous `shared.mailbox()` probe
    // minted a permanent registry entry for every queried channel id.
    Some(super::mailbox::peeked_provider_mailbox_state(&shared, channel_id).await)
}

pub async fn stop_runtime_turn_preserving_watcher(
    registry: Option<&HealthRegistry>,
    provider_name: Option<&str>,
    channel_id: Option<u64>,
    tmux_name: Option<&str>,
    stop_source: &'static str,
) -> HardStopRuntimeResult {
    runtime_turn_cleanup_by_lookup(
        registry,
        provider_name,
        channel_id,
        tmux_name,
        stop_source,
        false,
        RuntimeChannelOwnershipMode::AllowProcessGlobalMailboxFallback,
    )
    .await
}

pub async fn stop_providerless_runtime_turn_preserving_watcher_strict_ownership(
    registry: Option<&HealthRegistry>,
    channel_id: u64,
    stop_source: &'static str,
) -> HardStopRuntimeResult {
    runtime_turn_cleanup_by_lookup(
        registry,
        None,
        Some(channel_id),
        None,
        stop_source,
        false,
        RuntimeChannelOwnershipMode::StrictPerRuntime,
    )
    .await
}

pub async fn finish_cancelled_provider_channel_mailbox(
    registry: Option<&HealthRegistry>,
    provider_name: Option<&str>,
    channel_id: Option<u64>,
    stop_source: &'static str,
) -> FinishCancelledMailboxResult {
    let Some(registry) = registry else {
        return FinishCancelledMailboxResult::default();
    };
    let Some(channel_id) = channel_id.map(ChannelId::new) else {
        return FinishCancelledMailboxResult::default();
    };
    let Some(runtime) = find_runtime_channel_match(
        registry,
        provider_name,
        Some(channel_id),
        None,
        RuntimeChannelOwnershipMode::AllowProcessGlobalMailboxFallback,
    )
    .await
    else {
        return FinishCancelledMailboxResult::default();
    };

    let before = runtime.shared.restart.global_active.load(Ordering::Acquire);
    let owned_role_override =
        discord::turn_finalizer::cleanup::snapshot_role_override(&runtime.shared, channel_id);
    let finish = discord::mailbox_finish_cancelled_turn(&runtime.shared, channel_id).await;
    if finish.removed_token.is_none() {
        return FinishCancelledMailboxResult {
            cleared_active_turn: false,
            global_active_decremented: false,
            has_pending_queue: finish.has_pending,
            runtime_session_cleared: false,
        };
    }

    let runtime_session_cleared = apply_runtime_hard_stop_cleanup(
        &runtime.shared,
        &runtime.provider,
        channel_id,
        &finish,
        owned_role_override,
        stop_source,
        true,
    )
    .await;
    let after = runtime.shared.restart.global_active.load(Ordering::Acquire);
    let global_active_decremented = after < before;
    if !global_active_decremented {
        tracing::warn!(
            provider = runtime.provider.as_str(),
            channel_id = channel_id.get(),
            global_active_before = before,
            global_active_after = after,
            stop_source,
            "finished cancelled mailbox turn without decrementing global_active"
        );
    }
    FinishCancelledMailboxResult {
        cleared_active_turn: true,
        global_active_decremented,
        has_pending_queue: finish.has_pending,
        runtime_session_cleared,
    }
}

async fn runtime_turn_cleanup_by_lookup(
    registry: Option<&HealthRegistry>,
    provider_name: Option<&str>,
    channel_id: Option<u64>,
    tmux_name: Option<&str>,
    stop_source: &'static str,
    stop_watcher: bool,
    ownership_mode: RuntimeChannelOwnershipMode,
) -> HardStopRuntimeResult {
    let channel_id = channel_id.map(ChannelId::new);

    if let Some(registry) = registry
        && let Some(runtime) = find_runtime_channel_match(
            registry,
            provider_name,
            channel_id,
            tmux_name,
            ownership_mode,
        )
        .await
    {
        let owned_role_override = discord::turn_finalizer::cleanup::snapshot_role_override(
            &runtime.shared,
            runtime.channel_id,
        );
        let finish =
            discord::mailbox_finish_turn(&runtime.shared, &runtime.provider, runtime.channel_id)
                .await;
        let runtime_session_cleared = apply_runtime_hard_stop_cleanup(
            &runtime.shared,
            &runtime.provider,
            runtime.channel_id,
            &finish,
            owned_role_override,
            stop_source,
            stop_watcher,
        )
        .await;
        return HardStopRuntimeResult {
            cleanup_path: if finish.mailbox_online {
                "mailbox_canonical"
            } else {
                "mailbox_fallback"
            },
            had_active_turn: finish.removed_token.is_some(),
            has_pending_queue: finish.has_pending,
            runtime_session_cleared,
        };
    }

    if let Some(channel_id) = channel_id
        && let Some(handle) = discord::ChannelMailboxRegistry::global_handle(channel_id)
    {
        let finish = handle.hard_stop().await;
        if finish.has_pending {
            discord::turn_completion_events::warn_unresolvable_hard_stop_pending_backlog(
                channel_id,
                finish.has_pending,
                stop_source,
            );
        }
        discord::clear_watchdog_deadline_override(channel_id.get()).await;
        return HardStopRuntimeResult {
            cleanup_path: if finish.mailbox_online {
                "mailbox_canonical"
            } else {
                "mailbox_fallback"
            },
            had_active_turn: finish.removed_token.is_some(),
            has_pending_queue: finish.has_pending,
            runtime_session_cleared: false,
        };
    }

    HardStopRuntimeResult::default()
}

/// Best-effort runtime-side equivalent of `/clear` for an existing Discord channel session.
/// Used by auto-queue slot recycling so pooled unified-thread slots start the next group fresh
/// without killing the shared thread itself.
pub async fn clear_provider_channel_runtime(
    registry: &HealthRegistry,
    provider_name: &str,
    channel_id: ChannelId,
    session_key: Option<&str>,
) -> bool {
    let Some(provider) = ProviderKind::from_str(provider_name) else {
        return false;
    };

    let shared = {
        let providers = registry.providers.lock().await;
        providers
            .iter()
            .find(|entry| entry.name.eq_ignore_ascii_case(provider.as_str()))
            .map(|entry| entry.shared.clone())
    };
    let Some(shared) = shared else {
        return false;
    };

    let tmux_name = {
        let data = shared.core.lock().await;
        data.sessions
            .get(&channel_id)
            .and_then(|session| session.channel_name.as_ref())
            .map(|channel_name| provider.build_tmux_session_name(channel_name))
            .or_else(|| session_key.and_then(tmux_name_from_session_key))
    };

    let cleared = discord::mailbox_clear_channel(&shared, &provider, channel_id).await;
    if let Some(token) = cleared.removed_token {
        discord::turn_bridge::stop_active_turn(
            &provider,
            &token,
            discord::TmuxCleanupPolicy::PreserveSession,
            "auto-queue slot clear",
        )
        .await;
        discord::saturating_decrement_global_active(&shared);
    }

    {
        let mut data = shared.core.lock().await;
        if let Some(session) = data.sessions.get_mut(&channel_id) {
            discord::settings::cleanup_channel_uploads(channel_id);
            session.clear_provider_session();
            session.history.clear();
            session.pending_uploads.clear();
            session.cleared = true;
        }
    }

    #[cfg(unix)]
    if let Some(name) = tmux_name {
        if provider.uses_managed_tmux_backend() {
            discord::commands::reset_managed_process_session(&name);
        }
    }

    true
}

/// #896: Handle `POST /api/inflight/rebind` — rebind a live tmux session to
/// a freshly-created inflight state and respawn the watcher. Used to recover
/// orphan states where tmux is still running but turn_bridge has no inflight
/// to track against (e.g. after a prior turn's cleanup cleared the state and
/// the agent continued work via internal auto-triggers).
///
/// Body shape:
/// ```json
/// { "provider": "claude" | "codex" | "gemini" | "opencode" | "qwen",
///   "channel_id": "1234567890",
///   "tmux_session": "AgentDesk-codex-foo",  // optional — derived otherwise
///   "output_path": "/path/to/live.jsonl",   // optional operator override
///   "session_id": "provider-session-uuid"   // optional operator override
/// }
/// ```
pub async fn handle_rebind_inflight<'a>(
    registry: &HealthRegistry,
    body: &str,
) -> (&'a str, String) {
    let parsed = match parse_rebind_body(body) {
        Ok(p) => p,
        Err((status, body)) => return (status, body),
    };
    let ParsedRebindRequest {
        provider,
        channel_id,
        tmux_session: tmux_override,
        overrides,
    } = parsed;

    let Some(result) = registry
        .rebind_inflight(&provider, channel_id, tmux_override, overrides)
        .await
    else {
        // #897 counter-model review: dcserver bootstrap registers the
        // `ProviderEntry` before the provider's Discord HTTP client, so a
        // lookup miss here can mean EITHER permanent misconfiguration OR a
        // transient warmup window. The error text now tells operators to
        // retry instead of assuming the provider is permanently absent.
        return (
            "503 Service Unavailable",
            format!(
                r#"{{"ok":false,"error":"provider {} is not yet available in this dcserver (still warming up or not registered) — retry in a few seconds"}}"#,
                provider.as_str()
            ),
        );
    };

    match result {
        Ok(outcome) => (
            "200 OK",
            serde_json::json!({
                "ok": true,
                "tmux_session": outcome.tmux_session,
                "channel_id": outcome.channel_id.to_string(),
                "initial_offset": outcome.initial_offset,
                "watcher_spawned": outcome.watcher_spawned,
                "watcher_replaced": outcome.watcher_replaced,
            })
            .to_string(),
        ),
        Err(err) => {
            let (status, message) = rebind_error_status_and_message(&err);
            (
                status,
                serde_json::json!({ "ok": false, "error": message }).to_string(),
            )
        }
    }
}

/// #1462: Handle relay recovery dry-run / bounded auto-heal for one channel.
///
/// `apply=false` is the default and only returns the proposed action with
/// evidence. `apply=true` is intentionally conservative: only local,
/// idempotent cleanup paths marked eligible by the recovery planner can run.
pub async fn handle_relay_recovery<'a>(
    registry: &HealthRegistry,
    provider: Option<&str>,
    channel_id: u64,
    apply: bool,
) -> (&'a str, String) {
    match discord::relay_recovery::run_relay_recovery(registry, provider, channel_id, apply).await {
        Ok(response) => (
            "200 OK",
            serde_json::to_string(&response).unwrap_or_else(|error| {
                serde_json::json!({
                    "ok": false,
                    "error": format!("failed to serialize relay recovery response: {error}")
                })
                .to_string()
            }),
        ),
        Err(error) => (error.status_str(), error.body().to_string()),
    }
}

pub(super) fn rebind_error_status_and_message(
    err: &discord::recovery_engine::RebindError,
) -> (&'static str, String) {
    let status = match err {
        discord::recovery_engine::RebindError::TmuxNotAlive { .. } => "404 Not Found",
        discord::recovery_engine::RebindError::InflightAlreadyExists
        | discord::recovery_engine::RebindError::StaleOutputPath { .. }
        | discord::recovery_engine::RebindError::RuntimeBindingUnavailable { .. } => "409 Conflict",
        discord::recovery_engine::RebindError::ChannelNotBound
        | discord::recovery_engine::RebindError::ChannelNameMissing => "400 Bad Request",
        discord::recovery_engine::RebindError::Internal(_) => "500 Internal Server Error",
    };
    (status, err.to_string())
}

#[cfg(test)]
mod rebind_error_status_tests {
    use super::rebind_error_status_and_message;
    use crate::services::agent_protocol::RuntimeHandoffKind;
    use crate::services::discord::recovery_engine::RebindError;

    #[test]
    fn runtime_binding_unavailable_maps_to_conflict() {
        let err = RebindError::RuntimeBindingUnavailable {
            tmux_session: "AgentDesk-codex-adk-cdx".to_string(),
            runtime_kind: RuntimeHandoffKind::CodexTui,
        };

        let (status, message) = rebind_error_status_and_message(&err);

        assert_eq!(status, "409 Conflict");
        assert!(message.contains("codex_tui"));
        assert!(message.contains("AgentDesk-codex-adk-cdx"));
    }
}

/// Self-watchdog: runs on a dedicated OS thread (not tokio) to detect
/// runtime hangs.  Periodically opens a raw TCP connection to the server
/// port and expects a response within a few seconds.  If the check fails
/// `max_failures` times in a row the process is force-killed so launchd
/// (or systemd) can restart it.
pub fn spawn_watchdog(port: u16) {
    const CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
    const TCP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
    const MAX_FAILURES: u32 = 3;
    // Grace period: skip checks for the first 30s after startup so the
    // runtime has time to initialise Discord bots and register providers.
    const STARTUP_GRACE: std::time::Duration = std::time::Duration::from_secs(30);

    std::thread::Builder::new()
        .name("health-watchdog".into())
        .spawn(move || {
            std::thread::sleep(STARTUP_GRACE);

            let mut consecutive_failures: u32 = 0;

            loop {
                std::thread::sleep(CHECK_INTERVAL);

                let ok = (|| -> bool {
                    use std::io::{Read, Write};
                    let loopback = crate::config::loopback();
                    let addr = format!("{loopback}:{port}");
                    let mut stream =
                        match std::net::TcpStream::connect_timeout(
                            &addr.parse().unwrap(),
                            TCP_TIMEOUT,
                        ) {
                            Ok(s) => s,
                            Err(_) => return false,
                        };
                    let _ = stream.set_read_timeout(Some(TCP_TIMEOUT));
                    let _ = stream.set_write_timeout(Some(TCP_TIMEOUT));
                    let req = format!("GET /api/health HTTP/1.1\r\nHost: {loopback}\r\nConnection: close\r\n\r\n");
                    if stream.write_all(req.as_bytes()).is_err() {
                        return false;
                    }
                    let mut buf = [0u8; 512];
                    match stream.read(&mut buf) {
                        Ok(n) if n > 0 => {
                            // Any HTTP response means the process is alive and serving.
                            // Only TCP failure (Err/_) indicates a true hang/deadlock.
                            // A 503 (degraded/unhealthy state) still means the runtime is
                            // responsive — killing it would create an infinite crash loop
                            // when a provider is temporarily disconnected.
                            true
                        }
                        _ => false,
                    }
                })();

                if ok {
                    if consecutive_failures > 0 {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::warn!(
                            "  [{ts}] 🩺 watchdog: health recovered after {consecutive_failures} failure(s)"
                        );
                    }
                    consecutive_failures = 0;
                } else {
                    consecutive_failures += 1;
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::warn!(
                        "  [{ts}] 🩺 watchdog: health check failed ({consecutive_failures}/{MAX_FAILURES})"
                    );
                    if consecutive_failures >= MAX_FAILURES {
                        tracing::warn!(
                            "  [{ts}] 🩺 watchdog: runtime unresponsive — capturing diagnostics before exit"
                        );
                        // Capture process dump for post-mortem analysis (platform-aware)
                        // Write to runtime root's logs/ dir so dumps survive /tmp cleanup
                        let pid = std::process::id();
                        let dump_dir = crate::agentdesk_runtime_root()
                            .map(|r| r.join("logs"))
                            .unwrap_or_else(|| std::env::temp_dir());
                        let _ = std::fs::create_dir_all(&dump_dir);
                        let dump_path = format!(
                            "{}/adk-hang-{}-{}.txt",
                            dump_dir.display(),
                            pid,
                            chrono::Local::now().format("%Y%m%d-%H%M%S")
                        );
                        match crate::services::platform::capture_process_dump(pid, &dump_path) {
                            Ok(()) => tracing::warn!(
                                "  [{ts}] 🩺 watchdog: dump saved to {dump_path} — forcing exit"
                            ),
                            Err(e) => tracing::warn!(
                                "  [{ts}] 🩺 watchdog: dump capture failed ({e}) — forcing exit without diagnostics"
                            ),
                        }
                        std::process::exit(1);
                    }
                }
            }
        })
        .expect("Failed to spawn watchdog thread");
}

/// #4460: reason_code for the "suspected stall" mention that REPLACED the
/// branch-4 force-clean. A stable reason_code lets the `message_outbox`
/// dedupe key coalesce repeated pages inside the cooldown window.
const STALL_WATCHDOG_MENTION_REASON_CODE: &str = "stall_watchdog_suspected_stall";

/// #4460: per-channel page cooldown (seconds). Modeled on
/// `long_turn_watchdog`'s 1800s alert cooldown so a persistently-suspect turn
/// pages AT MOST once per this window per channel instead of on every pass.
const STALL_WATCHDOG_MENTION_COOLDOWN_SECS: i64 = 1800;

/// #4460: replacement for the removed branch-4 force-clean execution. Posts a
/// rate-limited Discord alert to the channel via the deduped `message_outbox`
/// path and NEVER terminates, cancels, or deletes anything. Modeled on
/// `long_turn_watchdog::enqueue_alert`: a stable reason_code + per-channel
/// session key collapse repeated pages into one row per cooldown window.
async fn notify_suspected_stall_without_cleanup(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    // Reserved for future detection tightening (verdict-specific copy); the
    // turn is intentionally left untouched, so it is not read today. (#4460)
    _snapshot: &WatcherStateSnapshot,
    inflight: Option<&discord::inflight::InflightTurnState>,
) {
    let Some(pool) = shared.pg_pool.as_ref() else {
        tracing::warn!(
            "  [stall-watchdog] #4460 suspected stall on channel {} (provider={}) but no pg_pool — skipping mention (never terminating)",
            channel_id,
            provider.as_str(),
        );
        return;
    };
    // Resolve the turn owner for a `<@id>` mention when available. The inflight
    // state carries the requesting user's id (`request_owner_user_id`); a zero
    // means "unknown", in which case we omit the mention but still post.
    let mention = inflight
        .map(|state| state.request_owner_user_id)
        .filter(|id| *id != 0)
        .map(|id| format!(" <@{id}>"))
        .unwrap_or_default();
    let content = format!(
        "⚠️ 스톨 의심: 이 세션이 오래 응답이 없어 보입니다{mention}. 워치독은 더 이상 자동 종료하지 않습니다 — 실제로 멈췄다면 취소해 주시고, 정상 작업 중이면 무시하세요. (채널 {channel_id})"
    );
    let target = channel_id.get().to_string();
    // Per-channel dedupe key: a persistently-suspect turn pages at most once
    // per cooldown per channel.
    let session_key = format!("stall-watchdog-mention:{channel_id}");
    match crate::services::message_outbox::enqueue_outbox_pg_with_ttl(
        pool,
        crate::services::message_outbox::OutboxMessage {
            target: &target,
            content: &content,
            bot: "notify",
            source: "stall_watchdog",
            reason_code: Some(STALL_WATCHDOG_MENTION_REASON_CODE),
            session_key: Some(&session_key),
        },
        STALL_WATCHDOG_MENTION_COOLDOWN_SECS,
    )
    .await
    {
        Ok(true) => {
            tracing::warn!(
                "  [stall-watchdog] #4460 suspected stall — mentioned owner on channel {} (provider={}) WITHOUT force-clean",
                channel_id,
                provider.as_str(),
            );
        }
        Ok(false) => {
            tracing::debug!(
                "  [stall-watchdog] #4460 suspected stall mention suppressed by cooldown ({}s) on channel {}",
                STALL_WATCHDOG_MENTION_COOLDOWN_SECS,
                channel_id,
            );
        }
        Err(error) => {
            tracing::warn!(
                "  [stall-watchdog] #4460 suspected stall mention enqueue failed on channel {}: {error}",
                channel_id,
            );
        }
    }
}

/// Run a single stall-watchdog pass against one provider+SharedData.
///
/// Iterates every attached watcher and cleans channels whose snapshot satisfies
/// the watchdog predicates. Returns the number of channels cleaned this pass.
pub(crate) async fn run_stall_watchdog_pass(
    registry: &HealthRegistry,
    provider: &ProviderKind,
) -> usize {
    let now_unix_secs = chrono::Utc::now().timestamp();
    stall_liveness::gc_stall_watchdog_liveness_state(now_unix_secs);
    watcher_respawn::gc_watcher_absence_state(now_unix_secs);

    // Sweep every same-provider runtime; name-only lookup would miss later
    // bots, so keep the runtime that exposed each watcher.
    let runtimes = registry.all_shared_for_provider(provider).await;
    if runtimes.is_empty() {
        return 0;
    }
    let mut candidate_channels: Vec<(ChannelId, Arc<SharedData>)> = Vec::new();
    let mut seen: std::collections::HashSet<ChannelId> = std::collections::HashSet::new();
    for runtime in &runtimes {
        let runtime_channels: Vec<ChannelId> = runtime
            .tmux_watchers
            .iter()
            .filter_map(|entry| {
                runtime
                    .tmux_watchers
                    .owner_channel_for_tmux_session(entry.key())
            })
            .collect();
        for channel_id in runtime_channels {
            if seen.insert(channel_id) {
                candidate_channels.push((channel_id, runtime.clone()));
            }
        }
    }
    // #3410 P1-a: no early return on empty candidates; trailing retry/dead-man
    // are keyed on watcher absence, not live-watcher candidates.
    let mut cleaned = 0usize;
    for (channel_id, shared) in candidate_channels {
        // Use the selected runtime so same-provider multi-bot snapshots target
        // the bot that owns this watcher.
        let snapshot = match registry
            .snapshot_watcher_state_for_shared(provider, shared.clone(), channel_id.get())
            .await
        {
            Some(snapshot) => snapshot,
            None => continue,
        };
        if relay_dead_reattach::try_apply(
            registry,
            shared.clone(),
            provider,
            channel_id,
            &snapshot,
            now_unix_secs,
        )
        .await
        {
            cleaned += 1;
            continue;
        }
        // #3668 F2: if JSONL still holds an unrelayed final answer after
        // `last_offset`, skip destructive watchdog branches this tick.
        if let Some(state) = discord::inflight::load_inflight_state(provider, channel_id.get())
            && crate::services::discord::relay_recovery::idle_tmux_repair_has_unrelayed_tail_answer(
                &state,
            )
        {
            continue;
        }
        // #2965: a ready-for-input TUI can still look "desynced" when the
        // capture file has bytes past relay offsets. Prefer the idle-safe
        // anchor cleanup before the destructive desynced force-clean branch.
        if stale_idle_foreground_queue_detected(
            snapshot.relay_health.active_turn,
            snapshot.relay_health.mailbox_has_cancel_token,
            snapshot.relay_health.queue_depth,
            snapshot.inflight_state_present,
            snapshot.inflight_updated_at.as_deref(),
            snapshot.tmux_session_alive,
            snapshot.relay_health.last_outbound_activity_ms,
            now_unix_secs,
            STALL_WATCHDOG_THRESHOLD_SECS,
        ) && let Some(tmux_session) = snapshot.tmux_session.clone()
            // #3169: same loop-mid-write liveness guard as the desynced force-
            // clean below — a freshly-written jsonl means this idle-foreground
            // anchor is a live loop turn, not a stranded one, so do not clear it.
            && !stall_liveness::stall_watchdog_jsonl_liveness_defers_force_clean(
                crate::services::dispatched_sessions::latest_runtime_activity_unix_nanos(
                    &tmux_session,
                ),
                now_unix_secs,
                STALL_WATCHDOG_LIVENESS_FRESHNESS_SECS,
            )
            && idle_tmux_repair_ready_for_input(provider, channel_id.get(), &tmux_session)
            && discord::inflight_state_allows_idle_tmux_repair_for_channel(
                provider,
                channel_id.get(),
            )
            .unwrap_or(false)
        {
            let Some(result) = clear_idle_tmux_stale_turn(
                registry,
                provider.as_str(),
                channel_id.get(),
                &tmux_session,
                "2965_stale_idle_foreground_queue_watchdog",
            )
            .await
            else {
                continue;
            };
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ⚡ STALL-WATCHDOG: cleared idle foreground TUI turn for channel {} (provider={}, pending_queue={}, inflight_cleared={})",
                channel_id,
                provider.as_str(),
                result.has_pending_queue,
                result.persistent_inflight_cleared
            );
            cleaned += 1;
            continue;
        }

        if stall_watchdog_should_force_clean_orphan_explicit_background_work(
            snapshot.relay_stall_state,
            snapshot.attached,
            snapshot.watcher_owner_channel_id,
            channel_id.get(),
            snapshot.desynced,
            snapshot.inflight_state_present,
            snapshot.inflight_updated_at.as_deref(),
            snapshot.tmux_session_alive,
            snapshot.unread_bytes,
            snapshot.relay_health.last_outbound_activity_ms,
            now_unix_secs,
            STALL_WATCHDOG_THRESHOLD_SECS,
        ) {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ⚡ STALL-WATCHDOG: forced cleanup for orphan explicit background work in channel {}",
                channel_id
            );
            let revalidated = revalidate_and_clear_explicit_background_inflight(
                provider,
                channel_id,
                snapshot.inflight_identity.as_ref(),
                snapshot.inflight_finalizer_turn_id,
            );
            if revalidated.clear_outcome != discord::inflight::GuardedClearOutcome::Cleared {
                tracing::info!(
                    provider = %provider.as_str(),
                    channel_id = channel_id.get(),
                    clear_outcome = ?revalidated.clear_outcome,
                    "STALL-WATCHDOG: skipped orphan explicit background cleanup because inflight identity changed before the destructive clear"
                );
                continue;
            }
            let finalizer_turn_id = revalidated.finalizer_turn_id;
            let generation = shared.restart.current_generation;
            let shared_for_submit = shared.clone();
            let provider_for_submit = provider.clone();
            let has_pending = cleanup_then_submit_explicit_background_watchdog_cancel(
                &shared,
                provider,
                channel_id,
                &revalidated,
                snapshot.tmux_session.as_deref(),
                move || async move {
                    shared_for_submit
                        .turn_finalizer
                        .submit_terminal(
                            discord::turn_finalizer::TurnKey::new(
                                channel_id,
                                finalizer_turn_id,
                                generation,
                            ),
                            provider_for_submit,
                            discord::turn_finalizer::TerminalEvent::Cancel,
                            discord::turn_finalizer::FinalizeContext::monitor(),
                            shared_for_submit.clone(),
                        )
                        .await
                },
            )
            .await;
            if !has_pending {
                let hydrate = hydrate_pending_queue_from_disk(&shared, provider, channel_id).await;
                if hydrate.queue_len_after > 0 && hydrate.persistence_error.is_none() {
                    discord::schedule_deferred_idle_queue_kickoff(
                        shared.clone(),
                        provider.clone(),
                        channel_id,
                        "2967_orphan_explicit_background_watchdog",
                    );
                }
            }
            crate::services::observability::emit_inflight_lifecycle_event(
                provider.as_str(),
                channel_id.get(),
                None,
                None,
                Some(&format!(
                    "discord:{}:{}",
                    channel_id.get(),
                    revalidated.finalizer_turn_id
                )),
                "cleared_by_explicit_background_watchdog",
                serde_json::json!({
                    "clear_outcome": format!("{:?}", revalidated.clear_outcome),
                    "finalizer_turn_id": revalidated.finalizer_turn_id,
                }),
            );
            if let Some(user_msg_id) = revalidated.pending_hourglass_user_msg_id {
                tv_clear(
                    &shared,
                    channel_id,
                    user_msg_id.into(),
                    generation,
                    "health_watchdog",
                )
                .await;
            }
            cleaned += 1;
            continue;
        }

        let capture_advancing = stall_liveness::stall_watchdog_capture_offset_advancing(
            provider,
            channel_id,
            &snapshot,
            now_unix_secs,
        );
        let should_clean = stall_watchdog_should_force_clean(
            snapshot.attached,
            snapshot.desynced,
            capture_advancing,
            snapshot.inflight_terminal_delivery_committed,
            snapshot.inflight_started_at.as_deref(),
            now_unix_secs,
            STALL_WATCHDOG_THRESHOLD_SECS,
            registry.started_at_unix(),
        );
        let judgment_basis = stall_liveness::StallWatchdogJudgmentBasis::from_snapshot(
            &snapshot,
            now_unix_secs,
            registry.started_at_unix(),
        );
        let mut force_clean_inflight = None;
        let mut liveness_decision = None;
        if should_clean {
            force_clean_inflight =
                discord::inflight::load_inflight_state(provider, channel_id.get());
            let decision = stall_liveness::evaluate_stall_watchdog_liveness(
                provider,
                channel_id,
                &snapshot,
                force_clean_inflight.as_ref(),
                now_unix_secs,
                stall_liveness::STALL_WATCHDOG_POSITIVE_LIVENESS_SECS,
                stall_liveness::STALL_WATCHDOG_MAX_LIVENESS_DEFERRALS,
                // #3671: backstop measures the turn's RAW age (restart-invariant),
                // NOT the boot-floored age the threshold gate above uses.
                judgment_basis.turn_age_secs,
            );
            if decision.should_defer() {
                stall_liveness::log_stall_watchdog_liveness_deferred(
                    provider,
                    channel_id,
                    &snapshot,
                    &judgment_basis,
                    &decision,
                    stall_liveness::STALL_WATCHDOG_POSITIVE_LIVENESS_SECS,
                    STALL_WATCHDOG_THRESHOLD_SECS,
                );
                continue;
            }
            liveness_decision = Some(decision);
        } else {
            stall_liveness::clear_stall_watchdog_liveness_state_if_healthy(
                provider, channel_id, &snapshot,
            );
        }
        if !should_clean {
            // Detection-only sibling probe for "completed-stale" inflight
            // leaks: bridge handed off cleanup to the watcher (or the watcher
            // delivered the response itself), but the inflight file persisted
            // past the staleness threshold even though the relay is healthy
            // and the mailbox has no active turn. This is the silent gap
            // behind the deadlock-manager 30/45/60-min alarm pattern. We do
            // NOT clean here — the live tmux session may be waiting for the
            // user's next message. Emitting the structured event lets the
            // external monitor and counters detect each occurrence so the
            // root cause can be isolated.
            if inflight_completed_stale_leak_detected(
                snapshot.attached,
                snapshot.desynced,
                snapshot.inflight_state_present,
                snapshot.mailbox_active_user_msg_id,
                snapshot.inflight_updated_at.as_deref(),
                snapshot.tmux_session_alive,
                now_unix_secs,
                STALL_WATCHDOG_THRESHOLD_SECS,
            ) {
                let leak_inflight =
                    discord::inflight::load_inflight_state(provider, channel_id.get());
                let leak_turn_id = leak_inflight
                    .as_ref()
                    .filter(|s| s.user_msg_id != 0)
                    .map(|s| format!("discord:{}:{}", s.channel_id, s.user_msg_id));
                let leak_dispatch_id = leak_inflight.as_ref().and_then(|s| s.dispatch_id.clone());
                let leak_session_key = leak_inflight.as_ref().and_then(|s| s.session_key.clone());
                // Only a genuinely unrelayed answer is a leak. A stale-but-
                // delivered turn — e.g. one whose answer went out as a bridge
                // fallback message, which advances response_sent_offset to len —
                // must NOT raise the leaked-answer alarm or trigger recovery.
                let has_unrelayed_answer = leak_inflight.as_ref().is_some_and(|s| {
                    leak_recovery_unrelayed_range(&s.full_response, s.response_sent_offset)
                        .is_some()
                });
                if !has_unrelayed_answer {
                    // #3629: a completed-stale row with no unrelayed answer is
                    // either a delivered-and-idle turn (committed → preserve) or
                    // a never-committed NO_REPLY/empty orphan (clean). We only
                    // reach here when the mailbox has NO active turn and the
                    // relay is healthy. Only a real, NON-ZERO user_msg_id is
                    // cleaned — zero-id rows are never auto-cleaned because they
                    // can be a live recovery/TUI-direct turn or a newer pinned
                    // zero-id turn (codex #3629 review). See
                    // `completed_stale_no_answer_orphan_should_clean`.
                    let terminal_committed = leak_inflight
                        .as_ref()
                        .is_some_and(|s| s.terminal_delivery_committed);
                    let this_turn_user_msg_id =
                        leak_inflight.as_ref().map(|s| s.user_msg_id).unwrap_or(0);
                    if completed_stale_no_answer_orphan_should_clean(
                        terminal_committed,
                        this_turn_user_msg_id,
                    ) {
                        // Identity-guarded removal: a newer turn that has since
                        // written this channel's row yields UserMsgMismatch and
                        // is preserved; planned-restart / rebind-origin rows are
                        // skipped. We only ever delete THIS leaked turn's row.
                        let clear_outcome = discord::inflight::clear_inflight_state_if_matches(
                            provider,
                            channel_id.get(),
                            this_turn_user_msg_id,
                        );
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::warn!(
                            "  [{ts}] 🧹 #3629 cleaned NO_REPLY/empty orphan inflight on channel {} (provider={}, outcome={:?})",
                            channel_id,
                            provider.as_str(),
                            clear_outcome
                        );
                        crate::services::observability::emit_inflight_lifecycle_event(
                            provider.as_str(),
                            channel_id.get(),
                            leak_dispatch_id.as_deref(),
                            leak_session_key.as_deref(),
                            leak_turn_id.as_deref(),
                            "leak_cleaned_noreply_orphan",
                            serde_json::json!({
                                "guarded_clear_outcome": format!("{clear_outcome:?}"),
                                "inflight_started_at": snapshot.inflight_started_at,
                                "inflight_updated_at": snapshot.inflight_updated_at,
                                "tmux_session_alive": snapshot.tmux_session_alive,
                            }),
                        );
                    }
                    continue;
                }
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] 🔎 inflight leak suspected on channel {} (provider={}): completed-stale + healthy watcher; emitting telemetry only",
                    channel_id,
                    provider.as_str()
                );
                crate::services::observability::emit_inflight_lifecycle_event(
                    provider.as_str(),
                    channel_id.get(),
                    leak_dispatch_id.as_deref(),
                    leak_session_key.as_deref(),
                    leak_turn_id.as_deref(),
                    "leak_detected_completed_stale",
                    serde_json::json!({
                        "inflight_started_at": snapshot.inflight_started_at,
                        "inflight_updated_at": snapshot.inflight_updated_at,
                        "tmux_session": snapshot.tmux_session,
                        "tmux_session_alive": snapshot.tmux_session_alive,
                        "watcher_attached": snapshot.attached,
                        "has_pending_queue": snapshot.has_pending_queue,
                        "full_response_len": leak_inflight
                            .as_ref()
                            .map(|s| s.full_response.len()),
                        "response_sent_offset": leak_inflight
                            .as_ref()
                            .map(|s| s.response_sent_offset),
                        "last_watcher_relayed_offset": leak_inflight
                            .as_ref()
                            .and_then(|s| s.last_watcher_relayed_offset),
                        "watcher_owns_live_relay": leak_inflight
                            .as_ref()
                            .map(|s| s.watcher_owns_live_relay),
                    }),
                );

                // #2860 delivery-lease consolidation: upgrade detection ->
                // recovery. Scoped to the watcher-delegated-but-never-relayed
                // signature and gated on a live-message probe, so it can never
                // double-deliver (see maybe_recover_completed_stale_leak). The
                // recovery re-loads its own fresh inflight state.
                maybe_recover_completed_stale_leak(registry, provider, &shared, channel_id).await;
            }
            continue;
        }
        stall_liveness::log_stall_watchdog_force_cleanup_judgment(
            provider,
            channel_id,
            &snapshot,
            &judgment_basis,
            liveness_decision.as_ref(),
            stall_liveness::STALL_WATCHDOG_POSITIVE_LIVENESS_SECS,
            STALL_WATCHDOG_THRESHOLD_SECS,
        );
        // #4460: DO NOT force-terminate the active turn here. The old branch-4
        // "desynced force-clean" deleted the inflight state, released the
        // mailbox cancel token, cancelled the watcher and cleared the turn
        // view — which force-killed a *live* codex session on a false positive
        // (watcher desync / post-restart re-sync lag while the producer was
        // still working). Per operator directive we now MENTION the owner and
        // leave the turn completely untouched.
        //
        // Detection tightening: reuse the exact shadow verdict that
        // `log_stall_watchdog_force_cleanup_judgment` just recorded. If it
        // classifies the producer as live, this is a false positive — skip
        // silently (no page, no cleanup, turn untouched).
        let (shadow_verdict, _shadow_reasons) = super::stall_verdict::judgment_log_fields(
            &snapshot,
            liveness_decision.as_ref(),
            stall_liveness::STALL_WATCHDOG_POSITIVE_LIVENESS_SECS,
            judgment_basis.restart_grace_active,
        );
        if shadow_verdict == super::stall_verdict::StallVerdict::ProducerLive.as_str() {
            // #4460: producer is live — the desync detection fired a false
            // positive. Never terminate, never page.
            continue;
        }
        // #4460: not classified live — surface a rate-limited mention so the
        // owner can decide, but NEVER terminate/cancel/delete anything. The
        // turn (inflight state, mailbox token, watcher, turn view) is left
        // fully intact.
        notify_suspected_stall_without_cleanup(
            &shared,
            provider,
            channel_id,
            &snapshot,
            force_clean_inflight.as_ref(),
        )
        .await;
        continue;
    }
    // #3410 cross-tick retry: channels whose force-clean respawn failed dropped
    // out of the watcher-derived candidate loop (no watcher = not a candidate),
    // so re-attempt each still-tracked absent channel — never give up after one.
    watcher_respawn::retry_pending_watcher_respawns(registry, provider, &runtimes, now_unix_secs)
        .await;
    cleaned + relay_auto_heal::run_orphan_token_auto_heal_pass(registry, provider, &runtimes).await
}

/// Spawn the long-lived background task that runs the stall watchdog at
/// `STALL_WATCHDOG_INTERVAL_SECS` cadence for the given provider. Should
/// be called once per provider during dcserver bootstrap, alongside
/// `placeholder_sweeper::spawn_placeholder_sweeper`.
pub fn spawn_stall_watchdog(registry: Arc<HealthRegistry>, provider: ProviderKind) {
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(
            STALL_WATCHDOG_INITIAL_DELAY_SECS,
        ))
        .await;
        loop {
            let cleaned = run_stall_watchdog_pass(&registry, &provider).await;
            if cleaned > 0 {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] ⚡ stall-watchdog ({}): cleaned={}",
                    provider.as_str(),
                    cleaned
                );
            }
            tokio::time::sleep(std::time::Duration::from_secs(STALL_WATCHDOG_INTERVAL_SECS)).await;
        }
    });
}

/// #2860 — recover a completed-stale inflight leak by delivering the generated
/// answer that never reached the user. Returns `true` only when an answer was
/// actually delivered.
///
/// SAFETY (no double-delivery) — two independent guards, both required:
///   1. Scope to the watcher-delegated-but-never-relayed signature
///      (`effective_relay_owner_kind() == Watcher` and `last_watcher_relayed_offset
///      == None`). The only delivery path that strands an answer in a *separate*
///      message — the bridge's local `SentFallbackAfterEditFailure` fallback-post
///      — lives exclusively in the bridge-owns-delivery branch
///      (`bridge_output_owner == None`), which never sets `RelayOwnerKind::Watcher`.
///      Requiring `Watcher` therefore excludes the fallback class entirely; for the
///      delegated class the bridge skipped its own delivery and the watcher (per the
///      null offset) never relayed, so the answer is provably nowhere.
///   2. A live-message probe of `current_msg_id`: recovery either starts from a
///      still-placeholder message, or derives an exact already-delivered chunk
///      prefix from the original message + same-bot continuation messages. A
///      non-placeholder body that does not match chunk 0 fails closed.
/// Recovery edits `current_msg_id` in place for chunk 0 and sends only missing
/// continuation chunks. Repeated watchdog passes resume after the confirmed
/// prefix, so partial success is retryable without duplicate chunk sends.
async fn maybe_recover_completed_stale_leak(
    registry: &HealthRegistry,
    provider: &ProviderKind,
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
) -> bool {
    // Operate on a freshly re-loaded row, not the detection-time snapshot, so any
    // relay that advanced state between detection and now is respected.
    let Some(state) = discord::inflight::load_inflight_state(provider, channel_id.get()) else {
        return false;
    };

    // Planned restart / rebind flows re-deliver the answer themselves.
    if state.restart_mode.is_some() || state.rebind_origin {
        return false;
    }
    // A silent turn intentionally suppresses assistant-text relay (mirrors the
    // bridge's `silent_turn` terminal-delivery suppression); never resurface it.
    if state.silent_turn {
        return false;
    }
    // Scope guard (1): only the watcher-delegated, never-relayed class — the exact
    // signature of the observed leaks. Excludes the bridge-local fallback-post
    // class (owner == None) that could strand the answer in a separate message.
    if state.effective_relay_owner_kind() != discord::inflight::RelayOwnerKind::Watcher
        || state.last_watcher_relayed_offset.is_some()
    {
        return false;
    }
    // We recover by editing the existing placeholder in place (bridge parity).
    // With no addressable placeholder there is nothing to safely edit.
    if state.current_msg_id == 0 {
        return false;
    }
    let Some((start, end)) =
        leak_recovery_unrelayed_range(&state.full_response, state.response_sent_offset)
    else {
        return false;
    };
    // NB: the bridge appends a turn-time `review_dispatch_warning` before
    // formatting; that guard is turn-lifecycle-only, so stale-leak recovery
    // intentionally delivers just the answer body.
    let Some(delivery_text) = render_leak_recovery_delivery(
        &state.full_response,
        start,
        shared.ui.status_panel_v2_enabled,
        provider,
    ) else {
        return false;
    };
    // `split_message` is the authoritative limit (its effective cap is below
    // Discord's 2000). For multi-chunk recovery we first consult the durable
    // per-chunk ledger. Legacy/pre-ledger retries can still derive a prefix
    // from live Discord state, then seed the ledger before continuing.
    let chunks = discord::formatting::split_message(&delivery_text);
    if chunks.is_empty() {
        return false;
    }
    let ledger_identity = LeakRecoveryLedgerIdentity::new(provider, &state, start, end, &chunks);
    let mut confirmed_chunks =
        leak_recovery_confirmed_prefix_from_ledger(&ledger_identity).unwrap_or(0);

    let Some(http) = owning_runtime_http_for_channel(registry, provider, channel_id).await else {
        return false;
    };

    let current_msg_id = MessageId::new(state.current_msg_id);
    let current_message = if confirmed_chunks == 0 {
        let current_bot_user_id = match http.get_current_user().await {
            Ok(user) => user.id.get(),
            Err(error) => {
                tracing::warn!(
                    "[leak-recover] failed to resolve current bot id for channel {}: {error}",
                    channel_id
                );
                return false;
            }
        };
        let current_message = match http.get_message(channel_id, current_msg_id).await {
            Ok(message) => message,
            Err(error) => {
                tracing::warn!(
                    "[leak-recover] failed to fetch placeholder message {} in channel {}: {error}",
                    current_msg_id,
                    channel_id
                );
                return false;
            }
        };
        if current_message.author.id.get() != current_bot_user_id {
            tracing::warn!(
                "[leak-recover] refusing recovery for channel {} msg {}: message author is not current bot",
                channel_id,
                current_msg_id
            );
            return false;
        }

        let continuation_messages = if chunks.len() > 1
            && leak_recovery_message_matches_chunk(&current_message.content, &chunks[0])
        {
            let Some(messages) = leak_recovery_fetch_continuation_contents(
                &http,
                channel_id,
                current_msg_id,
                current_bot_user_id,
            )
            .await
            else {
                return false;
            };
            messages
        } else {
            Vec::new()
        };

        confirmed_chunks = leak_recovery_confirmed_chunk_count(
            &current_message.content,
            continuation_messages
                .iter()
                .map(|(_, content)| content.as_str()),
            &chunks,
        )
        .unwrap_or(0);
        if confirmed_chunks > 0 {
            if let Err(error) =
                leak_recovery_record_confirmed_chunk(&ledger_identity, 0, state.current_msg_id)
            {
                tracing::warn!(
                    "[leak-recover] failed to persist confirmed chunk 1/{} for channel {}: {error}",
                    chunks.len(),
                    channel_id
                );
                return false;
            }
            let mut chunk_index = 1usize;
            for (message_id, content) in &continuation_messages {
                if chunk_index >= confirmed_chunks {
                    break;
                }
                if !leak_recovery_message_matches_chunk(content, &chunks[chunk_index]) {
                    continue;
                }
                if let Err(error) =
                    leak_recovery_record_confirmed_chunk(&ledger_identity, chunk_index, *message_id)
                {
                    tracing::warn!(
                        "[leak-recover] failed to persist confirmed chunk {}/{} for channel {}: {error}",
                        chunk_index + 1,
                        chunks.len(),
                        channel_id
                    );
                    return false;
                }
                chunk_index += 1;
            }
        }
        Some(current_message)
    } else {
        None
    };

    let mut wrote_any_chunk = false;
    if confirmed_chunks == 0 {
        let Some(current_message) = current_message.as_ref() else {
            tracing::warn!(
                "[leak-recover] missing live placeholder probe for channel {} despite empty ledger",
                channel_id
            );
            return false;
        };
        if !discord::placeholder_sweeper::is_message_still_placeholder(&current_message.content) {
            let turn_id = (state.user_msg_id != 0)
                .then(|| format!("discord:{}:{}", state.channel_id, state.user_msg_id));
            crate::services::observability::emit_inflight_lifecycle_event(
                provider.as_str(),
                channel_id.get(),
                state.dispatch_id.as_deref(),
                state.session_key.as_deref(),
                turn_id.as_deref(),
                "leak_recovery_skipped_already_delivered",
                serde_json::json!({
                    "current_msg_id": state.current_msg_id,
                    "byte_len": delivery_text.len(),
                    "chunks": chunks.len(),
                }),
            );
            return false;
        }

        // Edit the original placeholder to chunk 0. If Discord commits the edit
        // but the client observes an error/crash, the next pass derives
        // `confirmed_chunks == 1` from the live message and continues with chunk
        // 1. No fallback send is used for the first chunk.
        if discord::http::edit_channel_message(
            http.as_ref(),
            channel_id,
            current_msg_id,
            &chunks[0],
        )
        .await
        .is_err()
        {
            return false;
        }
        if let Err(error) =
            leak_recovery_record_confirmed_chunk(&ledger_identity, 0, state.current_msg_id)
        {
            tracing::warn!(
                "[leak-recover] edited chunk 1/{} on channel {} but failed to persist chunk ledger: {error}",
                chunks.len(),
                channel_id
            );
            return true;
        }
        wrote_any_chunk = true;
        confirmed_chunks = 1;
    }

    for (chunk_index, chunk) in chunks.iter().enumerate().skip(confirmed_chunks) {
        match discord::http::send_channel_message(http.as_ref(), channel_id, chunk).await {
            Ok(message) => {
                if let Err(error) = leak_recovery_record_confirmed_chunk(
                    &ledger_identity,
                    chunk_index,
                    message.id.get(),
                ) {
                    tracing::warn!(
                        "[leak-recover] sent continuation chunk {}/{} on channel {} but failed to persist chunk ledger: {error}",
                        chunk_index + 1,
                        chunks.len(),
                        channel_id
                    );
                    return true;
                }
                wrote_any_chunk = true;
                confirmed_chunks = chunk_index + 1;
                tracing::debug!(
                    "[leak-recover] sent continuation chunk {}/{} on channel {}",
                    chunk_index + 1,
                    chunks.len(),
                    channel_id
                );
            }
            Err(error) => {
                tracing::warn!(
                    "[leak-recover] continuation chunk {}/{} failed on channel {}: {error}",
                    chunk_index + 1,
                    chunks.len(),
                    channel_id
                );
                let turn_id = (state.user_msg_id != 0)
                    .then(|| format!("discord:{}:{}", state.channel_id, state.user_msg_id));
                crate::services::observability::emit_inflight_lifecycle_event(
                    provider.as_str(),
                    channel_id.get(),
                    state.dispatch_id.as_deref(),
                    state.session_key.as_deref(),
                    turn_id.as_deref(),
                    "leak_recovery_partial_retryable",
                    serde_json::json!({
                        "failed_chunk_index": chunk_index,
                        "confirmed_chunks_before_attempt": confirmed_chunks,
                        "chunks": chunks.len(),
                    }),
                );
                return wrote_any_chunk;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }

    // If a prior attempt already delivered every chunk but crashed before
    // persisting the offset, this pass reaches here with `wrote_any_chunk=false`;
    // persist the terminal offset and emit only a confirmation event.
    if confirmed_chunks >= chunks.len() && !wrote_any_chunk {
        let turn_id = (state.user_msg_id != 0)
            .then(|| format!("discord:{}:{}", state.channel_id, state.user_msg_id));
        crate::services::observability::emit_inflight_lifecycle_event(
            provider.as_str(),
            channel_id.get(),
            state.dispatch_id.as_deref(),
            state.session_key.as_deref(),
            turn_id.as_deref(),
            "leak_recovery_confirmed_already_flushed",
            serde_json::json!({
                "chunks": chunks.len(),
                "byte_start": start,
                "byte_end": end,
            }),
        );
    }

    if confirmed_chunks < chunks.len() && !wrote_any_chunk {
        return false;
    }
    let (delivery_detail, op) = if !wrote_any_chunk {
        ("confirmed", "probe")
    } else if chunks.len() == 1 {
        ("edited", "edit")
    } else {
        ("edited+continued", "edit+send")
    };

    // Advance the offset and persist so a later pass (even after a dcserver
    // restart) treats this tail as delivered and never re-sends it. Re-load and
    // re-check identity first so we never clobber a concurrently-updated row, and
    // skip if another path already advanced past `end`.
    let offset_save_outcome =
        discord::inflight::persist_leak_recovery_response_offset_if_matches_identity_locked(
            provider,
            channel_id.get(),
            &discord::inflight::InflightTurnIdentity::from_state(&state),
            state.current_msg_id,
            end,
        );
    if matches!(
        offset_save_outcome,
        discord::inflight::GuardedSaveOutcome::IoError
    ) {
        tracing::warn!(
            "[leak-recover] delivered answer on channel {} but failed to persist offset",
            channel_id
        );
    }
    if let Err(error) = leak_recovery_clear_chunk_ledger(&ledger_identity) {
        tracing::warn!(
            "[leak-recover] recovered answer on channel {} but failed to clear chunk ledger: {error}",
            channel_id
        );
    }

    let turn_id = (state.user_msg_id != 0)
        .then(|| format!("discord:{}:{}", state.channel_id, state.user_msg_id));
    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::warn!(
        "  [{ts}] 📤 leak recovered ({delivery_detail}): delivered {}-byte answer on channel {} (provider={})",
        end - start,
        channel_id,
        provider.as_str()
    );
    crate::services::observability::emit_inflight_lifecycle_event(
        provider.as_str(),
        channel_id.get(),
        state.dispatch_id.as_deref(),
        state.session_key.as_deref(),
        turn_id.as_deref(),
        "leak_recovered_flushed",
        serde_json::json!({
            "byte_start": start,
            "byte_end": end,
            "flushed_len": end - start,
            "delivery": delivery_detail,
        }),
    );
    crate::services::observability::emit_relay_delivery(
        provider.as_str(),
        channel_id.get(),
        state.dispatch_id.as_deref(),
        state.session_key.as_deref(),
        turn_id.as_deref(),
        Some(state.current_msg_id),
        "recovery",
        op,
        Some(start as u64),
        Some(end as u64),
        true,
        Some(delivery_detail),
    );

    // #3925: finalize the turn the SAME way normal completion does. The relay
    // broke mid-turn, so this turn's own finalizer never ran — both the inflight
    // row AND the in-memory mailbox active-turn token were left "in progress".
    // We have now delivered the completed answer out-of-band (reaching here only
    // on FULL delivery — every partial/retryable path returned earlier), so the
    // turn is genuinely done. Previously this path only advanced
    // `response_sent_offset` and returned, leaving the session pinned to a phantom
    // in-progress turn: the intake gate (`mailbox_has_live_active_turn_or_cleanup_stale_proof`)
    // kept queueing every new message with "앞선 턴 진행 중" and no completion event
    // ever fired to dequeue them → permanent dead session (#3925).
    //
    // Mirror the startup recovery branches (`recovery_engine.rs`
    // completed_during_downtime / output_completed): route the recovered terminal
    // through the single-authority finalizer (`finish_recovered_turn_mailbox` →
    // `mailbox_finish_turn` token release + gated `global_active` decrement +
    // idle-queue kickoff), then clear the inflight row under its identity guard.
    super::super::recovery_engine::finish_recovered_turn_mailbox(
        shared,
        provider,
        channel_id,
        "leak_recovery_oob_completion",
    )
    .await;
    // Identity-guarded clear so a turn that started in the narrow window after the
    // mailbox token released (the kickoff above drains the queue on a spawned task)
    // is never clobbered — the helper re-reads the fresh on-disk row under its
    // flock before deleting (#3860 RMW discipline).
    let inflight_clear_outcome = clear_recovered_leak_inflight(provider, channel_id, &state);
    crate::services::observability::emit_inflight_lifecycle_event(
        provider.as_str(),
        channel_id.get(),
        state.dispatch_id.as_deref(),
        state.session_key.as_deref(),
        turn_id.as_deref(),
        "leak_recovery_finalized_inflight",
        serde_json::json!({
            "guarded_clear_outcome": format!("{inflight_clear_outcome:?}"),
            "user_msg_id": state.user_msg_id,
            "byte_end": end,
        }),
    );
    true
}

/// #3925: finalize the inflight turn-state for a turn whose completed answer was
/// just delivered out-of-band by [`maybe_recover_completed_stale_leak`]. Mirrors
/// the identity-guarded clear the normal turn-completion path uses
/// (`turn_finalizer::do_finalize` step A / the startup recovery branches): a real
/// `user_msg_id` clears via `clear_inflight_state_if_matches` (a newer turn's row
/// yields `UserMsgMismatch` and is preserved); a zero-id-owned recovery /
/// TUI-direct turn clears its own zero-id row via the zero-owned guard.
fn clear_recovered_leak_inflight(
    provider: &ProviderKind,
    channel_id: ChannelId,
    state: &discord::inflight::InflightTurnState,
) -> discord::inflight::GuardedClearOutcome {
    let Some(root) = discord::inflight::inflight_runtime_root() else {
        return discord::inflight::GuardedClearOutcome::Missing;
    };
    clear_recovered_leak_inflight_in_root(&root, provider, channel_id, state)
}

/// Root-explicit variant of [`clear_recovered_leak_inflight`] for hermetic unit
/// tests (mirrors inflight.rs's own `_in_root` test convention).
fn clear_recovered_leak_inflight_in_root(
    root: &std::path::Path,
    provider: &ProviderKind,
    channel_id: ChannelId,
    state: &discord::inflight::InflightTurnState,
) -> discord::inflight::GuardedClearOutcome {
    if state.user_msg_id != 0 {
        discord::inflight::clear_inflight_state_if_matches_in_root(
            root,
            provider,
            channel_id.get(),
            state.user_msg_id,
        )
    } else {
        discord::inflight::clear_inflight_state_if_matches_zero_owned_in_root(
            root,
            provider,
            channel_id.get(),
        )
    }
}

/// #3925 — hermetic tests pinning that the OOB deadlock-manager leak recovery
/// finalizes the inflight turn-state after delivering the completed answer, so an
/// idle session stops queueing new messages forever. Uses TempDir + the `_in_root`
/// clear path (no env / SharedData), mirroring inflight.rs's own test convention.
#[cfg(test)]
mod leak_recovery_inflight_finalize_tests {
    use super::clear_recovered_leak_inflight_in_root;
    use crate::services::discord::inflight::{GuardedClearOutcome, InflightTurnState};
    use crate::services::provider::ProviderKind;
    use poise::serenity_prelude::ChannelId;

    fn leak_inflight_state(
        provider: &ProviderKind,
        channel_id: u64,
        user_msg_id: u64,
    ) -> InflightTurnState {
        serde_json::from_value(serde_json::json!({
            "version": 9,
            "provider": provider.as_str(),
            "channel_id": channel_id,
            "channel_name": "adk-cc",
            "request_owner_user_id": 7,
            "user_msg_id": user_msg_id,
            "current_msg_id": user_msg_id + 1,
            "current_msg_len": 0,
            "user_text": "prompt",
            "source": "text",
            "session_id": "session",
            "tmux_session_name": "AgentDesk-claude-adk-cc",
            "output_path": "/tmp/claude-transcript.jsonl",
            "input_fifo_path": null,
            "last_offset": 0,
            "full_response": "recovered answer body",
            "response_sent_offset": 0,
            "relay_owner_kind": "watcher",
            "started_at": "2026-01-01 00:00:00",
            "updated_at": "2026-01-01 00:00:00"
        }))
        .expect("leak inflight state")
    }

    fn seed_leak_inflight(
        root: &std::path::Path,
        provider: &ProviderKind,
        channel_id: u64,
        user_msg_id: u64,
    ) -> InflightTurnState {
        let state = leak_inflight_state(provider, channel_id, user_msg_id);
        let provider_dir = root.join(provider.as_str());
        std::fs::create_dir_all(&provider_dir).expect("create provider dir");
        let path = provider_dir.join(format!("{channel_id}.json"));
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&state).expect("serialize"),
        )
        .expect("seed inflight row");
        state
    }

    fn row_path(
        root: &std::path::Path,
        provider: &ProviderKind,
        channel_id: u64,
    ) -> std::path::PathBuf {
        root.join(provider.as_str())
            .join(format!("{channel_id}.json"))
    }

    /// The fix: after the OOB recovery delivers the completed answer, the inflight
    /// row that pinned the phantom in-progress turn is cleared, so the intake gate
    /// stops queueing new messages and the deferred kickoff can dequeue.
    #[test]
    fn oob_leak_recovery_clears_inflight_so_queue_can_drain() {
        let temp = tempfile::tempdir().expect("temp root");
        let provider = ProviderKind::Claude;
        let channel_id = 4242u64;
        let state = seed_leak_inflight(temp.path(), &provider, channel_id, 9001);
        assert!(
            row_path(temp.path(), &provider, channel_id).exists(),
            "precondition: the inflight row exists and would gate intake-queueing"
        );

        let outcome = clear_recovered_leak_inflight_in_root(
            temp.path(),
            &provider,
            ChannelId::new(channel_id),
            &state,
        );

        assert_eq!(
            outcome,
            GuardedClearOutcome::Cleared,
            "OOB recovery must finalize (clear) the inflight turn-state (#3925)"
        );
        assert!(
            !row_path(temp.path(), &provider, channel_id).exists(),
            "the phantom in-progress inflight row must be removed so the session returns to idle"
        );
    }

    /// #3860: the guarded clear must NOT clobber a NEWER turn that wrote this
    /// channel's row in the window after the mailbox token released.
    #[test]
    fn oob_leak_recovery_preserves_a_newer_turns_inflight() {
        let temp = tempfile::tempdir().expect("temp root");
        let provider = ProviderKind::Claude;
        let channel_id = 4242u64;
        // On disk: a NEWER turn's row.
        seed_leak_inflight(temp.path(), &provider, channel_id, 12345);
        // The recovered (older) turn carries a different user_msg_id.
        let recovered = leak_inflight_state(&provider, channel_id, 9001);

        let outcome = clear_recovered_leak_inflight_in_root(
            temp.path(),
            &provider,
            ChannelId::new(channel_id),
            &recovered,
        );

        assert_eq!(outcome, GuardedClearOutcome::UserMsgMismatch);
        assert!(
            row_path(temp.path(), &provider, channel_id).exists(),
            "a newer turn's inflight row must be preserved (#3860)"
        );
    }

    /// A zero-id-owned recovery / TUI-direct turn (its on-disk `user_msg_id` is 0)
    /// must still finalize its own row via the zero-owned guard.
    #[test]
    fn oob_leak_recovery_clears_zero_id_owned_inflight() {
        let temp = tempfile::tempdir().expect("temp root");
        let provider = ProviderKind::Claude;
        let channel_id = 7007u64;
        let state = seed_leak_inflight(temp.path(), &provider, channel_id, 0);

        let outcome = clear_recovered_leak_inflight_in_root(
            temp.path(),
            &provider,
            ChannelId::new(channel_id),
            &state,
        );

        assert_eq!(outcome, GuardedClearOutcome::Cleared);
        assert!(!row_path(temp.path(), &provider, channel_id).exists());
    }
}

/// #1446 — pure-helper tests for the stall-watchdog decision logic.
/// Always-on (`#[cfg(test)]`) because the helper has no filesystem/runtime
/// dependencies; keeping them in a removed SQLite-only gate would prevent them
/// from running in normal `cargo test --bin agentdesk` invocations.

#[cfg(test)]
mod stall_watchdog_pure_tests {
    use super::super::stall_liveness::stall_watchdog_jsonl_liveness_defers_force_clean;
    use super::leak_recovery_ledger::{
        LeakRecoveryLedgerIdentity, leak_recovery_chunk_fingerprints,
        leak_recovery_clear_chunk_ledger, leak_recovery_confirmed_chunk_count,
        leak_recovery_confirmed_prefix_from_ledger, leak_recovery_record_confirmed_chunk,
        leak_recovery_unrelayed_range, render_leak_recovery_delivery,
    };
    use super::watchdog_decisions::{
        STALL_WATCHDOG_THRESHOLD_SECS, completed_stale_no_answer_orphan_should_clean,
        force_clean_should_preserve_resume_selector, inflight_completed_stale_leak_detected,
        stale_idle_foreground_queue_detected, stall_watchdog_should_force_clean,
        stall_watchdog_should_force_clean_orphan_explicit_background_work,
    };
    use super::{
        capture_idle_tmux_stale_turn_inflight_pin, clear_idle_tmux_stale_turn_inflight_if_pinned,
        preserve_cancel_should_skip_provider_interrupt_for_idle_tui,
        revalidate_and_clear_explicit_background_inflight,
    };
    use crate::config::TestEnvVarGuard;
    use crate::services::discord::inflight::{
        self, GuardedClearOutcome, GuardedSaveOutcome, InflightTurnIdentity, InflightTurnState,
        RelayOwnerKind,
    };
    use crate::services::discord::relay_health::{RelayActiveTurn, RelayStallState};
    use crate::services::discord::{InflightRestartMode, TmuxCleanupPolicy};
    use crate::services::provider::ProviderKind;
    use chrono::TimeZone;
    use poise::serenity_prelude::ChannelId;

    fn test_ledger_identity(chunks: &[String]) -> LeakRecoveryLedgerIdentity {
        LeakRecoveryLedgerIdentity {
            provider: ProviderKind::Codex.as_str().to_string(),
            channel_id: 42,
            current_msg_id: 9001,
            user_msg_id: 7001,
            byte_start: 0,
            byte_end: 12345,
            chunk_fingerprints: leak_recovery_chunk_fingerprints(chunks),
        }
    }

    #[test]
    fn leak_range_whole_response_when_nothing_relayed() {
        // The exact 8-leak signature: response_sent_offset=0, full answer present.
        assert_eq!(
            leak_recovery_unrelayed_range("hello world", 0),
            Some((0, 11))
        );
    }

    #[test]
    fn leak_range_none_when_already_fully_delivered() {
        // Idempotent re-run after a prior recovery advanced the offset to len.
        assert_eq!(leak_recovery_unrelayed_range("hello world", 11), None);
        assert_eq!(leak_recovery_unrelayed_range("hello world", 99), None);
    }

    #[test]
    fn leak_range_empty_response_is_none() {
        assert_eq!(leak_recovery_unrelayed_range("", 0), None);
    }

    #[test]
    fn leak_range_partial_tail_only() {
        assert_eq!(
            leak_recovery_unrelayed_range("hello world", 6),
            Some((6, 11))
        );
    }

    #[test]
    fn leak_range_snaps_back_to_char_boundary() {
        // "안녕" is 6 bytes (3 each). An offset landing mid-codepoint must walk
        // back to a boundary so the returned slice never panics.
        let s = "안녕하세요";
        let (start, end) = leak_recovery_unrelayed_range(s, 4).unwrap();
        assert!(s.is_char_boundary(start));
        assert_eq!(start, 3); // walked back from 4 to the boundary after "안"
        assert_eq!(end, s.len());
        // Slicing at the returned start must be valid.
        let _ = &s[start..end];
    }

    #[test]
    fn render_skips_blank_tail() {
        // A blank/whitespace tail formats to empty -> no delivery, never a
        // placeholder post or empty notice.
        let provider = ProviderKind::Claude;
        assert_eq!(render_leak_recovery_delivery("", 0, false, &provider), None);
        assert_eq!(
            render_leak_recovery_delivery("   \n  ", 0, false, &provider),
            None
        );
    }

    #[test]
    fn render_returns_formatted_real_answer() {
        let provider = ProviderKind::Claude;
        let out = render_leak_recovery_delivery("실제 답변입니다", 0, false, &provider)
            .expect("real answer should render");
        assert!(out.contains("실제 답변입니다"));
    }

    #[test]
    fn large_leak_chunk_fingerprints_are_stable_and_ordered() {
        let chunks = vec!["first".to_string(), "second".to_string()];
        let first = leak_recovery_chunk_fingerprints(&chunks);
        let second = leak_recovery_chunk_fingerprints(&chunks);

        assert_eq!(first, second);
        assert_eq!(first.len(), 2);
        assert!(first[0].starts_with("0:"));
        assert!(first[1].starts_with("1:"));
        assert_ne!(first[0], first[1]);
    }

    #[test]
    fn multi_chunk_progress_is_unknown_before_first_chunk_edit() {
        let chunks = vec!["chunk-0".to_string(), "chunk-1".to_string()];

        assert_eq!(
            leak_recovery_confirmed_chunk_count("⠋ Processing...", std::iter::empty(), &chunks),
            None
        );
    }

    #[test]
    fn multi_chunk_progress_derives_confirmed_prefix_from_live_messages() {
        let chunks = vec![
            "chunk-0".to_string(),
            "chunk-1".to_string(),
            "chunk-2".to_string(),
        ];
        let continuation_contents = ["unrelated bot notice", "chunk-1", "chunk-2"];

        assert_eq!(
            leak_recovery_confirmed_chunk_count(
                "chunk-0",
                continuation_contents.into_iter(),
                &chunks,
            ),
            Some(3)
        );
    }

    #[test]
    fn multi_chunk_progress_accepts_legacy_continuation_context_prefixes() {
        let chunks = vec![
            "chunk-0".to_string(),
            "chunk-1".to_string(),
            "chunk-2".to_string(),
        ];
        let continuation_contents = ["[2]\nchunk-1", "[+]\nchunk-2"];

        assert_eq!(
            leak_recovery_confirmed_chunk_count(
                "[1/3]\nchunk-0",
                continuation_contents.into_iter(),
                &chunks,
            ),
            Some(3)
        );
    }

    #[test]
    fn multi_chunk_progress_rejects_non_prefix_marker_mentions() {
        let chunks = vec!["chunk-0".to_string(), "chunk-1".to_string()];

        assert_eq!(
            leak_recovery_confirmed_chunk_count("note [1/2]\nchunk-0", std::iter::empty(), &chunks,),
            None
        );
    }

    #[test]
    fn multi_chunk_progress_stops_at_missing_tail_for_retry() {
        let chunks = vec![
            "chunk-0".to_string(),
            "chunk-1".to_string(),
            "chunk-2".to_string(),
        ];
        let continuation_contents = ["chunk-1"];

        assert_eq!(
            leak_recovery_confirmed_chunk_count(
                "chunk-0",
                continuation_contents.into_iter(),
                &chunks,
            ),
            Some(2)
        );
    }

    #[test]
    fn chunk_ledger_survives_restart_and_resumes_tail_without_live_fetch() {
        let _guard = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let tempdir = tempfile::tempdir().expect("temp runtime root");
        let _env = TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            tempdir.path(),
        );
        let chunks: Vec<String> = (0..64).map(|index| format!("chunk-{index}")).collect();
        let identity = test_ledger_identity(&chunks);

        leak_recovery_clear_chunk_ledger(&identity).expect("clear ledger");
        for index in 0..37 {
            leak_recovery_record_confirmed_chunk(&identity, index, 10_000 + index as u64)
                .expect("record chunk");
        }

        assert_eq!(
            leak_recovery_confirmed_prefix_from_ledger(&identity),
            Some(37),
            "fresh process can resume from persisted prefix without scanning Discord messages"
        );

        leak_recovery_clear_chunk_ledger(&identity).expect("clear ledger");
        assert_eq!(leak_recovery_confirmed_prefix_from_ledger(&identity), None);
    }

    #[test]
    fn chunk_ledger_records_send_failure_boundary_without_offset_loss() {
        let _guard = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let tempdir = tempfile::tempdir().expect("temp runtime root");
        let _env = TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            tempdir.path(),
        );
        let chunks: Vec<String> = (0..5).map(|index| format!("chunk-{index}")).collect();
        let identity = test_ledger_identity(&chunks);

        leak_recovery_clear_chunk_ledger(&identity).expect("clear ledger");
        leak_recovery_record_confirmed_chunk(&identity, 0, identity.current_msg_id)
            .expect("record edited first chunk");
        leak_recovery_record_confirmed_chunk(&identity, 1, 10_001)
            .expect("record sent continuation");
        // Simulate Discord send failure on chunk 2: no record is written. A retry
        // must skip exactly chunks 0..1 and resume at the missing tail.
        assert_eq!(
            leak_recovery_confirmed_prefix_from_ledger(&identity),
            Some(2)
        );
    }

    #[test]
    fn chunk_ledger_ignores_stale_identity_or_changed_confirmed_chunk() {
        let _guard = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let tempdir = tempfile::tempdir().expect("temp runtime root");
        let _env = TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            tempdir.path(),
        );
        let chunks = vec!["chunk-0".to_string(), "chunk-1".to_string()];
        let identity = test_ledger_identity(&chunks);

        leak_recovery_clear_chunk_ledger(&identity).expect("clear ledger");
        leak_recovery_record_confirmed_chunk(&identity, 0, identity.current_msg_id)
            .expect("record chunk");
        leak_recovery_record_confirmed_chunk(&identity, 1, 10_001).expect("record tail chunk");

        // #3031(A): a CONFIRMED chunk being rewritten must trim the prefix at the
        // divergence so we never claim a stale delivery.
        let changed_chunks = vec!["chunk-0".to_string(), "changed-tail".to_string()];
        let changed_identity = LeakRecoveryLedgerIdentity {
            chunk_fingerprints: leak_recovery_chunk_fingerprints(&changed_chunks),
            byte_end: identity.byte_end + 4,
            ..identity.clone()
        };
        assert_eq!(
            leak_recovery_confirmed_prefix_from_ledger(&changed_identity),
            Some(1),
            "a rewritten confirmed tail chunk trims the prefix exactly at its index"
        );

        let other_turn = LeakRecoveryLedgerIdentity {
            user_msg_id: identity.user_msg_id + 1,
            ..identity.clone()
        };
        assert_eq!(
            leak_recovery_confirmed_prefix_from_ledger(&other_turn),
            None,
            "another turn using the same channel/message id must not inherit confirmations"
        );
    }

    #[test]
    fn chunk_ledger_growing_response_never_lowers_confirmed_prefix() {
        // #3031(A) regression: a late post-terminal continuation grows
        // `full_response`, moving `byte_end` and appending chunk fingerprints. The
        // already-confirmed prefix (whose fingerprints are byte-identical) must
        // remain confirmed so recovery never re-sends already-delivered chunks.
        let _guard = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let tempdir = tempfile::tempdir().expect("temp runtime root");
        let _env = TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            tempdir.path(),
        );

        let chunks = vec!["chunk-0".to_string(), "chunk-1".to_string()];
        let identity = test_ledger_identity(&chunks);
        leak_recovery_clear_chunk_ledger(&identity).expect("clear ledger");
        leak_recovery_record_confirmed_chunk(&identity, 0, identity.current_msg_id)
            .expect("record chunk 0");
        leak_recovery_record_confirmed_chunk(&identity, 1, 10_001).expect("record chunk 1");
        assert_eq!(
            leak_recovery_confirmed_prefix_from_ledger(&identity),
            Some(2)
        );

        // Response grows: the same two chunks plus two appended tail chunks. byte_end
        // and the fingerprint array both change, but the confirmed prefix is intact.
        let grown_chunks = vec![
            "chunk-0".to_string(),
            "chunk-1".to_string(),
            "chunk-2".to_string(),
            "chunk-3".to_string(),
        ];
        let grown_identity = LeakRecoveryLedgerIdentity {
            chunk_fingerprints: leak_recovery_chunk_fingerprints(&grown_chunks),
            byte_end: identity.byte_end + 16,
            ..identity.clone()
        };
        assert_eq!(
            leak_recovery_confirmed_prefix_from_ledger(&grown_identity),
            Some(2),
            "a longer full_response must NOT lower the confirmed prefix"
        );

        // Recording a further chunk against the grown identity must preserve, not
        // drop, the carried-forward prefix.
        leak_recovery_record_confirmed_chunk(&grown_identity, 2, 10_002)
            .expect("record appended chunk 2");
        assert_eq!(
            leak_recovery_confirmed_prefix_from_ledger(&grown_identity),
            Some(3),
            "appended-chunk confirmation builds on the preserved prefix"
        );
    }

    #[test]
    fn leak_recovery_offset_patch_preserves_concurrent_relay_watermark_update() {
        let _guard = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let tempdir = tempfile::tempdir().expect("temp runtime root");
        let _env = TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            tempdir.path(),
        );

        let provider = ProviderKind::Codex;
        let channel_id = ChannelId::new(4_111_001);
        let mut snapshot = InflightTurnState::new(
            provider.clone(),
            channel_id.get(),
            Some("adk-cc".to_string()),
            7,
            4_111_101,
            4_111_201,
            "recover leaked answer".to_string(),
            Some("session-4111-leak".to_string()),
            Some("AgentDesk-codex-4111-leak".to_string()),
            Some("/tmp/agentdesk-4111-leak.jsonl".to_string()),
            None,
            10,
        );
        snapshot.full_response = "already relayed plus recovered tail".to_string();
        snapshot.response_sent_offset = 7;
        inflight::save_inflight_state(&snapshot).expect("seed leak snapshot row");
        let identity = InflightTurnIdentity::from_state(&snapshot);
        let delivered_offset = snapshot.full_response.len();

        let mut concurrent = inflight::load_inflight_state(&provider, channel_id.get())
            .expect("seeded row for concurrent update");
        concurrent.last_watcher_relayed_offset = Some(2_048);
        concurrent.last_watcher_relayed_generation_mtime_ns = Some(9_999);
        concurrent.set_relay_owner_kind(RelayOwnerKind::SessionBoundRelay);
        inflight::save_inflight_state(&concurrent).expect("save concurrent watermark update");

        let outcome = inflight::persist_leak_recovery_response_offset_if_matches_identity_locked(
            &provider,
            channel_id.get(),
            &identity,
            snapshot.current_msg_id,
            delivered_offset,
        );

        assert_eq!(outcome, GuardedSaveOutcome::Saved);
        let persisted = inflight::load_inflight_state(&provider, channel_id.get())
            .expect("patched row must survive");
        assert_eq!(persisted.response_sent_offset, delivered_offset);
        assert_eq!(persisted.last_watcher_relayed_offset, Some(2_048));
        assert_eq!(
            persisted.last_watcher_relayed_generation_mtime_ns,
            Some(9_999)
        );
        assert_eq!(
            persisted.effective_relay_owner_kind(),
            RelayOwnerKind::SessionBoundRelay
        );
    }

    fn local_string(unix: i64) -> String {
        chrono::Local
            .timestamp_opt(unix, 0)
            .single()
            .expect("valid local time")
            .format("%Y-%m-%d %H:%M:%S")
            .to_string()
    }

    #[test]
    fn explicit_background_watchdog_abort_preserves_new_identity_after_snapshot() {
        let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let tempdir = tempfile::tempdir().expect("tempdir");
        let _env = TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            tempdir.path(),
        );

        let provider = ProviderKind::Claude;
        let channel_id = ChannelId::new(4_019_230_001);
        let mut old = InflightTurnState::new(
            provider.clone(),
            channel_id.get(),
            None,
            1,
            4_019_230_101,
            4_019_230_101,
            "old explicit background".to_string(),
            Some("old-session".to_string()),
            Some("AgentDesk-claude-r2-watchdog".to_string()),
            Some("/tmp/r2-watchdog-old.jsonl".to_string()),
            None,
            10,
        );
        old.turn_start_offset = Some(10);
        inflight::save_inflight_state(&old).expect("save old row");
        let old_identity = InflightTurnIdentity::from_state(&old);
        let old_finalizer_turn_id = old.effective_finalizer_turn_id();

        let mut newer_same_message = old.clone();
        newer_same_message.started_at = "2099-01-01 00:00:00".to_string();
        newer_same_message.turn_start_offset = Some(99);
        newer_same_message.session_id = Some("new-session".to_string());
        inflight::save_inflight_state(&newer_same_message).expect("replace row");

        let outcome = revalidate_and_clear_explicit_background_inflight(
            &provider,
            channel_id,
            Some(&old_identity),
            Some(old_finalizer_turn_id),
        );

        assert_eq!(outcome.clear_outcome, GuardedClearOutcome::UserMsgMismatch);
        let persisted = inflight::load_inflight_state(&provider, channel_id.get())
            .expect("newer row must survive mismatch");
        assert_eq!(persisted.started_at, "2099-01-01 00:00:00");
        assert_eq!(persisted.turn_start_offset, Some(99));
    }

    #[test]
    fn idle_tmux_stale_turn_clear_preserves_fresh_inflight_after_finish_window() {
        let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let tempdir = tempfile::tempdir().expect("tempdir");
        let _env = TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            tempdir.path(),
        );

        let provider = ProviderKind::Claude;
        let channel_id = ChannelId::new(4_030_601);
        let tmux_session = "AgentDesk-claude-stale-mailbox-toctou";
        let mut stale = InflightTurnState::new(
            provider.clone(),
            channel_id.get(),
            None,
            1,
            4_030_601_101,
            4_030_601_102,
            "stale idle turn".to_string(),
            Some("stale-session".to_string()),
            Some(tmux_session.to_string()),
            Some("/tmp/agentdesk-stale-mailbox-toctou-t1.jsonl".to_string()),
            None,
            10,
        );
        stale.turn_start_offset = Some(10);
        inflight::save_inflight_state(&stale).expect("save stale row");

        let pin = capture_idle_tmux_stale_turn_inflight_pin(&stale).expect("readiness-time pin");
        assert_eq!(pin.identity.user_msg_id, 4_030_601_101);

        let mut fresh = InflightTurnState::new(
            provider.clone(),
            channel_id.get(),
            None,
            1,
            4_030_601_201,
            4_030_601_202,
            "fresh follow-up turn".to_string(),
            Some("fresh-session".to_string()),
            Some(tmux_session.to_string()),
            Some("/tmp/agentdesk-stale-mailbox-toctou-t2.jsonl".to_string()),
            None,
            20,
        );
        fresh.turn_start_offset = Some(20);
        inflight::save_inflight_state(&fresh).expect("save fresh row in finish-to-clear window");

        assert_eq!(
            clear_idle_tmux_stale_turn_inflight_if_pinned(&provider, channel_id.get(), Some(&pin),),
            GuardedClearOutcome::UserMsgMismatch
        );
        let persisted = inflight::load_inflight_state(&provider, channel_id.get())
            .expect("fresh row must survive guarded stale clear");
        assert_eq!(persisted.user_msg_id, 4_030_601_201);
        assert_eq!(persisted.session_id.as_deref(), Some("fresh-session"));
        assert!(
            persisted.save_generation > pin.save_generation,
            "fresh row write must advance save_generation past the stale pin"
        );
    }

    /// #3629: a completed-stale, no-unrelayed-answer row is cleaned ONLY when it
    /// never committed a terminal delivery (NO_REPLY/empty orphan) AND it has a
    /// real, non-zero user_msg_id. A committed delivered-and-idle row (#3126
    /// wakeup-waiting) and any zero-id row (live recovery / newer pinned zero-id
    /// turn, codex #3629 review) must be preserved.
    #[test]
    fn completed_stale_no_answer_orphan_cleans_only_uncommitted_nonzero() {
        let real_id = 9_100_084_013_837_195_159u64;
        // NO_REPLY / empty terminal turn with a real id: orphan → clean.
        assert!(completed_stale_no_answer_orphan_should_clean(
            false, real_id
        ));
        // Delivered-and-idle turn (#3126): committed → preserve.
        assert!(!completed_stale_no_answer_orphan_should_clean(
            true, real_id
        ));
        // Zero-id rows are NEVER auto-cleaned, regardless of committed state.
        assert!(!completed_stale_no_answer_orphan_should_clean(false, 0));
        assert!(!completed_stale_no_answer_orphan_should_clean(true, 0));
    }

    /// `inflight_completed_stale_leak_detected` requires every signal of the
    /// "completed-stale on healthy watcher" pattern. Each AND-clause is
    /// inverted to lock the gate against accidental relaxation.
    #[test]
    fn inflight_completed_stale_leak_detected_requires_all_signals() {
        let now_unix = chrono::Utc::now().timestamp();
        let stale = local_string(now_unix - (STALL_WATCHDOG_THRESHOLD_SECS as i64) - 1);
        let fresh = local_string(now_unix - 5);

        // Happy path: attached + synced + inflight + idle mailbox + alive
        // tmux + stale updated_at → leak.
        assert!(inflight_completed_stale_leak_detected(
            true,
            false,
            true,
            None,
            Some(stale.as_str()),
            Some(true),
            now_unix,
            STALL_WATCHDOG_THRESHOLD_SECS,
        ));

        // Detached watcher → not this pattern (covered by other recovery).
        assert!(!inflight_completed_stale_leak_detected(
            false,
            false,
            true,
            None,
            Some(stale.as_str()),
            Some(true),
            now_unix,
            STALL_WATCHDOG_THRESHOLD_SECS,
        ));

        // Desynced relay → handled by stall_watchdog_should_force_clean
        // already; do not double-emit here.
        assert!(!inflight_completed_stale_leak_detected(
            true,
            true,
            true,
            None,
            Some(stale.as_str()),
            Some(true),
            now_unix,
            STALL_WATCHDOG_THRESHOLD_SECS,
        ));

        // Mailbox still has an active turn anchor → live work, not a leak.
        assert!(!inflight_completed_stale_leak_detected(
            true,
            false,
            true,
            Some(123),
            Some(stale.as_str()),
            Some(true),
            now_unix,
            STALL_WATCHDOG_THRESHOLD_SECS,
        ));

        // Tmux session gone → orphan path, not the completed-stale pattern.
        assert!(!inflight_completed_stale_leak_detected(
            true,
            false,
            true,
            None,
            Some(stale.as_str()),
            Some(false),
            now_unix,
            STALL_WATCHDOG_THRESHOLD_SECS,
        ));

        // No inflight on disk → nothing to leak.
        assert!(!inflight_completed_stale_leak_detected(
            true,
            false,
            false,
            None,
            Some(stale.as_str()),
            Some(true),
            now_unix,
            STALL_WATCHDOG_THRESHOLD_SECS,
        ));

        // Fresh updated_at → user may still be reading; do not flag yet.
        assert!(!inflight_completed_stale_leak_detected(
            true,
            false,
            true,
            None,
            Some(fresh.as_str()),
            Some(true),
            now_unix,
            STALL_WATCHDOG_THRESHOLD_SECS,
        ));

        // Unparseable updated_at → never infer a leak from missing data.
        assert!(!inflight_completed_stale_leak_detected(
            true,
            false,
            true,
            None,
            Some("not-a-real-timestamp"),
            Some(true),
            now_unix,
            STALL_WATCHDOG_THRESHOLD_SECS,
        ));
    }

    #[test]
    fn stale_idle_foreground_queue_detected_requires_no_progress_stale_inflight() {
        let now_unix = chrono::Utc::now().timestamp();
        let stale = local_string(now_unix - (STALL_WATCHDOG_THRESHOLD_SECS as i64) - 1);
        let fresh = local_string(now_unix - 5);

        assert!(stale_idle_foreground_queue_detected(
            RelayActiveTurn::Foreground,
            true,
            1,
            true,
            Some(stale.as_str()),
            Some(true),
            None,
            now_unix,
            STALL_WATCHDOG_THRESHOLD_SECS,
        ));
        assert!(stale_idle_foreground_queue_detected(
            RelayActiveTurn::Foreground,
            true,
            0,
            true,
            Some(stale.as_str()),
            Some(true),
            None,
            now_unix,
            STALL_WATCHDOG_THRESHOLD_SECS,
        ));
        assert!(stale_idle_foreground_queue_detected(
            RelayActiveTurn::Foreground,
            true,
            1,
            true,
            Some(stale.as_str()),
            Some(true),
            Some((now_unix - STALL_WATCHDOG_THRESHOLD_SECS as i64 - 1) * 1000),
            now_unix,
            STALL_WATCHDOG_THRESHOLD_SECS,
        ));

        for (
            name,
            active_turn,
            has_token,
            queue_depth,
            inflight,
            updated_at,
            tmux_alive,
            outbound,
        ) in [
            (
                "not foreground",
                RelayActiveTurn::ExplicitBackground,
                true,
                1,
                true,
                Some(stale.as_str()),
                Some(true),
                None,
            ),
            (
                "no mailbox token",
                RelayActiveTurn::Foreground,
                false,
                1,
                true,
                Some(stale.as_str()),
                Some(true),
                None,
            ),
            (
                "no inflight",
                RelayActiveTurn::Foreground,
                true,
                1,
                false,
                Some(stale.as_str()),
                Some(true),
                None,
            ),
            (
                "fresh inflight",
                RelayActiveTurn::Foreground,
                true,
                1,
                true,
                Some(fresh.as_str()),
                Some(true),
                None,
            ),
            (
                "tmux not live",
                RelayActiveTurn::Foreground,
                true,
                1,
                true,
                Some(stale.as_str()),
                Some(false),
                None,
            ),
            (
                "outbound progress exists",
                RelayActiveTurn::Foreground,
                true,
                1,
                true,
                Some(stale.as_str()),
                Some(true),
                Some((now_unix - 60) * 1000),
            ),
        ] {
            assert!(
                !stale_idle_foreground_queue_detected(
                    active_turn,
                    has_token,
                    queue_depth,
                    inflight,
                    updated_at,
                    tmux_alive,
                    outbound,
                    now_unix,
                    STALL_WATCHDOG_THRESHOLD_SECS,
                ),
                "{name}"
            );
        }
    }

    #[test]
    fn orphan_explicit_background_force_clean_requires_ownership_and_staleness() {
        let now_unix = chrono::Utc::now().timestamp();
        let stale = local_string(now_unix - (STALL_WATCHDOG_THRESHOLD_SECS as i64) - 1);
        let fresh = local_string(now_unix - 5);
        let stale_outbound = Some((now_unix - STALL_WATCHDOG_THRESHOLD_SECS as i64 - 1) * 1000);
        let fresh_outbound = Some((now_unix - 5) * 1000);

        assert!(
            stall_watchdog_should_force_clean_orphan_explicit_background_work(
                RelayStallState::ExplicitBackgroundWork,
                true,
                Some(42),
                42,
                false,
                true,
                Some(stale.as_str()),
                Some(true),
                Some(0),
                stale_outbound,
                now_unix,
                STALL_WATCHDOG_THRESHOLD_SECS,
            )
        );

        for (
            name,
            state,
            attached,
            owner,
            desynced,
            inflight,
            updated_at,
            tmux_alive,
            unread,
            outbound,
        ) in [
            (
                "not explicit background",
                RelayStallState::ActiveForegroundStream,
                true,
                Some(42),
                false,
                true,
                Some(stale.as_str()),
                Some(true),
                Some(0),
                stale_outbound,
            ),
            (
                "detached",
                RelayStallState::ExplicitBackgroundWork,
                false,
                Some(42),
                false,
                true,
                Some(stale.as_str()),
                Some(true),
                Some(0),
                stale_outbound,
            ),
            (
                "cross owner",
                RelayStallState::ExplicitBackgroundWork,
                true,
                Some(7),
                false,
                true,
                Some(stale.as_str()),
                Some(true),
                Some(0),
                stale_outbound,
            ),
            (
                "desynced path owns cleanup",
                RelayStallState::ExplicitBackgroundWork,
                true,
                Some(42),
                true,
                true,
                Some(stale.as_str()),
                Some(true),
                Some(0),
                stale_outbound,
            ),
            (
                "missing inflight",
                RelayStallState::ExplicitBackgroundWork,
                true,
                Some(42),
                false,
                false,
                Some(stale.as_str()),
                Some(true),
                Some(0),
                stale_outbound,
            ),
            (
                "fresh inflight",
                RelayStallState::ExplicitBackgroundWork,
                true,
                Some(42),
                false,
                true,
                Some(fresh.as_str()),
                Some(true),
                Some(0),
                stale_outbound,
            ),
            (
                "tmux not alive",
                RelayStallState::ExplicitBackgroundWork,
                true,
                Some(42),
                false,
                true,
                Some(stale.as_str()),
                Some(false),
                Some(0),
                stale_outbound,
            ),
            (
                "unread capture bytes",
                RelayStallState::ExplicitBackgroundWork,
                true,
                Some(42),
                false,
                true,
                Some(stale.as_str()),
                Some(true),
                Some(1),
                stale_outbound,
            ),
            (
                "fresh outbound",
                RelayStallState::ExplicitBackgroundWork,
                true,
                Some(42),
                false,
                true,
                Some(stale.as_str()),
                Some(true),
                Some(0),
                fresh_outbound,
            ),
        ] {
            assert!(
                !stall_watchdog_should_force_clean_orphan_explicit_background_work(
                    state,
                    attached,
                    owner,
                    42,
                    desynced,
                    inflight,
                    updated_at,
                    tmux_alive,
                    unread,
                    outbound,
                    now_unix,
                    STALL_WATCHDOG_THRESHOLD_SECS,
                ),
                "{name}"
            );
        }
    }

    #[test]
    fn preserve_cancel_skips_interrupt_only_for_idle_safe_claude_tui() {
        assert!(preserve_cancel_should_skip_provider_interrupt_for_idle_tui(
            &ProviderKind::Claude,
            TmuxCleanupPolicy::PreserveSession,
            true,
            true,
        ));
        assert!(preserve_cancel_should_skip_provider_interrupt_for_idle_tui(
            &ProviderKind::Claude,
            TmuxCleanupPolicy::PreserveSessionAndInflight {
                restart_mode: InflightRestartMode::HotSwapHandoff,
            },
            true,
            true,
        ));

        assert!(
            !preserve_cancel_should_skip_provider_interrupt_for_idle_tui(
                &ProviderKind::Codex,
                TmuxCleanupPolicy::PreserveSession,
                true,
                true,
            )
        );
        assert!(
            !preserve_cancel_should_skip_provider_interrupt_for_idle_tui(
                &ProviderKind::Claude,
                TmuxCleanupPolicy::PreserveSession,
                false,
                true,
            )
        );
        assert!(
            !preserve_cancel_should_skip_provider_interrupt_for_idle_tui(
                &ProviderKind::Claude,
                TmuxCleanupPolicy::PreserveSession,
                true,
                false,
            )
        );
        assert!(
            !preserve_cancel_should_skip_provider_interrupt_for_idle_tui(
                &ProviderKind::Claude,
                TmuxCleanupPolicy::CleanupSession {
                    termination_reason_code: Some("force"),
                },
                true,
                true,
            )
        );
    }

    /// All three signals (`attached`, `desynced`, stale `updated_at`) must
    /// be present before the watchdog cleans. A regression that drops any
    /// one of the AND-conditions is caught by these inversions.
    #[test]
    fn stall_watchdog_should_force_clean_requires_all_signals() {
        let now_unix = chrono::Utc::now().timestamp();
        let stale_unix = now_unix - (STALL_WATCHDOG_THRESHOLD_SECS as i64) - 1;
        let to_local = |unix: i64| {
            chrono::Local
                .timestamp_opt(unix, 0)
                .single()
                .expect("valid local time")
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        };
        let stale_str = to_local(stale_unix);
        let fresh_str = to_local(now_unix - 5);
        // Boot far in the past so `max(started_at, boot)` resolves to
        // `started_at` — these cases assert the pre-#3041 semantics.
        let boot_unix = stale_unix - 100;

        // Happy path: attached + desynced + stale + not-committed → clean.
        assert!(stall_watchdog_should_force_clean(
            true,
            true,
            false,
            false,
            Some(stale_str.as_str()),
            now_unix,
            STALL_WATCHDOG_THRESHOLD_SECS,
            boot_unix,
        ));

        // detached → no clean.
        assert!(!stall_watchdog_should_force_clean(
            false,
            true,
            false,
            false,
            Some(stale_str.as_str()),
            now_unix,
            STALL_WATCHDOG_THRESHOLD_SECS,
            boot_unix,
        ));

        // synced → no clean.
        assert!(!stall_watchdog_should_force_clean(
            true,
            false,
            false,
            false,
            Some(stale_str.as_str()),
            now_unix,
            STALL_WATCHDOG_THRESHOLD_SECS,
            boot_unix,
        ));

        // fresh started_at → no clean (live-turn safety net).
        assert!(!stall_watchdog_should_force_clean(
            true,
            true,
            false,
            false,
            Some(fresh_str.as_str()),
            now_unix,
            STALL_WATCHDOG_THRESHOLD_SECS,
            boot_unix,
        ));

        // missing started_at → no clean.
        assert!(!stall_watchdog_should_force_clean(
            true,
            true,
            false,
            false,
            None,
            now_unix,
            STALL_WATCHDOG_THRESHOLD_SECS,
            boot_unix,
        ));

        // unparseable started_at → no clean.
        assert!(!stall_watchdog_should_force_clean(
            true,
            true,
            false,
            false,
            Some("not-a-real-timestamp"),
            now_unix,
            STALL_WATCHDOG_THRESHOLD_SECS,
            boot_unix,
        ));

        // #3041 post-restart grace: identical stale+desynced+uncommitted
        // signature, but the process booted recently → the staleness clock
        // restarts at boot, so the row is NOT yet old enough to clean. This is
        // the watcher that simply hasn't re-synced since the restart.
        assert!(
            !stall_watchdog_should_force_clean(
                true,
                true,
                false,
                false,
                Some(stale_str.as_str()),
                now_unix,
                STALL_WATCHDOG_THRESHOLD_SECS,
                /* boot_unix_secs */ now_unix - 5,
            ),
            "a pre-restart-stale row must get a full staleness window after boot before force-clean"
        );

        // …and once that post-boot window has fully elapsed, a still-desynced
        // (genuinely hung) row IS cleaned even with the grace anchor.
        assert!(stall_watchdog_should_force_clean(
            true,
            true,
            false,
            false,
            Some(stale_str.as_str()),
            now_unix,
            STALL_WATCHDOG_THRESHOLD_SECS,
            /* boot_unix_secs */ now_unix - (STALL_WATCHDOG_THRESHOLD_SECS as i64) - 1,
        ));
    }

    #[test]
    fn stall_watchdog_capture_advancing_blocks_force_clean() {
        let now_unix = chrono::Utc::now().timestamp();
        let stale_unix = now_unix - (STALL_WATCHDOG_THRESHOLD_SECS as i64) - 1;
        let stale_str = chrono::Local
            .timestamp_opt(stale_unix, 0)
            .single()
            .expect("valid local time")
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();

        assert!(
            !stall_watchdog_should_force_clean(
                true,
                true,
                /* capture_advancing */ true,
                /* inflight_terminal_delivery_committed */ false,
                Some(stale_str.as_str()),
                now_unix,
                STALL_WATCHDOG_THRESHOLD_SECS,
                stale_unix - 100,
            ),
            "#4178: advancing capture offset means the tmux turn is alive, so the watchdog must not force-clean"
        );
    }

    #[test]
    fn stall_watchdog_capture_stopped_preserves_1446_force_clean() {
        let now_unix = chrono::Utc::now().timestamp();
        let stale_unix = now_unix - (STALL_WATCHDOG_THRESHOLD_SECS as i64) - 1;
        let stale_str = chrono::Local
            .timestamp_opt(stale_unix, 0)
            .single()
            .expect("valid local time")
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();

        assert!(stall_watchdog_should_force_clean(
            true,
            true,
            /* capture_advancing */ false,
            /* inflight_terminal_delivery_committed */ false,
            Some(stale_str.as_str()),
            now_unix,
            STALL_WATCHDOG_THRESHOLD_SECS,
            stale_unix - 100,
        ));
    }

    /// #3656: consecutive short turns under the same session key must not
    /// inherit an old channel-level update anchor. The force-clean age is based
    /// on the current turn's `started_at`, so a freshly-started turn is not
    /// killed just because a prior turn in the same session was old.
    #[test]
    fn stall_watchdog_age_uses_current_turn_started_at() {
        let now_unix = chrono::Utc::now().timestamp();
        let fresh_started_unix = now_unix - 5;
        let fresh_started = chrono::Local
            .timestamp_opt(fresh_started_unix, 0)
            .single()
            .expect("valid local time")
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();

        assert!(
            !stall_watchdog_should_force_clean(
                true,
                true,
                false,
                false,
                Some(fresh_started.as_str()),
                now_unix,
                STALL_WATCHDOG_THRESHOLD_SECS,
                now_unix - 10_000,
            ),
            "a fresh current-turn started_at must reset the watchdog age even for the same session"
        );
    }

    /// #3126 false-positive guard: a normally-completed turn that is now idle
    /// (wakeup / loop wind-down) carries `terminal_delivery_committed == true`.
    /// Even when it reads as attached + desynced + stale — exactly the
    /// otherwise-clean signature — the watchdog must NOT force-clean it,
    /// because killing a healthy wakeup-waiting session is the regression in
    /// issue #3126.
    #[test]
    fn stall_watchdog_skips_completed_idle_wakeup_turn() {
        let now_unix = chrono::Utc::now().timestamp();
        let stale_unix = now_unix - (STALL_WATCHDOG_THRESHOLD_SECS as i64) - 1;
        let stale_str = chrono::Local
            .timestamp_opt(stale_unix, 0)
            .single()
            .expect("valid local time")
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();

        // Boot far in the past so the grace anchor is inert for this test.
        let boot_unix = stale_unix - 100;

        // Same attached+desynced+stale signature as the happy path, but the
        // turn already committed its terminal response → completed-then-idle.
        assert!(
            !stall_watchdog_should_force_clean(
                true,
                true,
                /* capture_advancing */ false,
                /* inflight_terminal_delivery_committed */ true,
                Some(stale_str.as_str()),
                now_unix,
                STALL_WATCHDOG_THRESHOLD_SECS,
                boot_unix,
            ),
            "completed-then-idle (wakeup-waiting) session must not be force-cleaned"
        );

        // Control: the identical signature with an uncommitted (still hung)
        // turn IS force-cleaned, proving the guard is the only difference.
        assert!(stall_watchdog_should_force_clean(
            true,
            true,
            /* capture_advancing */ false,
            /* inflight_terminal_delivery_committed */ false,
            Some(stale_str.as_str()),
            now_unix,
            STALL_WATCHDOG_THRESHOLD_SECS,
            boot_unix,
        ));
    }

    /// #3169 Death #1: the jsonl-mtime liveness probe must DEFER the force-clean
    /// when the session wrote provider events inside the freshness window — even
    /// though the channel reads as desynced (the #3126 commit guard is defeated
    /// for loop turns with user_msg_id==0). A loop session that is mid-write is
    /// healthy, not hung.
    #[test]
    fn stall_watchdog_liveness_defers_force_clean_when_jsonl_fresh() {
        let now_unix = chrono::Utc::now().timestamp();
        let now_nanos = now_unix.saturating_mul(1_000_000_000);

        // jsonl written 30s ago — well inside the freshness window → defer.
        let fresh_nanos = now_nanos - 30i64.saturating_mul(1_000_000_000);
        assert!(
            stall_watchdog_jsonl_liveness_defers_force_clean(
                fresh_nanos,
                now_unix,
                STALL_WATCHDOG_THRESHOLD_SECS,
            ),
            "a session whose jsonl was just written is mid-write, not hung — force-clean must be deferred"
        );

        // A write at "now" (or future clock skew) is unambiguously fresh.
        assert!(stall_watchdog_jsonl_liveness_defers_force_clean(
            now_nanos,
            now_unix,
            STALL_WATCHDOG_THRESHOLD_SECS,
        ));
        assert!(stall_watchdog_jsonl_liveness_defers_force_clean(
            now_nanos + 5i64.saturating_mul(1_000_000_000),
            now_unix,
            STALL_WATCHDOG_THRESHOLD_SECS,
        ));
    }

    /// #3169 Death #1: the liveness probe must NOT suppress the force-clean for a
    /// genuine hang — a hung turn writes no provider events, so its jsonl mtime
    /// is stale past the window and the desynced force-clean still fires. This is
    /// the "no blanket suppression" guarantee.
    #[test]
    fn stall_watchdog_liveness_keeps_force_clean_when_jsonl_stale() {
        let now_unix = chrono::Utc::now().timestamp();
        let now_nanos = now_unix.saturating_mul(1_000_000_000);

        // jsonl stale by 2x the window → no liveness vouch → allow force-clean.
        let stale_nanos =
            now_nanos - (2 * STALL_WATCHDOG_THRESHOLD_SECS as i64).saturating_mul(1_000_000_000);
        assert!(
            !stall_watchdog_jsonl_liveness_defers_force_clean(
                stale_nanos,
                now_unix,
                STALL_WATCHDOG_THRESHOLD_SECS,
            ),
            "a hung turn with a stale jsonl must still be force-cleaned — no blanket suppression"
        );

        // No observable jsonl/.generation activity (probe == 0) → never defer.
        assert!(!stall_watchdog_jsonl_liveness_defers_force_clean(
            0,
            now_unix,
            STALL_WATCHDOG_THRESHOLD_SECS,
        ));

        // Boundary: exactly at the threshold is treated as stale (not < window).
        let boundary_nanos =
            now_nanos - (STALL_WATCHDOG_THRESHOLD_SECS as i64).saturating_mul(1_000_000_000);
        assert!(!stall_watchdog_jsonl_liveness_defers_force_clean(
            boundary_nanos,
            now_unix,
            STALL_WATCHDOG_THRESHOLD_SECS,
        ));
    }

    /// #3041 B: resume-selector preserve/discard classifier.
    #[test]
    fn force_clean_resume_selector_preserve_vs_discard() {
        // Unknown selector → never preserve.
        assert!(!force_clean_should_preserve_resume_selector(
            None,
            Some("host:sess"),
            true,
            Some(true),
        ));
        assert!(!force_clean_should_preserve_resume_selector(
            Some("sid"),
            None,
            true,
            Some(true),
        ));
        // Blank selector strings count as unknown.
        assert!(!force_clean_should_preserve_resume_selector(
            Some("   "),
            Some("host:sess"),
            true,
            Some(true),
        ));

        // Committed turn → preserve even if the pane already exited.
        assert!(force_clean_should_preserve_resume_selector(
            Some("sid"),
            Some("host:sess"),
            /* terminal_delivery_committed */ true,
            /* tmux_session_alive */ Some(false),
        ));

        // Live pane, not yet committed (interrupted but healthy) → preserve.
        assert!(force_clean_should_preserve_resume_selector(
            Some("sid"),
            Some("host:sess"),
            false,
            Some(true),
        ));

        // Genuine hang signature: pane dead AND never committed → discard.
        assert!(!force_clean_should_preserve_resume_selector(
            Some("sid"),
            Some("host:sess"),
            false,
            Some(false),
        ));
        // Pane liveness unknown AND never committed → discard (no evidence).
        assert!(!force_clean_should_preserve_resume_selector(
            Some("sid"),
            Some("host:sess"),
            false,
            None,
        ));
    }
}

#[cfg(test)]
mod stall_watchdog_auto_heal_tests {
    use super::super::HealthRegistry;
    use crate::config::TestEnvVarGuard;
    use crate::services::provider::{CancelToken, ProviderKind};
    use poise::serenity_prelude::{ChannelId, MessageId, UserId};
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64};

    struct IdleTmuxCandidateHookGuard {
        previous: Option<super::IdleTmuxStaleTurnInflightCandidateHook>,
    }

    impl Drop for IdleTmuxCandidateHookGuard {
        fn drop(&mut self) {
            *super::idle_tmux_stale_turn_inflight_candidate_hook()
                .lock()
                .unwrap_or_else(|poison| poison.into_inner()) = self.previous.take();
        }
    }

    struct IdleTmuxPostClearHookGuard {
        previous: Option<super::IdleTmuxStaleTurnPostClearHook>,
    }

    impl Drop for IdleTmuxPostClearHookGuard {
        fn drop(&mut self) {
            *super::idle_tmux_stale_turn_post_clear_hook()
                .lock()
                .unwrap_or_else(|poison| poison.into_inner()) = self.previous.take();
        }
    }

    fn set_idle_tmux_post_clear_hook(
        hook: super::IdleTmuxStaleTurnPostClearHook,
    ) -> IdleTmuxPostClearHookGuard {
        let mut slot = super::idle_tmux_stale_turn_post_clear_hook()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        IdleTmuxPostClearHookGuard {
            previous: slot.replace(hook),
        }
    }

    fn set_idle_tmux_candidate_hook(
        hook: super::IdleTmuxStaleTurnInflightCandidateHook,
    ) -> IdleTmuxCandidateHookGuard {
        let mut slot = super::idle_tmux_stale_turn_inflight_candidate_hook()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        IdleTmuxCandidateHookGuard {
            previous: slot.replace(hook),
        }
    }

    fn result_line(text: &str) -> String {
        format!("{{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"{text}\"}}\n")
    }

    fn watcher_handle(
        tmux_session_name: &str,
        output_path: &std::path::Path,
        cancel: Arc<AtomicBool>,
    ) -> crate::services::discord::TmuxWatcherHandle {
        crate::services::discord::TmuxWatcherHandle {
            tmux_session_name: tmux_session_name.to_string(),
            output_path: output_path.to_string_lossy().to_string(),
            paused: Arc::new(AtomicBool::new(false)),
            resume_offset: Arc::new(std::sync::Mutex::new(None)),
            cancel,
            pause_epoch: Arc::new(AtomicU64::new(0)),
            turn_delivered: Arc::new(AtomicBool::new(false)),
            last_heartbeat_ts_ms: Arc::new(AtomicI64::new(0)),
        }
    }

    fn seed_idle_inflight(
        provider: &ProviderKind,
        channel: ChannelId,
        user_msg_id: u64,
        tmux: &str,
        output_path: &std::path::Path,
        last_offset: u64,
        session_id: &str,
    ) -> crate::services::discord::inflight::InflightTurnState {
        let mut state = crate::services::discord::inflight::InflightTurnState::new(
            provider.clone(),
            channel.get(),
            None,
            1,
            user_msg_id,
            user_msg_id + 1,
            "idle tmux stale repair".to_string(),
            Some(session_id.to_string()),
            Some(tmux.to_string()),
            Some(output_path.to_string_lossy().to_string()),
            None,
            last_offset,
        );
        state.set_relay_owner_kind(crate::services::discord::inflight::RelayOwnerKind::Watcher);
        crate::services::discord::inflight::save_inflight_state(&state)
            .expect("seed idle inflight row");
        state
    }

    async fn seed_active_mailbox_and_session(
        shared: &Arc<crate::services::discord::SharedData>,
        channel: ChannelId,
        user_msg: MessageId,
    ) -> Arc<CancelToken> {
        let token = Arc::new(CancelToken::new());
        assert!(
            super::super::super::mailbox_try_start_turn(
                shared,
                channel,
                token.clone(),
                UserId::new(7),
                user_msg,
            )
            .await
        );
        shared.restart.global_active.store(1, Ordering::Relaxed);
        shared.core.lock().await.sessions.insert(
            channel,
            super::super::super::DiscordSession {
                session_id: Some("runtime-provider-session".to_string()),
                memento_context_loaded: true,
                memento_reflected: false,
                current_path: None,
                history: Vec::new(),
                pending_uploads: Vec::new(),
                cleared: false,
                remote_profile_name: None,
                channel_id: Some(channel.get()),
                channel_name: None,
                category_name: None,
                last_active: tokio::time::Instant::now(),
                worktree: None,
                born_generation: crate::services::discord::runtime_store::load_generation(),
            },
        );
        token
    }

    async fn assert_mailbox_and_session_preserved(
        shared: &Arc<crate::services::discord::SharedData>,
        channel: ChannelId,
        user_msg: MessageId,
        token: &Arc<CancelToken>,
    ) {
        let snapshot = super::super::super::mailbox_snapshot(shared, channel).await;
        assert_eq!(snapshot.active_user_message_id, Some(user_msg));
        assert!(!token.cancelled.load(Ordering::Relaxed));
        let session_id = shared
            .core
            .lock()
            .await
            .sessions
            .get(&channel)
            .and_then(|session| session.session_id.clone());
        assert_eq!(session_id.as_deref(), Some("runtime-provider-session"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn idle_tmux_stale_turn_clear_refusal_preserves_mailbox_and_session() {
        let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let tempdir = tempfile::tempdir().expect("runtime root tempdir");
        let _env = TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            tempdir.path(),
        );
        let provider = ProviderKind::Claude;
        let registry = HealthRegistry::new();
        let shared = super::super::super::make_shared_data_for_tests();
        registry
            .register(provider.as_str().to_string(), shared.clone())
            .await;
        let channel = ChannelId::new(4_111_401);
        let stale_msg = MessageId::new(4_111_501);
        let tmux = "AgentDesk-codex-4111-health-clear-refused";
        let output_path = tempdir.path().join("health-clear-refused.jsonl");
        std::fs::write(&output_path, result_line("")).expect("write ready output fixture");
        let token = seed_active_mailbox_and_session(&shared, channel, stale_msg).await;
        seed_idle_inflight(
            &provider,
            channel,
            stale_msg.get(),
            tmux,
            &output_path,
            0,
            "stale-session",
        );

        let hook_provider = provider.clone();
        let hook_output = output_path.clone();
        let _hook = set_idle_tmux_candidate_hook(Arc::new(move |_candidate| {
            seed_idle_inflight(
                &hook_provider,
                channel,
                4_111_601,
                tmux,
                &hook_output,
                0,
                "fresh-session",
            );
        }));

        let result = super::clear_idle_tmux_stale_turn(
            &registry,
            provider.as_str(),
            channel.get(),
            tmux,
            "health_clear_refused_test",
        )
        .await;

        assert!(result.is_none());
        assert_mailbox_and_session_preserved(&shared, channel, stale_msg, &token).await;
        let persisted =
            crate::services::discord::inflight::load_inflight_state(&provider, channel.get())
                .expect("fresh row must survive refused clear");
        assert_eq!(persisted.user_msg_id, 4_111_601);
        assert_eq!(persisted.session_id.as_deref(), Some("fresh-session"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn idle_tmux_stale_turn_guarded_finish_preserves_new_mailbox_claim() {
        let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let tempdir = tempfile::tempdir().expect("runtime root tempdir");
        let _env = TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            tempdir.path(),
        );
        let provider = ProviderKind::Claude;
        let registry = HealthRegistry::new();
        let shared = super::super::super::make_shared_data_for_tests();
        registry
            .register(provider.as_str().to_string(), shared.clone())
            .await;
        let channel = ChannelId::new(4_111_403);
        let stale_msg = MessageId::new(4_111_503);
        let fresh_msg = MessageId::new(4_111_603);
        let tmux = "AgentDesk-codex-4111-health-guarded-finish";
        let output_path = tempdir.path().join("health-guarded-finish.jsonl");
        std::fs::write(&output_path, result_line("")).expect("write ready output fixture");
        seed_idle_inflight(
            &provider,
            channel,
            stale_msg.get(),
            tmux,
            &output_path,
            0,
            "stale-session",
        );

        let fresh_token = Arc::new(std::sync::Mutex::new(None::<Arc<CancelToken>>));
        let hook_token = fresh_token.clone();
        let hook_shared = shared.clone();
        let hook_provider = provider.clone();
        let hook_output = output_path.clone();
        let _hook = set_idle_tmux_post_clear_hook(Arc::new(move || {
            let shared = hook_shared.clone();
            let provider = hook_provider.clone();
            let output_path = hook_output.clone();
            let token_slot = hook_token.clone();
            Box::pin(async move {
                let token = seed_active_mailbox_and_session(&shared, channel, fresh_msg).await;
                seed_idle_inflight(
                    &provider,
                    channel,
                    fresh_msg.get(),
                    tmux,
                    &output_path,
                    0,
                    "fresh-session",
                );
                *token_slot
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner()) = Some(token);
            })
        }));

        let result = super::clear_idle_tmux_stale_turn(
            &registry,
            provider.as_str(),
            channel.get(),
            tmux,
            "health_guarded_finish_test",
        )
        .await
        .expect("stale inflight clear should complete");

        assert!(!result.had_active_turn);
        assert!(!result.runtime_session_cleared);
        let fresh_token = fresh_token
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
            .expect("fresh turn token should be seeded in the post-clear gap");
        assert_mailbox_and_session_preserved(&shared, channel, fresh_msg, &fresh_token).await;
        let persisted =
            crate::services::discord::inflight::load_inflight_state(&provider, channel.get())
                .expect("fresh row must survive guarded finish mismatch");
        assert_eq!(persisted.user_msg_id, fresh_msg.get());
        assert_eq!(persisted.session_id.as_deref(), Some("fresh-session"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn idle_tmux_stale_turn_guarded_finish_preserves_new_same_id_mailbox_claim() {
        let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let tempdir = tempfile::tempdir().expect("runtime root tempdir");
        let _env = TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            tempdir.path(),
        );
        let provider = ProviderKind::Claude;
        let registry = HealthRegistry::new();
        let shared = super::super::super::make_shared_data_for_tests();
        registry
            .register(provider.as_str().to_string(), shared.clone())
            .await;
        let channel = ChannelId::new(4_111_404);
        let user_msg = MessageId::new(4_111_504);
        let tmux = "AgentDesk-codex-4111-health-guarded-finish-same-id";
        let output_path = tempdir.path().join("health-guarded-finish-same-id.jsonl");
        std::fs::write(&output_path, result_line("")).expect("write ready output fixture");
        seed_idle_inflight(
            &provider,
            channel,
            user_msg.get(),
            tmux,
            &output_path,
            0,
            "stale-session",
        );

        let fresh_token = Arc::new(std::sync::Mutex::new(None::<Arc<CancelToken>>));
        let hook_token = fresh_token.clone();
        let hook_shared = shared.clone();
        let hook_provider = provider.clone();
        let hook_output = output_path.clone();
        let _hook = set_idle_tmux_post_clear_hook(Arc::new(move || {
            let shared = hook_shared.clone();
            let provider = hook_provider.clone();
            let output_path = hook_output.clone();
            let token_slot = hook_token.clone();
            Box::pin(async move {
                let token = seed_active_mailbox_and_session(&shared, channel, user_msg).await;
                seed_idle_inflight(
                    &provider,
                    channel,
                    user_msg.get(),
                    tmux,
                    &output_path,
                    0,
                    "fresh-session",
                );
                *token_slot
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner()) = Some(token);
            })
        }));

        let result = super::clear_idle_tmux_stale_turn(
            &registry,
            provider.as_str(),
            channel.get(),
            tmux,
            "health_guarded_finish_same_id_test",
        )
        .await
        .expect("stale inflight clear should complete");

        assert!(!result.had_active_turn);
        assert!(!result.runtime_session_cleared);
        let fresh_token = fresh_token
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
            .expect("fresh same-id turn token should be seeded in the post-clear gap");
        assert_mailbox_and_session_preserved(&shared, channel, user_msg, &fresh_token).await;
        let persisted =
            crate::services::discord::inflight::load_inflight_state(&provider, channel.get())
                .expect("fresh same-id row must survive guarded finish");
        assert_eq!(persisted.user_msg_id, user_msg.get());
        assert_eq!(persisted.session_id.as_deref(), Some("fresh-session"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn idle_tmux_stale_turn_tail_recheck_preserves_mailbox_after_precheck_passed() {
        let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let tempdir = tempfile::tempdir().expect("runtime root tempdir");
        let _env = TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            tempdir.path(),
        );
        let provider = ProviderKind::Claude;
        let registry = HealthRegistry::new();
        let shared = super::super::super::make_shared_data_for_tests();
        registry
            .register(provider.as_str().to_string(), shared.clone())
            .await;
        let channel = ChannelId::new(4_111_402);
        let user_msg = MessageId::new(4_111_502);
        let tmux = "AgentDesk-codex-4111-health-tail-recheck";
        let output_path = tempdir.path().join("health-tail-recheck.jsonl");
        let pre = result_line("");
        std::fs::write(&output_path, &pre).expect("write pre-check output fixture");
        let token = seed_active_mailbox_and_session(&shared, channel, user_msg).await;
        seed_idle_inflight(
            &provider,
            channel,
            user_msg.get(),
            tmux,
            &output_path,
            pre.len() as u64,
            "tail-session",
        );
        assert!(
            !crate::services::discord::relay_recovery::channel_has_unrelayed_idle_tmux_tail_answer(
                &provider,
                channel.get(),
            ),
            "caller pre-check fixture must pass before the tail answer is appended"
        );
        std::fs::write(
            &output_path,
            format!("{pre}{}", result_line("FINAL ANSWER")),
        )
        .expect("append tail answer fixture");

        let result = super::clear_idle_tmux_stale_turn(
            &registry,
            provider.as_str(),
            channel.get(),
            tmux,
            "health_tail_recheck_test",
        )
        .await;

        assert!(result.is_none());
        assert_mailbox_and_session_preserved(&shared, channel, user_msg, &token).await;
        assert!(
            crate::services::discord::inflight::load_inflight_state(&provider, channel.get())
                .is_some(),
            "tail-answer skip must preserve inflight for normal relay"
        );
    }

    /// #4460: the branch-4 "desynced force-clean" must NEVER terminate a live
    /// turn anymore. This fixture previously drove force-clean (delete inflight,
    /// release the mailbox cancel token, cancel the watcher, clear the turn
    /// view) — the exact path that force-killed a live codex session on a false
    /// positive. It must now leave the turn FULLY intact: the mailbox token and
    /// the inflight row both survive and nothing is counted as cleaned. (In
    /// tests there is no pg_pool, so the mention no-ops; the invariant we assert
    /// is that the turn is never destroyed.)
    #[tokio::test(flavor = "current_thread")]
    async fn branch4_suspected_stall_preserves_turn_without_force_clean() {
        let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let tempdir = tempfile::tempdir().expect("runtime root tempdir");
        let _env = TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            tempdir.path(),
        );
        let provider = ProviderKind::Codex;
        let mut registry = HealthRegistry::new();
        registry.started_at_unix =
            chrono::Utc::now().timestamp() - super::STALL_WATCHDOG_THRESHOLD_SECS as i64 - 60;
        let shared = super::super::super::make_shared_data_for_tests();
        registry
            .register(provider.as_str().to_string(), shared.clone())
            .await;
        let channel = ChannelId::new(4_111_405);
        let user_msg = MessageId::new(4_111_505);
        let stale_tmux = "AgentDesk-codex-4111-force-clean-stale";
        let stale_output = tempdir.path().join("force-clean-stale.jsonl");
        std::fs::write(&stale_output, "partial stale output\n")
            .expect("write stale output fixture");
        let stale_token = seed_active_mailbox_and_session(&shared, channel, user_msg).await;
        let mut stale_state = seed_idle_inflight(
            &provider,
            channel,
            user_msg.get(),
            stale_tmux,
            &stale_output,
            0,
            "stale-session",
        );
        let stale_at = (chrono::Local::now() - chrono::Duration::hours(5))
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
        stale_state.started_at = stale_at.clone();
        stale_state.updated_at = stale_at;
        crate::services::discord::inflight::save_inflight_state(&stale_state)
            .expect("save stale inflight timestamps");
        shared.tmux_watchers.insert(
            channel,
            watcher_handle(stale_tmux, &stale_output, Arc::new(AtomicBool::new(false))),
        );

        let cleaned = super::run_stall_watchdog_pass(&registry, &provider).await;

        // #4460: branch 4 no longer force-cleans — nothing is cleaned and the
        // live turn (mailbox token + inflight row) is fully preserved.
        assert_eq!(
            cleaned, 0,
            "suspected stall must NOT be force-cleaned; the turn stays live (#4460)"
        );
        assert_mailbox_and_session_preserved(&shared, channel, user_msg, &stale_token).await;
        assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 1);
        let persisted =
            crate::services::discord::inflight::load_inflight_state(&provider, channel.get())
                .expect("inflight row must survive — branch 4 no longer deletes it (#4460)");
        assert_eq!(persisted.user_msg_id, user_msg.get());
        assert_eq!(persisted.session_id.as_deref(), Some("stale-session"));
    }

    #[tokio::test]
    async fn stall_watchdog_cleanup_releases_residual_orphan_pending_token() {
        let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let tempdir = tempfile::tempdir().expect("runtime root tempdir");
        let _env = TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            tempdir.path(),
        );
        let provider = ProviderKind::Codex;
        let registry = HealthRegistry::new();
        let shared = super::super::super::make_shared_data_for_tests();
        registry
            .register(provider.as_str().to_string(), shared.clone())
            .await;
        let channel = ChannelId::new(3_360_101);
        let token = Arc::new(CancelToken::new());
        let started = super::super::super::mailbox_try_start_turn(
            &shared,
            channel,
            token.clone(),
            UserId::new(7),
            MessageId::new(70),
        )
        .await;
        assert!(started, "test turn should seed a residual mailbox token");
        let watcher_cancel = Arc::new(AtomicBool::new(false));
        shared.tmux_watchers.insert(
            channel,
            super::super::super::TmuxWatcherHandle {
                tmux_session_name: "AgentDesk-codex-dead-watchdog".to_string(),
                output_path: "/tmp/agentdesk-test-watchdog.jsonl".to_string(),
                paused: Arc::new(AtomicBool::new(false)),
                resume_offset: Arc::new(std::sync::Mutex::new(None)),
                cancel: watcher_cancel.clone(),
                pause_epoch: Arc::new(AtomicU64::new(0)),
                turn_delivered: Arc::new(AtomicBool::new(false)),
                last_heartbeat_ts_ms: Arc::new(AtomicI64::new(0)),
            },
        );
        assert!(
            shared.tmux_watchers.contains_key(&channel),
            "test setup must leave watcher evidence before watchdog cleanup"
        );
        shared.restart.global_active.store(1, Ordering::Relaxed);

        let released = super::super::relay_auto_heal::apply_watchdog_orphan_token_cleanup(
            &registry,
            &provider,
            shared.clone(),
            channel,
        )
        .await;

        assert!(released, "watchdog cleanup must release the orphan token");
        assert!(watcher_cancel.load(Ordering::Relaxed));
        assert!(!shared.tmux_watchers.contains_key(&channel));
        assert!(
            super::super::super::mailbox_snapshot(&shared, channel)
                .await
                .cancel_token
                .is_none()
        );
        assert!(token.cancelled.load(Ordering::Relaxed));
        assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 0);
    }
}

#[cfg(test)]
mod hard_stop_completion_event_tests {
    use std::future::Future;
    use std::io::{self, Write};
    use std::sync::atomic::Ordering;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use poise::serenity_prelude::{ChannelId, MessageId, UserId};
    use tracing_subscriber::fmt::MakeWriter;

    use super::super::HealthRegistry;
    use crate::config::TestEnvVarGuard;
    use crate::services::provider::{CancelToken, ProviderKind};
    use crate::services::turn_orchestrator::{Intervention, InterventionMode};

    #[derive(Clone)]
    struct CapturingWriter {
        buffer: Arc<Mutex<Vec<u8>>>,
    }

    impl Write for CapturingWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.buffer
                .lock()
                .expect("log buffer lock")
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for CapturingWriter {
        type Writer = CapturingWriter;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    async fn capture_warns_async<F, Fut, T>(f: F) -> (T, String)
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = T>,
    {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let writer = CapturingWriter {
            buffer: buffer.clone(),
        };
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::WARN)
            .with_ansi(false)
            .without_time()
            .with_writer(writer)
            .finish();
        let guard = tracing::subscriber::set_default(subscriber);
        let result = f().await;
        drop(guard);
        (
            result,
            String::from_utf8_lossy(&buffer.lock().expect("log buffer lock").clone()).into_owned(),
        )
    }

    fn intervention(id: u64, text: &str) -> Intervention {
        Intervention {
            author_id: UserId::new(id),
            author_is_bot: false,
            message_id: MessageId::new(id),
            queued_generation: crate::services::discord::runtime_store::load_generation(),
            source_message_ids: vec![MessageId::new(id)],
            source_message_queued_generations: Vec::new(),
            source_text_segments: Vec::new(),
            text: text.to_string(),
            mode: InterventionMode::Soft,
            created_at: std::time::Instant::now(),
            reply_context: None,
            has_reply_boundary: false,
            merge_consecutive: false,
            pending_uploads: Vec::new(),
            voice_announcement: None,
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn providerless_hard_stop_with_runtime_publishes_completion_event_for_pending_queue() {
        let tempdir = tempfile::tempdir().expect("runtime root tempdir");
        let _env = TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", tempdir.path());
        let provider = ProviderKind::Claude;
        let registry = HealthRegistry::new();
        let shared = super::super::super::make_shared_data_for_tests();
        let channel = ChannelId::new(4_048_230);
        shared
            .mailbox(channel)
            .replace_queue(
                vec![intervention(4_048_231, "pending after hard stop")],
                super::super::super::queue_persistence_context(&shared, &provider, channel),
            )
            .await;
        assert!(
            super::super::super::mailbox_try_start_turn(
                &shared,
                channel,
                Arc::new(CancelToken::new()),
                UserId::new(4_048_232),
                MessageId::new(4_048_232),
            )
            .await
        );
        registry
            .register(provider.as_str().to_string(), shared.clone())
            .await;
        let mut rx =
            super::super::super::turn_completion_events::subscribe_turn_completion_events(&shared);

        let result = super::stop_runtime_turn_preserving_watcher(
            Some(&registry),
            None,
            Some(channel.get()),
            None,
            "hard_stop_completion_event_test",
        )
        .await;

        assert!(result.had_active_turn);
        assert!(result.has_pending_queue);
        let event = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("completion event receive should not time out")
            .expect("completion event bus should remain open");
        assert_eq!(event.channel_id, channel);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn providerless_stale_repair_uses_strict_per_runtime_mailbox_ownership() {
        let tempdir = tempfile::tempdir().expect("runtime root tempdir");
        let _env = TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", tempdir.path());
        let provider = ProviderKind::Claude;
        let registry = HealthRegistry::new();
        let first = super::super::super::make_shared_data_for_tests();
        let second = super::super::super::make_shared_data_for_tests();
        let channel = ChannelId::new(4_048_250);
        let token = Arc::new(CancelToken::new());

        assert!(
            first.mailbox_peek(channel).is_none(),
            "first registered runtime must not own the test channel"
        );
        second
            .mailbox(channel)
            .replace_queue(
                vec![intervention(4_048_251, "pending on owning runtime")],
                super::super::super::queue_persistence_context(&second, &provider, channel),
            )
            .await;
        assert!(
            super::super::super::mailbox_try_start_turn(
                &second,
                channel,
                token.clone(),
                UserId::new(4_048_252),
                MessageId::new(4_048_252),
            )
            .await
        );
        let global_before =
            crate::services::turn_orchestrator::ChannelMailboxRegistry::global_handle(channel)
                .expect("owning runtime should publish global handle");
        assert_eq!(
            global_before.snapshot().await.intervention_queue.len(),
            1,
            "test setup must leave a queued item on the owning runtime"
        );

        registry
            .register(provider.as_str().to_string(), first.clone())
            .await;
        registry
            .register(provider.as_str().to_string(), second.clone())
            .await;
        let mut first_rx =
            super::super::super::turn_completion_events::subscribe_turn_completion_events(&first);
        let mut second_rx =
            super::super::super::turn_completion_events::subscribe_turn_completion_events(&second);

        let result = super::stop_providerless_runtime_turn_preserving_watcher_strict_ownership(
            Some(&registry),
            channel.get(),
            "stale_mailbox_repair",
        )
        .await;

        assert!(result.had_active_turn);
        assert!(result.has_pending_queue);
        assert!(
            token.cancelled.load(Ordering::Relaxed),
            "owning runtime's active token should be cancelled by hard-stop cleanup"
        );
        let event = tokio::time::timeout(Duration::from_secs(1), second_rx.recv())
            .await
            .expect("completion event on owning runtime should not time out")
            .expect("owning runtime completion bus should remain open");
        assert_eq!(event.channel_id, channel);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), first_rx.recv())
                .await
                .is_err(),
            "non-owning first runtime must not publish a completion event"
        );
        assert!(
            first.mailbox_peek(channel).is_none(),
            "strict provider-less repair must not create a mailbox on the first runtime"
        );

        let second_snapshot = second
            .mailbox_peek(channel)
            .expect("owning runtime mailbox should remain registered")
            .snapshot()
            .await;
        assert!(second_snapshot.cancel_token.is_none());
        assert_eq!(second_snapshot.intervention_queue.len(), 1);
        let global_after =
            crate::services::turn_orchestrator::ChannelMailboxRegistry::global_handle(channel)
                .expect("global handle should still point at the owning mailbox");
        assert_eq!(
            global_after.snapshot().await.intervention_queue.len(),
            1,
            "global handle must not be overwritten by an empty first-runtime mailbox"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn providerless_strict_repair_ignores_empty_mailbox_created_by_snapshot_scan() {
        let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let tempdir = tempfile::tempdir().expect("runtime root tempdir");
        let _env = TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            tempdir.path(),
        );
        let provider = ProviderKind::Claude;
        let registry = HealthRegistry::new();
        let first = super::super::super::make_shared_data_for_tests();
        let second = super::super::super::make_shared_data_for_tests();
        let channel = ChannelId::new(4_048_260);
        let token = Arc::new(CancelToken::new());

        second
            .mailbox(channel)
            .replace_queue(
                vec![intervention(4_048_261, "pending on second runtime")],
                super::super::super::queue_persistence_context(&second, &provider, channel),
            )
            .await;
        assert!(
            super::super::super::mailbox_try_start_turn(
                &second,
                channel,
                token.clone(),
                UserId::new(4_048_262),
                MessageId::new(4_048_262),
            )
            .await
        );
        assert!(
            first.mailbox_peek(channel).is_none(),
            "first runtime must begin without local mailbox evidence"
        );

        registry
            .register(provider.as_str().to_string(), first.clone())
            .await;
        registry
            .register(provider.as_str().to_string(), second.clone())
            .await;

        let observed = registry
            .snapshot_watcher_state(channel.get())
            .await
            .expect("providerless pre-repair snapshot should find second runtime's mailbox");
        assert!(observed.has_pending_queue);
        // cfcdcd6135572601af287e2165afae34b3ddd464 (#4068) made health
        // snapshots peek-only: observation must not materialize or globalize
        // an empty first-runtime mailbox.
        assert!(
            first.mailbox_peek(channel).is_none(),
            "providerless snapshot scan must leave the first runtime without local mailbox evidence"
        );
        let global_after_observation =
            crate::services::turn_orchestrator::ChannelMailboxRegistry::global_handle(channel)
                .expect("snapshot scan should leave a process-global handle");
        let global_snapshot = global_after_observation.snapshot().await;
        assert!(global_snapshot.cancel_token.is_some());
        assert!(
            !global_snapshot.intervention_queue.is_empty(),
            "pre-repair observation should keep the global handle on the owning second runtime"
        );

        let mut first_rx =
            super::super::super::turn_completion_events::subscribe_turn_completion_events(&first);
        let mut second_rx =
            super::super::super::turn_completion_events::subscribe_turn_completion_events(&second);

        let result = super::stop_providerless_runtime_turn_preserving_watcher_strict_ownership(
            Some(&registry),
            channel.get(),
            "stale_mailbox_repair",
        )
        .await;

        assert!(
            result.had_active_turn,
            "strict repair must select and finalize the owning second runtime"
        );
        assert!(result.has_pending_queue);
        assert!(
            token.cancelled.load(Ordering::Relaxed),
            "second runtime's active token should be cancelled by selected repair"
        );
        let event = tokio::time::timeout(Duration::from_secs(1), second_rx.recv())
            .await
            .expect("owning second runtime should publish completion event")
            .expect("owning second runtime completion bus should remain open");
        assert_eq!(event.channel_id, channel);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), first_rx.recv())
                .await
                .is_err(),
            "empty first runtime must not be selected or publish completion"
        );

        let first_after = first.mailbox_peek(channel);
        assert!(
            first_after.is_none(),
            "empty first runtime mailbox should not be created by observation or repair"
        );
        let second_after = second
            .mailbox_peek(channel)
            .expect("owning second runtime mailbox should remain registered")
            .snapshot()
            .await;
        assert!(second_after.cancel_token.is_none());
        assert_eq!(second_after.intervention_queue.len(), 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn unresolvable_raw_hard_stop_warns_about_potentially_stranded_pending_queue() {
        let tempdir = tempfile::tempdir().expect("runtime root tempdir");
        let _env = TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", tempdir.path());
        let provider = ProviderKind::Claude;
        let shared = super::super::super::make_shared_data_for_tests();
        let channel = ChannelId::new(4_048_240);
        shared
            .mailbox(channel)
            .replace_queue(
                vec![intervention(4_048_241, "pending without runtime")],
                super::super::super::queue_persistence_context(&shared, &provider, channel),
            )
            .await;
        assert!(
            super::super::super::mailbox_try_start_turn(
                &shared,
                channel,
                Arc::new(CancelToken::new()),
                UserId::new(4_048_242),
                MessageId::new(4_048_242),
            )
            .await
        );

        let (result, logs) = capture_warns_async(|| async {
            super::stop_runtime_turn_preserving_watcher(
                None,
                None,
                Some(channel.get()),
                None,
                "hard_stop_unresolvable_test",
            )
            .await
        })
        .await;

        assert!(result.had_active_turn);
        assert!(result.has_pending_queue);
        assert!(
            logs.contains("raw hard_stop fallback could not resolve the owning runtime"),
            "strand warning message missing from logs: {logs}"
        );
        assert!(
            logs.contains(&format!("channel_id={}", channel.get())),
            "strand warning must include channel_id: {logs}"
        );
        assert!(
            logs.contains("has_pending=true"),
            "strand warning must include has_pending=true: {logs}"
        );
    }
}

#[cfg(test)]
mod explicit_background_watchdog_cleanup_tests {
    use super::{
        ExplicitBackgroundWatchdogClear, cleanup_then_submit_explicit_background_watchdog_cancel,
        stop_hard_stop_cleanup_watcher_if_current,
    };
    use crate::services::discord::inflight::GuardedClearOutcome;
    use crate::services::provider::ProviderKind;
    use poise::serenity_prelude::ChannelId;
    use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    fn watcher(
        tmux_session_name: &str,
        output_path: &str,
        cancel: Arc<AtomicBool>,
    ) -> super::super::super::TmuxWatcherHandle {
        super::super::super::TmuxWatcherHandle {
            tmux_session_name: tmux_session_name.to_string(),
            output_path: output_path.to_string(),
            paused: Arc::new(AtomicBool::new(false)),
            resume_offset: Arc::new(std::sync::Mutex::new(None)),
            cancel,
            pause_epoch: Arc::new(AtomicU64::new(0)),
            turn_delivered: Arc::new(AtomicBool::new(false)),
            last_heartbeat_ts_ms: Arc::new(AtomicI64::new(0)),
        }
    }

    #[tokio::test]
    async fn explicit_background_cleanup_precedes_release_and_preserves_new_watcher() {
        let shared = super::super::super::make_shared_data_for_tests();
        let channel = ChannelId::new(4_019_402_900);
        let old_cancel = Arc::new(AtomicBool::new(false));
        let new_cancel = Arc::new(AtomicBool::new(false));
        shared.tmux_watchers.insert(
            channel,
            watcher(
                "AgentDesk-codex-old-explicit-background",
                "/tmp/agentdesk-old-explicit-background.jsonl",
                old_cancel.clone(),
            ),
        );
        let revalidated = ExplicitBackgroundWatchdogClear {
            clear_outcome: GuardedClearOutcome::Cleared,
            pending_hourglass_user_msg_id: Some(4_019_402_901),
            finalizer_turn_id: 4_019_402_901,
        };
        let order = Arc::new(Mutex::new(Vec::new()));

        let has_pending = cleanup_then_submit_explicit_background_watchdog_cancel(
            &shared,
            &ProviderKind::Codex,
            channel,
            &revalidated,
            Some("AgentDesk-codex-old-explicit-background"),
            {
                let shared = shared.clone();
                let old_cancel = old_cancel.clone();
                let new_cancel = new_cancel.clone();
                let order = order.clone();
                move || async move {
                    assert!(
                        old_cancel.load(Ordering::Relaxed),
                        "old watcher must be stopped before finalizer release"
                    );
                    assert!(
                        !shared.tmux_watchers.contains_key(&channel),
                        "old watcher must be removed before finalizer release"
                    );
                    order
                        .lock()
                        .expect("order lock")
                        .extend(["cleanup", "finalizer_release"]);
                    shared.tmux_watchers.insert(
                        channel,
                        watcher(
                            "AgentDesk-codex-new-explicit-background",
                            "/tmp/agentdesk-new-explicit-background.jsonl",
                            new_cancel.clone(),
                        ),
                    );
                    super::super::super::turn_finalizer::FinalizeOutcome::Finalized {
                        removed_token: None,
                        has_pending: false,
                        mailbox_online: true,
                    }
                }
            },
        )
        .await;

        assert!(!has_pending);
        assert_eq!(
            *order.lock().expect("order lock"),
            ["cleanup", "finalizer_release"]
        );
        assert!(
            shared.tmux_watchers.contains_key(&channel),
            "watcher registered after finalizer release must survive pass completion"
        );
        let binding = shared
            .tmux_watchers
            .channel_binding(&channel)
            .expect("new watcher binding");
        assert_eq!(
            binding.tmux_session_name,
            "AgentDesk-codex-new-explicit-background"
        );
        assert!(!new_cancel.load(Ordering::Relaxed));
    }

    #[test]
    fn hard_stop_cleanup_watcher_stop_is_tmux_session_conditional() {
        let shared = super::super::super::make_shared_data_for_tests();
        let channel = ChannelId::new(4_019_402_901);
        let cancel = Arc::new(AtomicBool::new(false));
        shared.tmux_watchers.insert(
            channel,
            watcher(
                "AgentDesk-codex-current-turn",
                "/tmp/agentdesk-current-turn.jsonl",
                cancel.clone(),
            ),
        );

        assert!(!stop_hard_stop_cleanup_watcher_if_current(
            &shared,
            channel,
            Some("AgentDesk-codex-stalled-turn"),
        ));
        assert!(
            shared.tmux_watchers.contains_key(&channel),
            "mismatched current watcher must remain registered"
        );
        assert!(!cancel.load(Ordering::Relaxed));

        assert!(stop_hard_stop_cleanup_watcher_if_current(
            &shared,
            channel,
            Some("AgentDesk-codex-current-turn"),
        ));
        assert!(!shared.tmux_watchers.contains_key(&channel));
        assert!(cancel.load(Ordering::Relaxed));
    }
}

#[cfg(test)]
mod owning_runtime_http_tests {
    use super::super::HealthRegistry;
    use crate::services::provider::ProviderKind;
    use poise::serenity_prelude::ChannelId;

    #[tokio::test]
    async fn owning_runtime_http_uses_channel_matched_runtime() {
        let provider = ProviderKind::Claude;
        let registry = HealthRegistry::new();
        let first = super::super::super::make_shared_data_for_tests();
        let second = super::super::super::make_shared_data_for_tests();
        let first_channel = ChannelId::new(401_900_000_000_001);
        let second_channel = ChannelId::new(401_900_000_000_002);

        {
            let mut settings = first.settings.write().await;
            settings.allowed_channel_ids = vec![first_channel.get()];
        }
        {
            let mut settings = second.settings.write().await;
            settings.allowed_channel_ids = vec![second_channel.get()];
        }
        let _ = second
            .http
            .cached_bot_token
            .set("test-owning-runtime-token".to_string());

        registry
            .register(provider.as_str().to_string(), first.clone())
            .await;
        registry
            .register(provider.as_str().to_string(), second.clone())
            .await;

        assert!(
            super::owning_runtime_http_for_channel(&registry, &provider, second_channel)
                .await
                .is_some(),
            "channel-aware lookup must select the second same-provider runtime"
        );
        assert!(
            super::owning_runtime_http_for_channel(&registry, &provider, first_channel)
                .await
                .is_none(),
            "a first-runtime/name-only lookup would mask this missing owner HTTP"
        );
    }
}
