use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use poise::serenity_prelude::{self as serenity, ChannelId, MessageId};
use serde::Serialize;

use crate::services::discord::relay_health::{RelayActiveTurn, RelayStallState};
use crate::services::discord::{self as discord, SharedData};
use crate::services::provider::{CancelToken, ProviderKind};

use super::HealthRegistry;
use super::{relay_auto_heal, relay_dead_reattach, stall_liveness, watcher_respawn};

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

fn idle_tmux_repair_ready_for_input(
    provider: &ProviderKind,
    channel_id: u64,
    tmux_session: &str,
) -> bool {
    let structured_ready = super::super::inflight::load_inflight_state(provider, channel_id)
        .and_then(|state| {
            let output_path = state
                .output_path
                .as_deref()
                .map(str::trim)
                .filter(|path| !path.is_empty())?;
            crate::services::tui_turn_state::jsonl_ready_for_input(
                provider,
                state.runtime_kind,
                std::path::Path::new(output_path),
                Some(state.last_offset),
            )
        });
    structured_ready
        .map(crate::services::tui_turn_state::TuiReadyState::is_ready)
        .unwrap_or_else(|| {
            crate::services::provider::tmux_session_ready_for_input(tmux_session, provider)
        })
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

async fn find_runtime_channel_match(
    registry: &HealthRegistry,
    provider_name: Option<&str>,
    channel_id: Option<ChannelId>,
    tmux_name: Option<&str>,
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
            if has_session || discord::ChannelMailboxRegistry::global_handle(channel_id).is_some() {
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
    stop_source: &'static str,
    stop_watcher: bool,
) -> bool {
    if let Some(token) = finish.removed_token.as_ref() {
        token.cancelled.store(true, Ordering::Relaxed);
        discord::saturating_decrement_global_active(shared);
    }

    discord::clear_watchdog_deadline_override(channel_id.get()).await;
    shared
        .dispatch
        .thread_parents
        .retain(|_, thread| *thread != channel_id);
    shared.restart.recovering_channels.remove(&channel_id);
    shared.turn_start_times.remove(&channel_id);

    if !finish.has_pending {
        shared.dispatch.role_overrides.remove(&channel_id);
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
    let provider = ProviderKind::from_str(provider_name)?;
    if !idle_tmux_repair_ready_for_input(&provider, channel_id, tmux_session) {
        return None;
    }

    let channel_id = ChannelId::new(channel_id);
    let shared = shared_for_provider(registry, &provider, channel_id).await?;
    let finish = discord::mailbox_finish_turn(&shared, &provider, channel_id).await;
    let runtime_session_cleared = apply_runtime_hard_stop_cleanup(
        &shared,
        &provider,
        channel_id,
        &finish,
        stop_source,
        false,
    )
    .await;
    let persistent_inflight_cleared = discord::clear_inflight_state(&provider, channel_id.get());

    Some(IdleTmuxStaleTurnRepairResult {
        had_active_turn: finish.removed_token.is_some(),
        has_pending_queue: finish.has_pending,
        persistent_inflight_cleared,
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
    let Some(runtime) =
        find_runtime_channel_match(registry, provider_name, Some(channel_id), None).await
    else {
        return FinishCancelledMailboxResult::default();
    };

    let before = runtime.shared.restart.global_active.load(Ordering::Acquire);
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
) -> HardStopRuntimeResult {
    let channel_id = channel_id.map(ChannelId::new);

    if let Some(registry) = registry
        && let Some(runtime) =
            find_runtime_channel_match(registry, provider_name, channel_id, tmux_name).await
    {
        let finish =
            discord::mailbox_finish_turn(&runtime.shared, &runtime.provider, runtime.channel_id)
                .await;
        let runtime_session_cleared = apply_runtime_hard_stop_cleanup(
            &runtime.shared,
            &runtime.provider,
            runtime.channel_id,
            &finish,
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
            .or_else(|| {
                session_key
                    .and_then(|key| key.split_once(':'))
                    .map(|(_, tmux_name)| tmux_name.to_string())
            })
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

/// #896: Parsed `/api/inflight/rebind` body, extracted for unit-test
/// coverage of input validation without spinning up a `HealthRegistry`.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct ParsedRebindRequest {
    pub(super) provider: crate::services::provider::ProviderKind,
    pub(super) channel_id: u64,
    pub(super) tmux_session: Option<String>,
}

/// #896: Parse and validate the rebind request body. Returns a status-tuple
/// error on malformed input so the caller can surface it verbatim.
pub(super) fn parse_rebind_body(body: &str) -> Result<ParsedRebindRequest, (&'static str, String)> {
    let json: serde_json::Value = serde_json::from_str(body).map_err(|_| {
        (
            "400 Bad Request",
            r#"{"ok":false,"error":"invalid JSON"}"#.to_string(),
        )
    })?;

    let provider_raw = json
        .get("provider")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    let provider =
        crate::services::provider::ProviderKind::from_str(provider_raw).ok_or_else(|| {
            (
                "400 Bad Request",
                r#"{"ok":false,"error":"provider must be one of: claude, codex, gemini, opencode, qwen"}"#.to_string(),
            )
        })?;

    // Accept channel_id as either a JSON number or a decimal string so
    // callers can forward snowflake IDs without precision loss.
    let channel_id: u64 = match json.get("channel_id") {
        Some(v) if v.is_u64() => v.as_u64().unwrap_or(0),
        Some(v) if v.is_string() => v.as_str().unwrap_or("").trim().parse::<u64>().unwrap_or(0),
        _ => 0,
    };
    if channel_id == 0 {
        return Err((
            "400 Bad Request",
            r#"{"ok":false,"error":"channel_id is required (non-zero u64)"}"#.to_string(),
        ));
    }

    let tmux_session = json
        .get("tmux_session")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    Ok(ParsedRebindRequest {
        provider,
        channel_id,
        tmux_session,
    })
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
///   "tmux_session": "AgentDesk-codex-foo"   // optional — derived otherwise
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
    } = parsed;

    let Some(result) = registry
        .rebind_inflight(&provider, channel_id, tmux_override)
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

/// #1446 stall-deadlock recovery — pure decision helper for the
/// `stall_watchdog` periodic loop. Returns `true` when the watchdog should
/// force-clean a watcher's state. The caller is responsible for actually
/// invoking the cleanup (so the helper can be exercised by unit tests
/// without a live `SharedData`).
///
/// All gates must hold:
/// - `attached == true` and `desynced == true` (snapshot already classified
///   the watcher as detached/diverged), AND
/// - `inflight_started_at` is older than `threshold_secs` seconds
///   (defaults to `2 * INFLIGHT_STALENESS_THRESHOLD_SECS`), AND
/// - `terminal_delivery_committed == false` (the in-flight row is NOT a
///   normally-completed turn that is merely sleeping; see below).
///
/// Either staleness signal alone is insufficient — a fresh desynced watcher
/// might just be mid-stream and a stale-but-synced one might be waiting on an
/// idle agent. The conjunction is the actual stall pattern from issue
/// #1446 (parent channel queues forever because thread inflight stayed
/// behind after the dispatch terminated).
///
/// #3041 B: decide whether a force-cleaned turn's provider session selector
/// should be PRESERVED (persisted to DB so the next turn `--resume`s the same
/// provider session) or DISCARDED (next turn cold-starts a fresh session).
///
/// Preserve only when we both KNOW the selector and have positive evidence the
/// underlying session is intact:
///   - `terminal_delivery_committed`: the turn finished and delivered its
///     answer, so the session is idle-but-healthy and fully resumable; OR
///   - `tmux_session_alive == Some(true)`: the provider pane is still live, so
///     the transcript is coherent up to the interruption and `--resume` grafts
///     clean context.
///
/// Discard when the selector is unknown, or the pane is dead AND the turn never
/// committed — the genuine hang / abnormal-exit signature where the transcript
/// may be truncated mid-write and resuming would carry corrupt context into the
/// next turn. Discarding lets the next turn cold-start cleanly.
pub(crate) fn force_clean_should_preserve_resume_selector(
    session_id: Option<&str>,
    session_key: Option<&str>,
    terminal_delivery_committed: bool,
    tmux_session_alive: Option<bool>,
) -> bool {
    let has_selector = session_id.is_some_and(|s| !s.trim().is_empty())
        && session_key.is_some_and(|s| !s.trim().is_empty());
    if !has_selector {
        return false;
    }
    terminal_delivery_committed || tmux_session_alive == Some(true)
}

/// #3041 B side-effecting wrapper: classify via
/// `force_clean_should_preserve_resume_selector` and, on the preserve branch,
/// persist the selector to DB so the next turn restores it
/// (`db_provider_session_restored`) instead of falling to
/// `no_cached_provider_session`. The discard branch is a no-op (the next turn
/// cold-starts) but is logged so the distinction is observable.
async fn preserve_resume_selector_on_force_clean(
    provider: &ProviderKind,
    channel_id: ChannelId,
    shared: &Arc<SharedData>,
    inflight: Option<&discord::inflight::InflightTurnState>,
    tmux_session_alive: Option<bool>,
) {
    let Some(inflight) = inflight else {
        return;
    };
    let preserve = force_clean_should_preserve_resume_selector(
        inflight.session_id.as_deref(),
        inflight.session_key.as_deref(),
        inflight.terminal_delivery_committed,
        tmux_session_alive,
    );
    let ts = chrono::Local::now().format("%H:%M:%S");
    let (Some(session_id), Some(session_key)) = (
        inflight
            .session_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty()),
        inflight
            .session_key
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty()),
    ) else {
        // No selector to preserve — next turn cold-starts regardless.
        return;
    };
    if !preserve {
        tracing::info!(
            "  [{ts}] 🧹 STALL-WATCHDOG: discarding resume selector for channel {} (provider={}, committed={}, pane_alive={:?}) — next turn cold-starts",
            channel_id,
            provider.as_str(),
            inflight.terminal_delivery_committed,
            tmux_session_alive,
        );
        return;
    }
    discord::adk_session::save_provider_session_id(
        session_key,
        session_id,
        Some(session_id),
        provider,
        channel_id,
        shared.api_port,
    )
    .await;
    tracing::info!(
        "  [{ts}] ♻ STALL-WATCHDOG: preserved resume selector for channel {} (provider={}, session_key={}) — next turn will --resume",
        channel_id,
        provider.as_str(),
        session_key,
    );
}

/// #3126 false-positive guard: a turn that finished normally commits its
/// terminal response to the outbound delivery path
/// (`InflightTurnState::terminal_delivery_committed`) and then leaves the
/// session idle — e.g. the agent scheduled a `ScheduleWakeup` or the loop
/// wound down with a `stop_hook_summary`/`turn_duration` transcript record and
/// no further events. That idle row goes stale (no relay writes) and can read
/// as `desynced` (#2965: a ready-for-input TUI has capture bytes past the
/// relay offsets), which previously tripped the desynced force-clean and
/// killed a perfectly healthy wakeup-waiting session. Excluding committed
/// turns keeps the watchdog targeting only genuinely hung (never-completed)
/// turns.
///
/// #3041 post-restart grace: the current turn's `started_at` may predate a
/// dcserver restart. Right after deploy/restart every watcher is transiently
/// `desynced` (relay offsets not yet re-synced), so the bare
/// `now - started_at >= threshold` test could fire immediately and force-kill
/// a perfectly healthy work session that simply hadn't re-synced yet. Anchoring
/// the age at `max(started_at, boot)` restarts the staleness clock at boot,
/// giving the watcher a full `threshold_secs` window after restart to re-sync
/// (which clears `desynced` and the kill never happens). A genuinely hung turn
/// stays desynced past that window and is still cleaned.
/// #3656: age from the current turn's `started_at` (not `updated_at`) so consecutive short turns under one session key don't accumulate into a fake stall.
pub(crate) fn stall_watchdog_should_force_clean(
    attached: bool,
    desynced: bool,
    inflight_terminal_delivery_committed: bool,
    inflight_started_at: Option<&str>,
    now_unix_secs: i64,
    threshold_secs: u64,
    boot_unix_secs: i64,
) -> bool {
    if !attached || !desynced {
        return false;
    }
    // #3126: a normally-completed turn that is now idle (wakeup/loop
    // wind-down) is not a hang — never force-clean it.
    if inflight_terminal_delivery_committed {
        return false;
    }
    let Some(started_at) = inflight_started_at else {
        return false;
    };
    let Some(started_at_unix) = discord::inflight::parse_updated_at_unix(started_at) else {
        return false;
    };
    // #3041: never count staleness that accrued before this process booted —
    // a pre-restart turn `started_at` must not instantly satisfy the
    // threshold the moment the watchdog's initial delay elapses.
    let age_anchor = started_at_unix.max(boot_unix_secs);
    let age_secs = now_unix_secs.saturating_sub(age_anchor);
    age_secs >= 0 && (age_secs as u64) >= threshold_secs
}

/// Detection-only counterpart to `stall_watchdog_should_force_clean`:
/// returns `true` for the "completed-stale inflight on a healthy watcher"
/// pattern that the deadlock-manager 30-min alarms keep flagging. All five
/// signals must hold:
/// - `attached == true` and `desynced == false` (relay is fine)
/// - `inflight_state_present == true` (a stale file exists)
/// - `mailbox_active_user_msg_id.is_none()` (no active turn anchor)
/// - `tmux_session_alive == Some(true)` (session still waiting for input)
/// - `inflight_updated_at` older than `threshold_secs`
///
/// Callers must NOT clean on this signal alone — the user may be reading the
/// delivered response and about to send the next message. The helper exists
/// so the watchdog can emit telemetry without altering recovery behaviour.
pub(crate) fn inflight_completed_stale_leak_detected(
    attached: bool,
    desynced: bool,
    inflight_state_present: bool,
    mailbox_active_user_msg_id: Option<u64>,
    inflight_updated_at: Option<&str>,
    tmux_session_alive: Option<bool>,
    now_unix_secs: i64,
    threshold_secs: u64,
) -> bool {
    if !attached || desynced {
        return false;
    }
    if !inflight_state_present {
        return false;
    }
    if mailbox_active_user_msg_id.is_some() {
        return false;
    }
    if tmux_session_alive != Some(true) {
        return false;
    }
    let Some(updated_at) = inflight_updated_at else {
        return false;
    };
    let Some(updated_at_unix) = discord::inflight::parse_updated_at_unix(updated_at) else {
        return false;
    };
    let age_secs = now_unix_secs.saturating_sub(updated_at_unix);
    age_secs >= 0 && (age_secs as u64) >= threshold_secs
}

/// #3629: clean-vs-preserve fork for a completed-stale inflight that has NO
/// unrelayed answer. Reached only after [`inflight_completed_stale_leak_detected`]
/// already held — i.e. the relay is healthy, the mailbox has NO active turn,
/// the tmux session is alive, and the row is stale. The sole remaining
/// discriminator is whether the turn ever committed a terminal delivery:
///
/// - `terminal_delivery_committed == true` → the answer WAS delivered and the
///   session is merely idle now (e.g. a #3126 wakeup-waiting loop, or a turn
///   whose answer the watcher relayed). PRESERVE — the user may still send the
///   next message and the delivered response must not be disturbed.
/// - `terminal_delivery_committed == false` → nothing was ever delivered AND
///   there is nothing left to deliver (no unrelayed answer): a NO_REPLY / empty
///   terminal turn. The bridge left an inflight row that no answer will ever
///   fill and that no live turn owns, so it never self-clears and the external
///   deadlock monitor flags it every ~30 min forever (#3629). CLEAN it.
///
/// The removal at the call site is identity-guarded against the on-disk
/// `user_msg_id`, so a newer turn's row is never clobbered — this predicate only
/// decides intent. Kept as a pure seam so the fork is unit-testable without
/// driving the watchdog loop.
///
/// `this_turn_user_msg_id == 0` is NEVER cleaned (codex #3629 review): a zero-id
/// row cannot be distinguished from a LIVE recovery/TUI-direct turn
/// (`RecoveryKickoff` holds a live cancel_token with `active_user_message_id =
/// None`, so the "no active mailbox turn" precondition does not prove it is
/// dead) nor from a NEWER pinned zero-id turn (the zero-owned guard only checks
/// `user_msg_id == 0`, not identity). Only a real, non-zero user_msg_id can be
/// identity-guarded safely, so zero-id rows keep the prior detection-only
/// behavior.
pub(crate) fn completed_stale_no_answer_orphan_should_clean(
    terminal_delivery_committed: bool,
    this_turn_user_msg_id: u64,
) -> bool {
    !terminal_delivery_committed && this_turn_user_msg_id != 0
}

fn outbound_activity_is_recent(
    last_outbound_activity_ms: Option<i64>,
    now_unix_secs: i64,
    threshold_secs: u64,
) -> bool {
    let Some(last_outbound_activity_ms) = last_outbound_activity_ms else {
        return false;
    };
    let now_ms = now_unix_secs.saturating_mul(1000);
    if last_outbound_activity_ms >= now_ms {
        return true;
    }
    let age_ms = now_ms.saturating_sub(last_outbound_activity_ms) as u64;
    age_ms < threshold_secs.saturating_mul(1000)
}

pub(crate) fn stale_idle_foreground_queue_detected(
    active_turn: RelayActiveTurn,
    mailbox_has_cancel_token: bool,
    _queue_depth: usize,
    inflight_state_present: bool,
    inflight_updated_at: Option<&str>,
    tmux_session_alive: Option<bool>,
    last_outbound_activity_ms: Option<i64>,
    now_unix_secs: i64,
    threshold_secs: u64,
) -> bool {
    // Queue depth is intentionally ignored: a stale foreground health anchor
    // can strand health even when no user intervention is queued behind it.
    if active_turn != RelayActiveTurn::Foreground
        || !mailbox_has_cancel_token
        || !inflight_state_present
        || tmux_session_alive != Some(true)
        || outbound_activity_is_recent(last_outbound_activity_ms, now_unix_secs, threshold_secs)
    {
        return false;
    }
    let Some(updated_at) = inflight_updated_at else {
        return false;
    };
    let Some(updated_at_unix) = discord::inflight::parse_updated_at_unix(updated_at) else {
        return false;
    };
    let age_secs = now_unix_secs.saturating_sub(updated_at_unix);
    age_secs >= 0 && (age_secs as u64) >= threshold_secs
}

pub(crate) fn stall_watchdog_should_force_clean_orphan_explicit_background_work(
    relay_stall_state: RelayStallState,
    attached: bool,
    watcher_owner_channel_id: Option<u64>,
    channel_id: u64,
    desynced: bool,
    inflight_state_present: bool,
    inflight_updated_at: Option<&str>,
    tmux_session_alive: Option<bool>,
    unread_bytes: Option<u64>,
    last_outbound_activity_ms: Option<i64>,
    now_unix_secs: i64,
    threshold_secs: u64,
) -> bool {
    if relay_stall_state != RelayStallState::ExplicitBackgroundWork
        || !attached
        || watcher_owner_channel_id != Some(channel_id)
        || desynced
        || !inflight_state_present
        || tmux_session_alive != Some(true)
        || unread_bytes != Some(0)
        || last_outbound_activity_ms.is_none()
        || outbound_activity_is_recent(last_outbound_activity_ms, now_unix_secs, threshold_secs)
    {
        return false;
    }

    let Some(updated_at) = inflight_updated_at else {
        return false;
    };
    let Some(updated_at_unix) = discord::inflight::parse_updated_at_unix(updated_at) else {
        return false;
    };
    let age_secs = now_unix_secs.saturating_sub(updated_at_unix);
    age_secs >= 0 && (age_secs as u64) >= threshold_secs
}

/// Watchdog tick interval. Picked to converge inside ~1 cycle once the
/// `2x` staleness window has elapsed, while staying well below the
/// gateway-lease keepalive cadence so we never starve the gateway loop.
pub(crate) const STALL_WATCHDOG_INTERVAL_SECS: u64 = 30;

/// Initial delay before the first watchdog pass — mirrors
/// `placeholder_sweeper::INITIAL_DELAY_SECS` so we never observe a freshly
/// recovered turn as "desynced" mid-bootstrap.
pub(crate) const STALL_WATCHDOG_INITIAL_DELAY_SECS: u64 = 90;

/// Force-cleanup window; strictly larger than THREAD-GUARD staleness so the
/// watchdog never races ahead of an in-flight intake call.
pub(crate) const STALL_WATCHDOG_THRESHOLD_SECS: u64 =
    2 * discord::inflight::INFLIGHT_STALENESS_THRESHOLD_SECS;

/// #3169: freshness window for the jsonl-mtime liveness probe. Provider events
/// inside this staleness window prove loop mid-write, not a hung desync.
pub(crate) const STALL_WATCHDOG_LIVENESS_FRESHNESS_SECS: u64 = STALL_WATCHDOG_THRESHOLD_SECS;

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
        if crate::services::discord::relay_recovery::idle_tmux_repair_has_unrelayed_tail_answer(
            provider,
            channel_id.get(),
        ) {
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
            && let Some(result) = clear_idle_tmux_stale_turn(
                registry,
                provider.as_str(),
                channel_id.get(),
                &tmux_session,
                "2965_stale_idle_foreground_queue_watchdog",
            )
            .await
        {
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
            let pending_hourglass_user_msg_id =
                discord::inflight::load_inflight_state(provider, channel_id.get())
                    .filter(|state| state.user_msg_id != 0)
                    .map(|state| state.user_msg_id);
            discord::inflight::delete_inflight_state_file(provider, channel_id.get());
            let finish = discord::mailbox_finish_turn(&shared, provider, channel_id).await;
            apply_runtime_hard_stop_cleanup(
                &shared,
                provider,
                channel_id,
                &finish,
                "2967_orphan_explicit_background_watchdog",
                true,
            )
            .await;
            if !finish.has_pending {
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
            if let Some(user_msg_id) = pending_hourglass_user_msg_id
                && let Ok(http) = super::resolve_bot_http(registry, provider.as_str()).await
            {
                discord::formatting::remove_reaction_raw(
                    &http,
                    channel_id,
                    user_msg_id.into(),
                    '⏳',
                )
                .await;
            }
            cleaned += 1;
            continue;
        }

        let should_clean = stall_watchdog_should_force_clean(
            snapshot.attached,
            snapshot.desynced,
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
        // Force cleanup releases the durable inflight lock first, then asks
        // relay_recovery to clear the mailbox only if its fresh safety
        // predicate still sees no bridge, watcher, or live tmux evidence.
        // That keeps token cleanup on the same audited path as the
        // operator-facing orphan_pending_token recovery and avoids a second
        // local interpretation of "safe to release".
        // #1914: capture user_msg_id BEFORE deleting the inflight state file
        // so we can scrub the ⏳ reaction the bridge added at turn start. The
        // normal cleanup paths (`turn_bridge::mod.rs:3047-3048` and the four
        // `tmux_watcher` finalize sites) all skip this code path because the
        // turn never reached a watcher-side completion event.
        let force_clean_inflight = force_clean_inflight
            .or_else(|| discord::inflight::load_inflight_state(provider, channel_id.get()));
        let pending_hourglass_user_msg_id = force_clean_inflight
            .as_ref()
            .filter(|state| state.user_msg_id != 0)
            .map(|state| state.user_msg_id);
        // #3041 B: before we tear the turn down, decide whether the next turn
        // should `--resume` this provider session or cold-start. A force-clean
        // that fires on a still-healthy session (watcher desync, post-restart
        // re-sync lag) must NOT silently drop the user's work, while a genuine
        // hang / abnormal-exit must NOT graft a possibly-truncated transcript
        // onto the next turn. `preserve_resume_selector_on_force_clean`
        // persists the selector to DB only on the safe branch.
        preserve_resume_selector_on_force_clean(
            provider,
            channel_id,
            &shared,
            force_clean_inflight.as_ref(),
            snapshot.tmux_session_alive,
        )
        .await;
        discord::inflight::delete_inflight_state_file(provider, channel_id.get());
        let _ = relay_auto_heal::apply_watchdog_orphan_token_cleanup(
            registry,
            provider,
            shared.clone(),
            channel_id,
        )
        .await;
        // #3410: the cleanup above cancelled the watcher and (on the
        // tmux_alive_relay_dead branch) skipped relay recovery — release the
        // stale mailbox ownership it left and respawn the watcher on the still-
        // live tmux session. Whoever kills the watcher owns the respawn.
        watcher_respawn::complete_force_clean_watcher_recovery(
            registry,
            provider,
            &shared,
            channel_id,
            &snapshot,
            now_unix_secs,
        )
        .await;
        shared
            .dispatch
            .thread_parents
            .retain(|_, thread_id| *thread_id != channel_id);
        if let Some(user_msg_id) = pending_hourglass_user_msg_id
            && let Ok(http) = super::resolve_bot_http(registry, provider.as_str()).await
        {
            discord::formatting::remove_reaction_raw(&http, channel_id, user_msg_id.into(), '⏳')
                .await;
        }
        stall_liveness::clear_stall_watchdog_liveness_state(
            provider,
            channel_id,
            snapshot.tmux_session.as_deref(),
        );
        cleaned += 1;
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

/// #2860 — pure decision: the unrelayed byte range of `full_response` that a
/// completed-stale leak recovery should deliver. `start` is `response_sent_offset`
/// snapped down to a UTF-8 char boundary (Korean/multibyte safety) and clamped to
/// len; `end` is the full length. Returns `None` when nothing is unrelayed
/// (`start >= len`), making a repeated watchdog pass an idempotent no-op once the
/// offset has been advanced to len.
///
/// `last_watcher_relayed_offset` is deliberately NOT mixed into this range:
/// it is a tmux output-buffer coordinate, not a `full_response` byte index, so
/// max()-ing it against `response_sent_offset` could both over- and under-skip.
/// The authoritative delivered/not-delivered decision is the live-message probe,
/// not this offset; this range only bounds WHAT to send once the probe confirms
/// the message is still an undelivered placeholder.
fn leak_recovery_unrelayed_range(
    full_response: &str,
    response_sent_offset: usize,
) -> Option<(usize, usize)> {
    let len = full_response.len();
    let mut start = response_sent_offset.min(len);
    while start > 0 && !full_response.is_char_boundary(start) {
        start -= 1;
    }
    if start >= len {
        None
    } else {
        Some((start, len))
    }
}

/// #2860 — pure render: format the unrelayed tail exactly as the bridge's
/// terminal-replace path would (strip TUI chrome, then status-panel or provider
/// formatting selected by the same flag). Returns `None` when the tail strips or
/// formats to empty — recovery must never post a placeholder or an empty notice,
/// only real leaked content.
fn render_leak_recovery_delivery(
    full_response: &str,
    start: usize,
    status_panel_v2_enabled: bool,
    provider: &ProviderKind,
) -> Option<String> {
    let raw_tail = full_response.get(start..)?;
    let stripped = discord::response_sanitizer::strip_leading_tui_response_chrome(raw_tail);
    // Mirror terminal_delivery_response_after_offset: if the raw tail had content
    // but it was all chrome, there is nothing real to deliver.
    if !raw_tail.trim().is_empty() && stripped.trim().is_empty() {
        return None;
    }
    let rendered = if status_panel_v2_enabled {
        discord::formatting::format_for_discord_with_status_panel(&stripped, provider)
    } else {
        discord::formatting::format_for_discord_with_provider(&stripped, provider)
    };
    if rendered.trim().is_empty() {
        None
    } else {
        Some(rendered)
    }
}

fn leak_recovery_chunk_fingerprints(chunks: &[String]) -> Vec<String> {
    chunks
        .iter()
        .enumerate()
        .map(|(index, chunk)| {
            let hash = blake3::hash(chunk.as_bytes());
            format!("{index}:{}", hash.to_hex())
        })
        .collect()
}

const LEAK_RECOVERY_CONTINUATION_SCAN_LIMIT: u8 = 100;

#[derive(Debug, Clone, PartialEq, Eq)]
struct LeakRecoveryLedgerIdentity {
    provider: String,
    channel_id: u64,
    current_msg_id: u64,
    user_msg_id: u64,
    byte_start: usize,
    byte_end: usize,
    chunk_fingerprints: Vec<String>,
}

impl LeakRecoveryLedgerIdentity {
    fn new(
        provider: &ProviderKind,
        state: &discord::inflight::InflightTurnState,
        start: usize,
        end: usize,
        chunks: &[String],
    ) -> Self {
        Self {
            provider: provider.as_str().to_string(),
            channel_id: state.channel_id,
            current_msg_id: state.current_msg_id,
            user_msg_id: state.user_msg_id,
            byte_start: start,
            byte_end: end,
            chunk_fingerprints: leak_recovery_chunk_fingerprints(chunks),
        }
    }
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
struct LeakRecoveryChunkLedger {
    version: u32,
    provider: String,
    channel_id: u64,
    current_msg_id: u64,
    user_msg_id: u64,
    byte_start: usize,
    byte_end: usize,
    chunk_fingerprints: Vec<String>,
    confirmed_chunks: Vec<LeakRecoveryConfirmedChunk>,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
struct LeakRecoveryConfirmedChunk {
    index: usize,
    message_id: u64,
}

fn leak_recovery_chunk_ledger_root() -> Option<PathBuf> {
    crate::config::runtime_root().map(|root| {
        root.join("runtime")
            .join("discord_leak_recovery_chunk_ledgers")
    })
}

fn leak_recovery_chunk_ledger_path(identity: &LeakRecoveryLedgerIdentity) -> Option<PathBuf> {
    leak_recovery_chunk_ledger_root().map(|root| {
        root.join(&identity.provider)
            .join(identity.channel_id.to_string())
            .join(format!("{}.json", identity.current_msg_id))
    })
}

/// #3031(A) — stable, byte-boundary-INDEPENDENT identity gate.
///
/// The durable ledger's idempotency key is the turn coordinate
/// (`provider/channel_id/current_msg_id/user_msg_id`) plus `byte_start`, NOT the
/// *whole* response extent. We deliberately drop `byte_end` and the full
/// `chunk_fingerprints` array from this gate: a late post-terminal assistant
/// continuation grows `full_response`, which moves `byte_end` and appends chunk
/// fingerprints. Gating on those would invalidate the entire ledger and reset the
/// confirmed prefix to 0 (`.unwrap_or(0)`), risking a re-send of already-delivered
/// chunks. Per-chunk fingerprint equality is enforced separately and only for the
/// chunks the ledger actually claims as confirmed, so the confirmed prefix stays
/// immutable (monotonic) as the response grows.
fn leak_recovery_ledger_stable_identity_matches(
    ledger: &LeakRecoveryChunkLedger,
    identity: &LeakRecoveryLedgerIdentity,
) -> bool {
    ledger.version == 1
        && ledger.provider == identity.provider
        && ledger.channel_id == identity.channel_id
        && ledger.current_msg_id == identity.current_msg_id
        && ledger.user_msg_id == identity.user_msg_id
        && ledger.byte_start == identity.byte_start
}

/// #3031(A) — count the confirmed chunk prefix that is still valid against the
/// (possibly grown) live identity. A confirmed chunk only counts while its
/// ledger fingerprint still equals the current identity's fingerprint at the same
/// index; the first divergence (or a chunk index now beyond the live chunk count)
/// stops the prefix. This makes a longer `full_response` unable to LOWER the
/// confirmed prefix — appended tail chunks leave the confirmed prefix untouched,
/// while an actually-rewritten confirmed chunk still fails closed at that index.
fn leak_recovery_confirmed_prefix_against_identity(
    ledger: &LeakRecoveryChunkLedger,
    identity: &LeakRecoveryLedgerIdentity,
) -> usize {
    let mut expected = 0usize;
    for confirmed in &ledger.confirmed_chunks {
        if confirmed.index != expected || confirmed.message_id == 0 {
            break;
        }
        // The chunk must still exist in the live response and carry the same
        // fingerprint we recorded when we confirmed delivery. A grown response
        // keeps these prefix fingerprints byte-identical, so growth never trims
        // the prefix; a content rewrite at this index does.
        match (
            ledger.chunk_fingerprints.get(expected),
            identity.chunk_fingerprints.get(expected),
        ) {
            (Some(recorded), Some(live)) if recorded == live => {}
            _ => break,
        }
        expected += 1;
    }
    expected.min(identity.chunk_fingerprints.len())
}

fn leak_recovery_confirmed_prefix_from_ledger(
    identity: &LeakRecoveryLedgerIdentity,
) -> Option<usize> {
    let path = leak_recovery_chunk_ledger_path(identity)?;
    let content = fs::read_to_string(path).ok()?;
    let ledger: LeakRecoveryChunkLedger = serde_json::from_str(&content).ok()?;
    if !leak_recovery_ledger_stable_identity_matches(&ledger, identity) {
        return None;
    }
    Some(leak_recovery_confirmed_prefix_against_identity(
        &ledger, identity,
    ))
}

fn leak_recovery_record_confirmed_chunk(
    identity: &LeakRecoveryLedgerIdentity,
    chunk_index: usize,
    message_id: u64,
) -> Result<(), String> {
    if chunk_index >= identity.chunk_fingerprints.len() || message_id == 0 {
        return Err(format!(
            "invalid confirmed chunk index={chunk_index} message_id={message_id}"
        ));
    }
    let Some(path) = leak_recovery_chunk_ledger_path(identity) else {
        return Err("runtime root unavailable for leak recovery chunk ledger".to_string());
    };
    let mut confirmed_chunks = Vec::new();
    if let Ok(content) = fs::read_to_string(&path)
        && let Ok(existing) = serde_json::from_str::<LeakRecoveryChunkLedger>(&content)
        && leak_recovery_ledger_stable_identity_matches(&existing, identity)
    {
        // #3031(A): carry forward only the confirmed prefix that is STILL valid
        // against the current identity. A grown response keeps prefix fingerprints
        // identical (prefix preserved); a rewritten confirmed chunk trims the prefix
        // at the divergence so we never claim a stale delivery.
        let valid_prefix = leak_recovery_confirmed_prefix_against_identity(&existing, identity);
        confirmed_chunks = existing.confirmed_chunks;
        confirmed_chunks.retain(|chunk| chunk.index < valid_prefix);
    }

    confirmed_chunks.retain(|chunk| chunk.index < chunk_index);
    if confirmed_chunks.len() != chunk_index
        || confirmed_chunks
            .iter()
            .enumerate()
            .any(|(index, chunk)| chunk.index != index || chunk.message_id == 0)
    {
        return Err(format!(
            "cannot record non-contiguous leak recovery chunk {chunk_index}"
        ));
    }
    confirmed_chunks.push(LeakRecoveryConfirmedChunk {
        index: chunk_index,
        message_id,
    });
    let ledger = LeakRecoveryChunkLedger {
        version: 1,
        provider: identity.provider.clone(),
        channel_id: identity.channel_id,
        current_msg_id: identity.current_msg_id,
        user_msg_id: identity.user_msg_id,
        byte_start: identity.byte_start,
        byte_end: identity.byte_end,
        chunk_fingerprints: identity.chunk_fingerprints.clone(),
        confirmed_chunks,
    };
    let json = serde_json::to_string_pretty(&ledger)
        .map_err(|error| format!("serialize leak recovery chunk ledger: {error}"))?;
    discord::runtime_store::atomic_write(&path, &json)
}

fn leak_recovery_clear_chunk_ledger(identity: &LeakRecoveryLedgerIdentity) -> Result<(), String> {
    let Some(path) = leak_recovery_chunk_ledger_path(identity) else {
        return Ok(());
    };
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "remove leak recovery chunk ledger {}: {error}",
            path.display()
        )),
    }
}

fn leak_recovery_confirmed_chunk_count<'a>(
    current_message_content: &str,
    continuation_contents: impl IntoIterator<Item = &'a str>,
    chunks: &[String],
) -> Option<usize> {
    let first_chunk = chunks.first()?;
    if current_message_content != first_chunk {
        return None;
    }

    let mut confirmed = 1usize;
    for content in continuation_contents {
        if confirmed >= chunks.len() {
            break;
        }
        if content == chunks[confirmed] {
            confirmed += 1;
        }
    }
    Some(confirmed)
}

async fn leak_recovery_fetch_continuation_contents(
    http: &Arc<serenity::Http>,
    channel_id: ChannelId,
    current_msg_id: MessageId,
    current_bot_user_id: u64,
) -> Option<Vec<(u64, String)>> {
    let messages = channel_id
        .messages(
            http.as_ref(),
            serenity::builder::GetMessages::new()
                .after(current_msg_id)
                .limit(LEAK_RECOVERY_CONTINUATION_SCAN_LIMIT),
        )
        .await
        .ok()?;

    Some(
        messages
            .into_iter()
            .filter(|msg| msg.author.id.get() == current_bot_user_id)
            .map(|msg| (msg.id.get(), msg.content))
            .collect(),
    )
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

    let http = match super::resolve_bot_http(registry, provider.as_str()).await {
        Ok(http) => http,
        Err(_) => return false,
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

        let continuation_messages = if chunks.len() > 1 && current_message.content == chunks[0] {
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
                if content != &chunks[chunk_index] {
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
    if let Some(mut fresh) = discord::inflight::load_inflight_state(provider, channel_id.get())
        && fresh.user_msg_id == state.user_msg_id
        && fresh.current_msg_id == state.current_msg_id
        && fresh.response_sent_offset < end
    {
        fresh.response_sent_offset = end;
        if let Err(error) = discord::inflight::save_inflight_state(&fresh) {
            tracing::warn!(
                "[leak-recover] delivered answer on channel {} but failed to persist offset: {error}",
                channel_id
            );
        }
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
    true
}

/// #1446 — pure-helper tests for the stall-watchdog decision logic.
/// Always-on (`#[cfg(test)]`) because the helper has no filesystem/runtime
/// dependencies; keeping them in a removed SQLite-only gate would prevent them
/// from running in normal `cargo test --bin agentdesk` invocations.

#[cfg(test)]
mod stall_watchdog_pure_tests {
    use super::super::stall_liveness::stall_watchdog_jsonl_liveness_defers_force_clean;
    use super::{
        LeakRecoveryLedgerIdentity, STALL_WATCHDOG_THRESHOLD_SECS,
        completed_stale_no_answer_orphan_should_clean, force_clean_should_preserve_resume_selector,
        inflight_completed_stale_leak_detected, leak_recovery_chunk_fingerprints,
        leak_recovery_clear_chunk_ledger, leak_recovery_confirmed_chunk_count,
        leak_recovery_confirmed_prefix_from_ledger, leak_recovery_record_confirmed_chunk,
        leak_recovery_unrelayed_range, preserve_cancel_should_skip_provider_interrupt_for_idle_tui,
        render_leak_recovery_delivery, stale_idle_foreground_queue_detected,
        stall_watchdog_should_force_clean,
        stall_watchdog_should_force_clean_orphan_explicit_background_work,
    };
    use crate::services::discord::relay_health::{RelayActiveTurn, RelayStallState};
    use crate::services::discord::{InflightRestartMode, TmuxCleanupPolicy};
    use crate::services::provider::ProviderKind;
    use chrono::TimeZone;
    use std::ffi::OsString;

    struct EnvVarReset {
        key: &'static str,
        previous: Option<OsString>,
    }

    impl EnvVarReset {
        fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
            let previous = std::env::var_os(key);
            unsafe { std::env::set_var(key, value) };
            Self { key, previous }
        }
    }

    impl Drop for EnvVarReset {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => unsafe { std::env::set_var(self.key, value) },
                None => unsafe { std::env::remove_var(self.key) },
            }
        }
    }

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
        let _guard = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let tempdir = tempfile::tempdir().expect("temp runtime root");
        let _env = EnvVarReset::set("AGENTDESK_ROOT_DIR", tempdir.path());
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
        let _guard = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let tempdir = tempfile::tempdir().expect("temp runtime root");
        let _env = EnvVarReset::set("AGENTDESK_ROOT_DIR", tempdir.path());
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
        let _guard = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let tempdir = tempfile::tempdir().expect("temp runtime root");
        let _env = EnvVarReset::set("AGENTDESK_ROOT_DIR", tempdir.path());
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
        let _guard = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let tempdir = tempfile::tempdir().expect("temp runtime root");
        let _env = EnvVarReset::set("AGENTDESK_ROOT_DIR", tempdir.path());

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

    fn local_string(unix: i64) -> String {
        chrono::Local
            .timestamp_opt(unix, 0)
            .single()
            .expect("valid local time")
            .format("%Y-%m-%d %H:%M:%S")
            .to_string()
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
            Some(stale_str.as_str()),
            now_unix,
            STALL_WATCHDOG_THRESHOLD_SECS,
            /* boot_unix_secs */ now_unix - (STALL_WATCHDOG_THRESHOLD_SECS as i64) - 1,
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
    use crate::services::provider::{CancelToken, ProviderKind};
    use poise::serenity_prelude::{ChannelId, MessageId, UserId};
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64};

    #[tokio::test]
    async fn stall_watchdog_cleanup_releases_residual_orphan_pending_token() {
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
