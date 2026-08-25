use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WatcherClaimAction {
    SpawnFresh,
    SpawnReplacedStale,
    SpawnReplacedDifferentSession,
    SpawnReplacedForced,
    ReuseExisting,
}

#[derive(Debug, Clone)]
pub(in crate::services::discord) struct WatcherClaimIncarnation {
    owner_channel_id: ChannelId,
    cancel: Arc<std::sync::atomic::AtomicBool>,
    pub(in crate::services::discord) paused: Arc<std::sync::atomic::AtomicBool>,
    pub(in crate::services::discord) resume_offset: Arc<std::sync::Mutex<Option<u64>>>,
    pub(in crate::services::discord) turn_delivered: Arc<std::sync::atomic::AtomicBool>,
}

impl WatcherClaimIncarnation {
    fn from_handle(owner_channel_id: ChannelId, handle: &TmuxWatcherHandle) -> Self {
        Self {
            owner_channel_id,
            cancel: Arc::clone(&handle.cancel),
            paused: Arc::clone(&handle.paused),
            resume_offset: Arc::clone(&handle.resume_offset),
            turn_delivered: Arc::clone(&handle.turn_delivered),
        }
    }

    #[rustfmt::skip]
    pub(in crate::services::discord) fn adopt_if_current<T>(
        &self,
        watchers: &TmuxWatcherRegistry,
        adopt: impl FnOnce(&Self) -> T,
    ) -> Option<T> {
        let guard = lock_tmux_watcher_registry();
        #[cfg(test)]
        if EVICT_CLAIM_BEFORE_ADOPTION.compare_exchange(
            self.owner_channel_id.get(), 0,
            std::sync::atomic::Ordering::SeqCst, std::sync::atomic::Ordering::SeqCst,
        ).is_ok() {
            let _ = watchers.remove_locked(&guard, &self.owner_channel_id);
        }
        let current = watchers.get(&self.owner_channel_id)?;
        if !Arc::ptr_eq(&current.cancel, &self.cancel)
            || current.cancel.load(std::sync::atomic::Ordering::Relaxed)
        {
            return None;
        }
        drop(current);
        Some(adopt(self))
    }
}

#[derive(Debug, Clone)]
pub(crate) struct WatcherClaimOutcome {
    pub(crate) action: WatcherClaimAction,
    owner_channel_id: ChannelId,
    incarnation: WatcherClaimIncarnation,
}

impl WatcherClaimOutcome {
    fn new(
        action: WatcherClaimAction,
        owner_channel_id: ChannelId,
        incarnation: WatcherClaimIncarnation,
    ) -> Self {
        Self {
            action,
            owner_channel_id,
            incarnation,
        }
    }

    pub(crate) fn owner_channel_id(&self) -> ChannelId {
        self.owner_channel_id
    }

    pub(in crate::services::discord) fn incarnation(&self) -> &WatcherClaimIncarnation {
        &self.incarnation
    }

    pub(crate) fn should_spawn(&self) -> bool {
        matches!(
            self.action,
            WatcherClaimAction::SpawnFresh
                | WatcherClaimAction::SpawnReplacedStale
                | WatcherClaimAction::SpawnReplacedDifferentSession
                | WatcherClaimAction::SpawnReplacedForced
        )
    }

    pub(crate) fn replaced_existing(&self) -> bool {
        matches!(
            self.action,
            WatcherClaimAction::SpawnReplacedStale
                | WatcherClaimAction::SpawnReplacedDifferentSession
                | WatcherClaimAction::SpawnReplacedForced
        )
    }

    pub(crate) fn as_str(&self) -> &'static str {
        match self.action {
            WatcherClaimAction::SpawnFresh => "spawn_fresh",
            WatcherClaimAction::SpawnReplacedStale => "spawn_replaced_stale",
            WatcherClaimAction::SpawnReplacedDifferentSession => "spawn_replaced_different_session",
            WatcherClaimAction::SpawnReplacedForced => "spawn_replaced_forced",
            WatcherClaimAction::ReuseExisting => "reuse_existing",
        }
    }
}

#[cfg(test)]
#[rustfmt::skip]
static EVICT_CLAIM_BEFORE_ADOPTION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cfg(test)]
pub(in crate::services::discord) struct ClaimAdoptionEvictionGuard(u64);

#[cfg(test)]
#[rustfmt::skip]
impl Drop for ClaimAdoptionEvictionGuard {
    fn drop(&mut self) {
        let _ = EVICT_CLAIM_BEFORE_ADOPTION.compare_exchange(
            self.0, 0, std::sync::atomic::Ordering::SeqCst, std::sync::atomic::Ordering::SeqCst,
        );
    }
}

#[cfg(test)]
#[rustfmt::skip]
pub(in crate::services::discord) fn evict_claim_before_adoption_for_test(owner: ChannelId) -> ClaimAdoptionEvictionGuard {
    EVICT_CLAIM_BEFORE_ADOPTION.store(owner.get(), std::sync::atomic::Ordering::SeqCst);
    ClaimAdoptionEvictionGuard(owner.get())
}

pub(crate) fn find_watcher_by_tmux_session(
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ThreadFollowUpParent {
    channel_id: ChannelId,
    provenance: &'static str,
}

impl ThreadFollowUpParent {
    fn persisted(channel_id: ChannelId) -> Self {
        Self {
            channel_id,
            provenance: "persisted_inflight",
        }
    }

    fn live_discord(channel_id: ChannelId) -> Self {
        Self {
            channel_id,
            provenance: "live_discord",
        }
    }
}

pub(crate) fn thread_follow_up_parent_from_live(
    thread_parent_channel_id: Option<ChannelId>,
) -> Option<ThreadFollowUpParent> {
    thread_parent_channel_id.map(ThreadFollowUpParent::live_discord)
}

pub(crate) fn thread_follow_up_parent_channel_id(
    channel_id: ChannelId,
    logical_channel_id: Option<u64>,
    thread_id: Option<u64>,
) -> Option<ThreadFollowUpParent> {
    (thread_id == Some(channel_id.get()))
        .then_some(logical_channel_id)
        .flatten()
        .filter(|parent_channel_id| *parent_channel_id != channel_id.get())
        .map(ChannelId::new)
        .map(ThreadFollowUpParent::persisted)
}

fn observe_cross_channel_tmux_claim(
    provider: Option<&ProviderKind>,
    requested_channel_id: ChannelId,
    existing_channel_id: ChannelId,
    tmux_session_name: &str,
    source: &str,
    thread_parent: Option<ThreadFollowUpParent>,
) {
    let intended_thread_follow_up =
        thread_parent.is_some_and(|thread_parent| thread_parent.channel_id == existing_channel_id);
    let (claim_classification, intention_basis) = if intended_thread_follow_up {
        (
            "intended_thread_follow_up",
            "requesting thread parent matches the existing watcher owner",
        )
    } else {
        (
            "unintended_cross_channel_claim",
            "existing owner does not match the requesting thread parent",
        )
    };

    let _ = crate::services::observability::record_invariant_check_with_severity(
        false,
        crate::services::observability::InvariantViolation {
            provider: provider.map(ProviderKind::as_str),
            channel_id: Some(requested_channel_id.get()),
            dispatch_id: None,
            session_key: None,
            turn_id: None,
            invariant: "watcher_cross_channel_tmux_claim_observed",
            code_location: "src/services/discord/watchers/lifecycle/claims.rs",
            message: "cross-channel tmux watcher claim classification recorded",
            details: serde_json::json!({
                "source": source,
                "requested_channel_id": requested_channel_id.get(),
                "existing_channel_id": existing_channel_id.get(),
                "tmux_session_name": tmux_session_name,
                "claim_classification": claim_classification,
                "intention_basis": intention_basis,
                "thread_parent_channel_id": thread_parent.map(|parent| parent.channel_id.get()),
                "thread_parent_provenance": thread_parent
                    .map(|parent| parent.provenance)
                    .unwrap_or("none"),
            }),
        },
        crate::services::observability::InvariantSeverity::Warn,
    );
}

pub(crate) fn restore_scan_should_skip_existing_watcher(
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
    try_claim_watcher_with_thread_parent(watchers, channel_id, handle, None, None)
}

pub(in crate::services::discord) fn try_claim_watcher_with_thread_parent(
    watchers: &TmuxWatcherRegistry,
    channel_id: ChannelId,
    handle: TmuxWatcherHandle,
    provider: Option<&ProviderKind>,
    thread_parent: Option<ThreadFollowUpParent>,
) -> bool {
    let guard = lock_tmux_watcher_registry();
    let requested_tmux = handle.tmux_session_name.clone();
    let requested_output_path = handle.output_path.clone();
    if let Some(existing) = find_watcher_by_tmux_session(watchers, &requested_tmux) {
        if channel_id != existing.0 {
            observe_cross_channel_tmux_claim(
                provider,
                channel_id,
                existing.0,
                &requested_tmux,
                "try_claim_watcher",
                thread_parent,
            );
        }
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
    claim_or_reuse_watcher_with_thread_parent(watchers, channel_id, handle, provider, source, None)
}

pub(in crate::services::discord) fn claim_or_reuse_watcher_with_thread_parent(
    watchers: &TmuxWatcherRegistry,
    channel_id: ChannelId,
    handle: TmuxWatcherHandle,
    provider: &ProviderKind,
    source: &str,
    thread_parent: Option<ThreadFollowUpParent>,
) -> WatcherClaimOutcome {
    claim_watcher(
        watchers,
        channel_id,
        handle,
        provider,
        source,
        false,
        thread_parent,
    )
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
    claim_or_replace_watcher_with_thread_parent(
        watchers, channel_id, handle, provider, source, None,
    )
}

pub(in crate::services::discord) fn claim_or_replace_watcher_with_thread_parent(
    watchers: &TmuxWatcherRegistry,
    channel_id: ChannelId,
    handle: TmuxWatcherHandle,
    provider: &ProviderKind,
    source: &str,
    thread_parent: Option<ThreadFollowUpParent>,
) -> WatcherClaimOutcome {
    claim_watcher(
        watchers,
        channel_id,
        handle,
        provider,
        source,
        true,
        thread_parent,
    )
}

pub(crate) fn claim_watcher(
    watchers: &TmuxWatcherRegistry,
    channel_id: ChannelId,
    handle: TmuxWatcherHandle,
    provider: &ProviderKind,
    source: &str,
    force_replace_live_same_tmux: bool,
    thread_parent: Option<ThreadFollowUpParent>,
) -> WatcherClaimOutcome {
    let guard = lock_tmux_watcher_registry();
    let requested_tmux = handle.tmux_session_name.clone();
    let requested_output_path = handle.output_path.clone();
    let mut removed_stale_same_tmux = false;

    if let Some((existing_channel_id, existing_cancelled, existing_paused, existing_output_path)) =
        find_watcher_by_tmux_session(watchers, &requested_tmux)
    {
        let turn_start_uses_provisional_output_path =
            matches!(source, "turn_start_message" | "turn_start_headless");
        let output_path_changed = existing_output_path != requested_output_path;
        let replace_paused_incumbent = existing_paused && !turn_start_uses_provisional_output_path;
        // Turn admission resolves the canonical pre-handoff wrapper path. Once a
        // healthy watcher has adopted the provider-native runtime transcript,
        // that provisional path must not downgrade the live registry binding.
        let replace_for_output_path =
            output_path_changed && !turn_start_uses_provisional_output_path;
        let replaces_existing = force_replace_live_same_tmux
            || existing_cancelled
            || replace_paused_incumbent
            || replace_for_output_path;
        if channel_id != existing_channel_id {
            observe_cross_channel_tmux_claim(
                Some(provider),
                channel_id,
                existing_channel_id,
                &requested_tmux,
                source,
                thread_parent,
            );
        }
        if replaces_existing {
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
                    output_path_changed,
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
            let incarnation = watchers
                .by_tmux_session
                .get(&requested_tmux)
                .map(|entry| WatcherClaimIncarnation::from_handle(existing_channel_id, &entry))
                .expect("registry lock keeps the selected incumbent installed");
            return WatcherClaimOutcome::new(
                WatcherClaimAction::ReuseExisting,
                existing_channel_id,
                incarnation,
            );
        }
    }

    // The claim result owns the exact Arc identity installed by this mutation.
    // Consumers may safely use it after the registry lock is released without
    // re-resolving the channel to a replacement watcher incarnation.
    let installed_incarnation = WatcherClaimIncarnation::from_handle(channel_id, &handle);
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
            WatcherClaimOutcome::new(
                WatcherClaimAction::SpawnReplacedForced,
                channel_id,
                installed_incarnation.clone(),
            )
        } else if same_tmux {
            WatcherClaimOutcome::new(
                WatcherClaimAction::SpawnReplacedStale,
                channel_id,
                installed_incarnation.clone(),
            )
        } else {
            WatcherClaimOutcome::new(
                WatcherClaimAction::SpawnReplacedDifferentSession,
                channel_id,
                installed_incarnation.clone(),
            )
        }
    } else {
        watchers.insert_locked(&guard, channel_id, handle);
        if force_replace_live_same_tmux && removed_stale_same_tmux {
            WatcherClaimOutcome::new(
                WatcherClaimAction::SpawnReplacedForced,
                channel_id,
                installed_incarnation.clone(),
            )
        } else if removed_stale_same_tmux {
            WatcherClaimOutcome::new(
                WatcherClaimAction::SpawnReplacedStale,
                channel_id,
                installed_incarnation.clone(),
            )
        } else {
            WatcherClaimOutcome::new(
                WatcherClaimAction::SpawnFresh,
                channel_id,
                installed_incarnation,
            )
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

#[cfg(test)]
pub(crate) fn claim_cross_channel_tmux_watcher_for_test(
    requested_channel_id: ChannelId,
    existing_channel_id: ChannelId,
    thread_parent_channel_id: Option<ChannelId>,
) {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64};

    fn handle(tmux_session_name: &str) -> TmuxWatcherHandle {
        TmuxWatcherHandle {
            tmux_session_name: tmux_session_name.to_string(),
            output_path: "/tmp/agentdesk-4984-cross-channel-claim.jsonl".to_string(),
            paused: Arc::new(AtomicBool::new(false)),
            resume_offset: Arc::new(std::sync::Mutex::new(None)),
            cancel: Arc::new(AtomicBool::new(false)),
            pause_epoch: Arc::new(AtomicU64::new(0)),
            turn_delivered: Arc::new(AtomicBool::new(false)),
            last_heartbeat_ts_ms: Arc::new(AtomicI64::new(
                crate::services::discord::tmux_watcher_now_ms(),
            )),
        }
    }

    let watchers = TmuxWatcherRegistry::new();
    let tmux_session_name = "AgentDesk-claude-4984-cross-channel-claim";
    assert!(try_claim_watcher(
        &watchers,
        existing_channel_id,
        handle(tmux_session_name),
    ));
    let outcome = claim_watcher(
        &watchers,
        requested_channel_id,
        handle(tmux_session_name),
        &ProviderKind::Claude,
        "high_risk_recovery_4984",
        false,
        thread_follow_up_parent_from_live(thread_parent_channel_id),
    );
    assert_eq!(outcome.action, WatcherClaimAction::ReuseExisting);
    assert_eq!(outcome.owner_channel_id(), existing_channel_id);
}
