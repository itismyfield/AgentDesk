//! Discord runtime intervals, counters, inflight probes and poise user data shared by the bot.

#[cfg(test)]
use super::fs;
use super::{
    Arc, AtomicUsize, ChannelId, Duration, MessageId, Ordering, ProviderKind, SharedData, inflight,
    runtime_store, single_message_panel,
};

pub(super) const UPLOAD_CLEANUP_INTERVAL: Duration = Duration::from_secs(60 * 60);

pub(super) const UPLOAD_MAX_AGE: Duration = Duration::from_secs(3 * 24 * 60 * 60);

pub(super) const SESSION_CLEANUP_INTERVAL: Duration = Duration::from_secs(60 * 60); // 1 hour

// #1085 (908-3): extended from 1h → 4h. Working agents idle between dispatch
// turns and the prior 60-min cap forced the next user/dispatch turn to start a
// fresh provider session, defeating cache reuse. 4h covers typical "go for
// lunch / sync meeting" gaps while still bounding zombie growth via the
// cleanup interval reaper at `mod.rs:2093`.
pub(super) const SESSION_MAX_IDLE: Duration = Duration::from_secs(4 * 60 * 60); // 4 hours

pub(super) const DEAD_SESSION_REAP_INTERVAL: Duration = Duration::from_secs(60); // 1 minute

pub(super) const RESTART_REPORT_FLUSH_INTERVAL: Duration = Duration::from_secs(1);

pub(super) const DEFERRED_RESTART_POLL_INTERVAL: Duration = Duration::from_secs(10);

pub(in crate::services::discord) fn advance_last_message_checkpoint(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    message_id: MessageId,
) -> u64 {
    let message_id = message_id.get();
    let checkpoint = *shared
        .last_message_ids
        .entry(channel_id)
        .and_modify(|current| *current = (*current).max(message_id))
        .or_insert(message_id);
    runtime_store::save_last_message_id(provider.as_str(), channel_id.get(), checkpoint);
    checkpoint
}

pub(in crate::services) fn single_message_panel_enabled() -> bool {
    single_message_panel::enabled()
}

/// Parse `var` as a `u64` seconds `Duration`, falling back to `default_secs`.
pub(super) fn env_duration_secs(var: &str, default_secs: u64) -> Duration {
    let secs = (std::env::var(var).ok()).and_then(|s| s.parse::<u64>().ok());
    Duration::from_secs(secs.unwrap_or(default_secs))
}

/// Minimum interval between Discord placeholder progress edits (AGENTDESK_STATUS_INTERVAL_SECS, default 5s).
pub(in crate::services) fn status_update_interval() -> Duration {
    static CACHED: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| env_duration_secs("AGENTDESK_STATUS_INTERVAL_SECS", 5))
}

pub(crate) fn clear_inflight_state_for_channel(provider: &ProviderKind, channel_id: u64) -> bool {
    inflight::clear_inflight_state(provider, channel_id)
}

pub(crate) fn inflight_state_allows_idle_tmux_repair_for_channel(
    provider: &ProviderKind,
    channel_id: u64,
) -> Option<bool> {
    inflight::inflight_state_allows_idle_tmux_repair(provider, channel_id)
}

/// Reads only `channel_id`'s rows and writes nothing, so probing one thread never saves another
/// thread's row; a row that cannot be read or parsed counts as fresh.
pub(crate) fn has_fresh_inflight_for_channel(channel_id: u64) -> bool {
    let now_unix_secs = chrono::Local::now().timestamp();
    [
        ProviderKind::Claude,
        ProviderKind::Codex,
        ProviderKind::OpenCode,
        ProviderKind::Qwen,
    ]
    .iter()
    .any(
        |provider| match inflight::load_channel_inflight_for_probe(provider, channel_id) {
            Err(_) => true,
            Ok(None) => false,
            Ok(Some(state)) => {
                !state.rebind_origin
                    && state.channel_id == channel_id
                    && !inflight::inflight_state_is_stale(
                        &state,
                        now_unix_secs,
                        inflight::INFLIGHT_STALENESS_THRESHOLD_SECS,
                    )
            }
        },
    )
}

/// Consume a legacy deferred-restart signal.
///
/// #2713 changed restart semantics to quick-exit + rehydrate: provider TUI/tmux
/// sessions survive process restart, so this helper no longer waits for
/// `global_active` / `global_finalizing` to drain. Callers must persist cheap
/// queue/checkpoint state before invoking it.
#[cfg(test)]
pub(in crate::services) fn check_deferred_restart(shared: &SharedData) {
    if !shared
        .restart
        .restart_pending
        .load(std::sync::atomic::Ordering::Relaxed)
    {
        return;
    }
    // CAS: ensure this provider only decrements once
    if shared
        .restart
        .shutdown_counted
        .compare_exchange(
            false,
            true,
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Relaxed,
        )
        .is_err()
    {
        return;
    }
    if shared
        .restart
        .shutdown_remaining
        .fetch_sub(1, std::sync::atomic::Ordering::AcqRel)
        != 1
    {
        return;
    }
    let version = crate::config::runtime_root()
        .map(|root| root.join("restart_pending"))
        .and_then(|marker| {
            let version = fs::read_to_string(&marker).unwrap_or_default();
            let _ = fs::remove_file(&marker);
            Some(version)
        })
        .unwrap_or_default();
    let version = version.trim();
    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!(target: "agentdesk::services::discord", "  [{ts}] 🔄 Deferred restart quick-exit requested for v{version}");
    std::process::exit(0);
}

pub(in crate::services::discord) fn saturating_decrement_counter(counter: &AtomicUsize) -> bool {
    counter
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            current.checked_sub(1)
        })
        .is_ok()
}

/// Decrement `global_active` without allowing a stale/restored cleanup path
/// to wrap the counter from 0 to `usize::MAX`.
pub(in crate::services::discord) fn saturating_decrement_global_active(
    shared: &SharedData,
) -> bool {
    super::saturating_decrement_counter(shared.restart.global_active.as_ref())
}

/// Single authoritative writer for the INCREMENT side of `global_active`,
/// mirroring [`saturating_decrement_global_active`] on the decrement side
/// (#3019, sub-issue of #3016).
///
/// INVARIANT: `global_active` == number of mailbox slots currently in the
/// started-not-yet-finished state. This helper MUST be called +1 IFF a mailbox
/// `try_start_turn` / `recovery_kickoff` actually activated a slot
/// (`started` / `activated_turn()`); the matching -1 happens IFF a
/// mailbox finish/clear actually removed it (`removed_token.is_some()`). Keeping
/// increment/decrement 1:1 with the real mailbox state transition — NEVER caller
/// intent — is what prevents the drift/underflow seen in #2934.
///
/// Callers are responsible for the mailbox-activation gate; this helper does NOT
/// change WHEN the counter moves, only funnels HOW it moves so increment is
/// single-authority/single-helper exactly like decrement. `reason` is recorded
/// for observability so every increment is attributable to its activation site.
pub(in crate::services::discord) fn increment_global_active(
    shared: &SharedData,
    reason: &str,
) -> usize {
    increment_counter(shared.restart.global_active.as_ref(), reason)
}

pub(super) fn increment_counter(counter: &AtomicUsize, reason: &str) -> usize {
    let next = counter.fetch_add(1, Ordering::Relaxed) + 1;
    tracing::debug!(
        target: "agentdesk::global_active",
        reason,
        global_active = next,
        "global_active increment"
    );
    next
}

/// Poise user data type
pub(in crate::services) struct Data {
    pub(in crate::services) shared: Arc<SharedData>,
    pub(in crate::services) token: String,
    pub(in crate::services) provider: ProviderKind,
}

pub(in crate::services) fn mark_reconcile_complete(shared: &SharedData) {
    let duration_ms = shared.restart.recovery_started_at.elapsed().as_millis();
    let duration_ms = duration_ms.min(u64::MAX as u128) as u64;
    let _ = shared.restart.recovery_duration_ms.compare_exchange(
        0,
        duration_ms,
        std::sync::atomic::Ordering::AcqRel,
        std::sync::atomic::Ordering::Relaxed,
    );
    shared
        .restart
        .reconcile_done
        .store(true, std::sync::atomic::Ordering::Release);
}

/// `mark_reconcile_complete`, for tests outside `services`.
#[cfg(all(test, target_os = "linux"))]
pub(crate) fn mark_reconcile_complete_for_test(shared: &SharedData) {
    mark_reconcile_complete(shared);
}

pub(in crate::services) type Error = Box<dyn std::error::Error + Send + Sync>;

pub(in crate::services) type Context<'a> = poise::Context<'a, Data, Error>;

#[derive(Clone, Copy, Debug, Default)]
pub(in crate::services) struct IdleQueueKickoffChannelOutcome {
    pub(in crate::services) started: bool,
}
