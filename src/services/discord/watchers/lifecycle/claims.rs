use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WatcherClaimAction {
    SpawnFresh,
    SpawnReplacedStale,
    SpawnReplacedDifferentSession,
    SpawnReplacedForced,
    ReuseExisting,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WatcherClaimOutcome {
    pub(super) action: WatcherClaimAction,
    owner_channel_id: ChannelId,
}

impl WatcherClaimOutcome {
    fn new(action: WatcherClaimAction, owner_channel_id: ChannelId) -> Self {
        Self {
            action,
            owner_channel_id,
        }
    }

    pub(crate) fn owner_channel_id(self) -> ChannelId {
        self.owner_channel_id
    }

    pub(crate) fn should_spawn(self) -> bool {
        matches!(
            self.action,
            WatcherClaimAction::SpawnFresh
                | WatcherClaimAction::SpawnReplacedStale
                | WatcherClaimAction::SpawnReplacedDifferentSession
                | WatcherClaimAction::SpawnReplacedForced
        )
    }

    pub(crate) fn replaced_existing(self) -> bool {
        matches!(
            self.action,
            WatcherClaimAction::SpawnReplacedStale
                | WatcherClaimAction::SpawnReplacedDifferentSession
                | WatcherClaimAction::SpawnReplacedForced
        )
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self.action {
            WatcherClaimAction::SpawnFresh => "spawn_fresh",
            WatcherClaimAction::SpawnReplacedStale => "spawn_replaced_stale",
            WatcherClaimAction::SpawnReplacedDifferentSession => "spawn_replaced_different_session",
            WatcherClaimAction::SpawnReplacedForced => "spawn_replaced_forced",
            WatcherClaimAction::ReuseExisting => "reuse_existing",
        }
    }
}

pub(super) fn find_watcher_by_tmux_session(
    watchers: &TmuxWatcherRegistry,
    tmux_session_name: &str,
) -> Option<(ChannelId, bool, bool, String)> {
    let owner = watchers.owner_channel_for_tmux_session(tmux_session_name)?;
    let entry = watchers.by_tmux_session.get(tmux_session_name)?;
    Some((
        owner,
        entry.heartbeat_stale() || entry.cancel.load(std::sync::atomic::Ordering::Relaxed),
        entry.paused.load(std::sync::atomic::Ordering::Relaxed),
        entry.output_path.clone(),
    ))
}

pub(super) fn restore_scan_should_skip_existing_watcher(
    cancelled: bool,
    paused: bool,
    existing_output_path: &str,
    restored_output_path: &str,
) -> bool {
    !cancelled && !paused && existing_output_path == restored_output_path
}

/// #226/#1170: Atomically claim a tmux session for watcher creation.
/// Returns true if the claim succeeded (caller should spawn the watcher).
/// Returns false if a watcher already exists (caller should skip).
pub(in crate::services::discord) fn try_claim_watcher(
    watchers: &TmuxWatcherRegistry,
    channel_id: ChannelId,
    handle: TmuxWatcherHandle,
) -> bool {
    let guard = lock_tmux_watcher_registry();
    let requested_tmux = handle.tmux_session_name.clone();
    let requested_output_path = handle.output_path.clone();
    if let Some(existing) = find_watcher_by_tmux_session(watchers, &requested_tmux) {
        if existing.1 || existing.2 || existing.3 != requested_output_path {
            if let Some((_, existing_handle)) =
                watchers.remove_tmux_session_locked(&guard, &requested_tmux)
            {
                existing_handle
                    .cancel
                    .store(true, std::sync::atomic::Ordering::Relaxed);
            }
        } else {
            record_watcher_invariant(
                true,
                None,
                channel_id,
                "watcher_one_per_tmux_session",
                "src/services/discord/tmux.rs:try_claim_watcher",
                "same tmux session must reuse the live watcher slot",
                serde_json::json!({
                    "existing_channel_id": existing.0.get(),
                    "tmux_session_name": requested_tmux,
                    "output_path": requested_output_path,
                    "watcher_slots": watchers.len(),
                }),
            );
            return false;
        }
    }
    let claimed = if watchers.contains_key(&channel_id) {
        false
    } else {
        watchers.insert_locked(&guard, channel_id, handle);
        true
    };
    let slot_present = watchers.contains_key(&channel_id);
    record_watcher_invariant(
        slot_present,
        None,
        channel_id,
        "watcher_one_per_channel",
        "src/services/discord/tmux.rs:try_claim_watcher",
        "watcher claim must leave a single channel-owned watcher slot",
        serde_json::json!({
            "claimed": claimed,
            "watcher_slots": watchers.len(),
        }),
    );
    debug_assert!(
        slot_present,
        "watcher claim must leave a channel-owned watcher slot"
    );
    claimed
}

/// Claim a channel for watcher creation with the #1135 single-watcher policy.
///
/// Same tmux session:
/// - live incumbent: reuse it and do not spawn another watcher;
/// - cancelled incumbent: remove it and spawn the requested watcher.
///
/// Same channel but a different tmux session still replaces the incumbent. That
/// preserves the existing new-turn recovery behavior without allowing two
/// owners for one tmux session.
pub(in crate::services::discord) fn claim_or_reuse_watcher(
    watchers: &TmuxWatcherRegistry,
    channel_id: ChannelId,
    handle: TmuxWatcherHandle,
    provider: &ProviderKind,
    source: &str,
) -> WatcherClaimOutcome {
    claim_watcher(watchers, channel_id, handle, provider, source, false)
}

/// Force a fresh watcher/converter generation even when a live same-session
/// incumbent watches the same output path. Recovery uses this only after it
/// proves that the persisted Codex render seed belongs to an earlier provider
/// turn: reusing that incumbent would keep the stale Discord anchor alive.
pub(in crate::services::discord) fn claim_or_replace_watcher(
    watchers: &TmuxWatcherRegistry,
    channel_id: ChannelId,
    handle: TmuxWatcherHandle,
    provider: &ProviderKind,
    source: &str,
) -> WatcherClaimOutcome {
    claim_watcher(watchers, channel_id, handle, provider, source, true)
}

pub(super) fn claim_watcher(
    watchers: &TmuxWatcherRegistry,
    channel_id: ChannelId,
    handle: TmuxWatcherHandle,
    provider: &ProviderKind,
    source: &str,
    force_replace_live_same_tmux: bool,
) -> WatcherClaimOutcome {
    let guard = lock_tmux_watcher_registry();
    let requested_tmux = handle.tmux_session_name.clone();
    let requested_output_path = handle.output_path.clone();
    let mut removed_stale_same_tmux = false;

    if let Some((existing_channel_id, existing_cancelled, existing_paused, existing_output_path)) =
        find_watcher_by_tmux_session(watchers, &requested_tmux)
    {
        let replace_paused_incumbent =
            existing_paused && !matches!(source, "turn_start_message" | "turn_start_headless");
        if force_replace_live_same_tmux
            || existing_cancelled
            || replace_paused_incumbent
            || existing_output_path != requested_output_path
        {
            if let Some((_, existing_handle)) =
                watchers.remove_tmux_session_locked(&guard, &requested_tmux)
            {
                existing_handle
                    .cancel
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                // #3277 (Defect B): this cancel+remove was completely silent —
                // in the incident the replaced incumbent's later "stopped" log
                // was misattributed to the replacement watcher. Log the claim.
                tracing::info!(
                    source,
                    tmux_session = %requested_tmux,
                    existing_channel = existing_channel_id.get(),
                    existing_cancelled,
                    force_replace_live_same_tmux,
                    replace_paused_incumbent,
                    output_path_changed = existing_output_path != requested_output_path,
                    "watcher claim cancelled same-tmux incumbent before spawning replacement"
                );
            }
            removed_stale_same_tmux = true;
        } else {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ⏭ watcher reuse for channel {} — tmux {} is already watched by channel {}",
                channel_id,
                requested_tmux,
                existing_channel_id
            );
            record_watcher_invariant(
                true,
                Some(provider),
                channel_id,
                "watcher_one_per_tmux_session",
                "src/services/discord/tmux.rs:claim_or_reuse_watcher",
                "same tmux session must reuse the live watcher slot",
                serde_json::json!({
                    "source": source,
                    "existing_channel_id": existing_channel_id.get(),
                    "tmux_session_name": requested_tmux,
                    "output_path": requested_output_path,
                    "watcher_slots": watchers.len(),
                }),
            );
            return WatcherClaimOutcome::new(
                WatcherClaimAction::ReuseExisting,
                existing_channel_id,
            );
        }
    }

    let outcome = if let Some(entry) = watchers.get(&channel_id) {
        let previous_tmux = entry.tmux_session_name.clone();
        let same_tmux = previous_tmux == requested_tmux;
        entry
            .cancel
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let stale_cancelled = entry.cancel.load(std::sync::atomic::Ordering::Relaxed);
        record_watcher_invariant(
            stale_cancelled,
            Some(provider),
            channel_id,
            "watcher_replacement_cancels_stale",
            "src/services/discord/tmux.rs:claim_or_reuse_watcher",
            "replacing a watcher must cancel the stale watcher before installing the new handle",
            serde_json::json!({
                "source": source,
                "same_tmux": same_tmux,
                "previous_tmux_session_name": previous_tmux,
                "tmux_session_name": requested_tmux.as_str(),
            }),
        );
        debug_assert!(
            stale_cancelled,
            "stale watcher must be cancelled before replacement"
        );
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] ♻ watcher replaced for channel {} — cancelled stale watcher",
            channel_id
        );
        drop(entry);
        watchers.insert_locked(&guard, channel_id, handle);
        crate::services::observability::emit_watcher_replaced(
            provider.as_str(),
            channel_id.get(),
            source,
        );
        if force_replace_live_same_tmux && same_tmux {
            WatcherClaimOutcome::new(WatcherClaimAction::SpawnReplacedForced, channel_id)
        } else if same_tmux {
            WatcherClaimOutcome::new(WatcherClaimAction::SpawnReplacedStale, channel_id)
        } else {
            WatcherClaimOutcome::new(
                WatcherClaimAction::SpawnReplacedDifferentSession,
                channel_id,
            )
        }
    } else {
        watchers.insert_locked(&guard, channel_id, handle);
        if force_replace_live_same_tmux && removed_stale_same_tmux {
            WatcherClaimOutcome::new(WatcherClaimAction::SpawnReplacedForced, channel_id)
        } else if removed_stale_same_tmux {
            WatcherClaimOutcome::new(WatcherClaimAction::SpawnReplacedStale, channel_id)
        } else {
            WatcherClaimOutcome::new(WatcherClaimAction::SpawnFresh, channel_id)
        }
    };
    let slot_present = watchers.contains_key(&channel_id);
    record_watcher_invariant(
        slot_present,
        Some(provider),
        channel_id,
        "watcher_one_per_channel",
        "src/services/discord/tmux.rs:claim_or_reuse_watcher",
        "watcher replacement must leave exactly one channel-owned watcher slot",
        serde_json::json!({
            "outcome": outcome.as_str(),
            "source": source,
            "watcher_slots": watchers.len(),
        }),
    );
    debug_assert!(
        slot_present,
        "watcher replacement must leave a channel-owned watcher slot"
    );
    outcome
}
