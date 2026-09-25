use crate::services::discord::health::snapshot::WatcherStateSnapshot;
use crate::services::discord::relay_health::{RelayActiveTurn, RelayStallState};
use crate::services::discord::relay_recovery::AxisBSite;
use crate::services::discord::relay_recovery::{self, RelayRecoveryActionKind};
use crate::services::discord::{self as discord};
use crate::services::provider::ProviderKind;

pub(crate) fn idle_tmux_repair_ready_for_input(
    provider: &ProviderKind,
    channel_id: u64,
    tmux_session: &str,
) -> bool {
    relay_recovery::idle_tmux_repair_ready_for_input(provider, channel_id, tmux_session)
}

/// #1446 stall-deadlock recovery: pure decision helper for the
/// `stall_watchdog` loop. `true` means the watchdog should force-clean a
/// watcher's state; the caller performs the actual cleanup so this stays
/// unit-testable without a live `SharedData`.
///
/// Force-clean only when `attached && desynced` (already classified
/// detached/diverged) AND `inflight_started_at` is older than
/// `threshold_secs` AND `terminal_delivery_committed == false` — either
/// staleness signal alone is insufficient (a fresh desynced watcher may be
/// mid-stream; a stale-but-synced one may be waiting on an idle agent).
///
/// #3041 B: whether to PRESERVE the force-cleaned turn's provider session
/// selector (persisted so the next turn `--resume`s it) or DISCARD it (cold
/// start). Preserve only with positive evidence the session is intact:
/// `terminal_delivery_committed` (answer delivered, idle-but-healthy) or
/// `tmux_session_alive == Some(true)` (pane still live, transcript coherent).
/// Otherwise the transcript may be truncated mid-write, so discard and let
/// the next turn cold-start clean.
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

/// #3126 false-positive guard: a turn that finished normally commits
/// `terminal_delivery_committed` and then idles (e.g. a `ScheduleWakeup`).
/// That idle row can read as `desynced` (#2965), which previously tripped
/// force-clean on a perfectly healthy wakeup-waiting session — excluding
/// committed turns keeps the watchdog targeting only genuine hangs.
///
/// #3041 post-restart grace: anchoring age at `max(started_at, boot)` means a
/// turn whose `started_at` predates a restart gets a full `threshold_secs`
/// window after boot to re-sync, instead of instantly satisfying the
/// threshold while every watcher is transiently `desynced` post-restart. A
/// genuinely hung turn stays desynced past that window and is still cleaned.
///
/// #3656: age is measured from `started_at`, not `updated_at`, so consecutive
/// short turns under one session key don't accumulate into a fake stall.
#[allow(clippy::too_many_arguments)]
pub(crate) fn stall_watchdog_should_force_clean(
    attached: bool,
    desynced: bool,
    capture_advancing: bool,
    inflight_terminal_delivery_committed: bool,
    inflight_started_at: Option<&str>,
    now_unix_secs: i64,
    threshold_secs: u64,
    boot_unix_secs: i64,
) -> bool {
    if !attached || !desynced {
        return false;
    }
    // #4178: advancing capture offset means the tmux turn is alive despite a stalled relay.
    if capture_advancing {
        return false;
    }
    if inflight_terminal_delivery_committed {
        return false;
    }
    let Some(started_at) = inflight_started_at else {
        return false;
    };
    let Some(started_at_unix) = discord::inflight::parse_updated_at_unix(started_at) else {
        return false;
    };
    // #3041: anchor age at boot, not a pre-restart `started_at`.
    let age_anchor = started_at_unix.max(boot_unix_secs);
    let age_secs = now_unix_secs.saturating_sub(age_anchor);
    age_secs >= 0 && (age_secs as u64) >= threshold_secs
}

/// Detection-only counterpart to `stall_watchdog_should_force_clean`: `true`
/// for the "completed-stale inflight on a healthy watcher" pattern the
/// deadlock-manager 30-min alarms flag. Requires `attached && !desynced`,
/// `inflight_state_present`, no active mailbox turn, `tmux_session_alive ==
/// Some(true)`, and `inflight_updated_at` older than `threshold_secs`.
///
/// Callers must NOT clean on this signal alone - the user may be about to
/// send the next message. Exists so the watchdog can emit telemetry without
/// altering recovery behaviour.
#[allow(clippy::too_many_arguments)]
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

/// #3629: clean-vs-preserve fork for a completed-stale inflight with no
/// unrelayed answer, reached only after
/// [`inflight_completed_stale_leak_detected`] already held. Sole
/// discriminator: `terminal_delivery_committed`.
///
/// - `true` — the answer was delivered and the session is merely idle (e.g. a
///   #3126 wakeup-waiting loop); PRESERVE, the user may still reply.
/// - `false` — nothing was ever delivered and nothing is left to deliver: an
///   empty/NO_REPLY terminal turn that never self-clears and that the
///   external deadlock monitor flags every ~30 min (#3629); CLEAN it.
///
/// The removal at the call site is identity-guarded against the on-disk
/// `user_msg_id`, so this predicate only decides intent, not identity.
///
/// `this_turn_user_msg_id == 0` is NEVER cleaned (codex #3629 review): a
/// zero-id row can't be distinguished from a live `RecoveryKickoff` turn
/// (which has no active mailbox anchor either) nor from a newer zero-id
/// turn. Only a real, non-zero id can be identity-guarded safely.
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

#[allow(clippy::too_many_arguments)]
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
    // Queue depth is ignored: a stale foreground anchor can strand health regardless.
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

#[allow(clippy::too_many_arguments)]
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

/// No tmux evidence off unix, so the warrant abstains and the structural predicate alone decides.
#[cfg(not(unix))]
pub(crate) fn watchdog_axis_b_warrants(
    provider: &ProviderKind,
    snapshot: &WatcherStateSnapshot,
    site: AxisBSite,
) -> bool {
    let _ = (provider, snapshot, site);
    true
}

/// Binds the two watchdog structural candidates to the axis-B warrant, only
/// preserving or lowering `structural_candidate_apply`.
#[cfg(unix)]
pub(crate) fn watchdog_axis_b_warrants(
    provider: &ProviderKind,
    snapshot: &WatcherStateSnapshot,
    site: AxisBSite,
) -> bool {
    let Some(action) = (match site {
        AxisBSite::WatchdogStaleIdle => Some(RelayRecoveryActionKind::ClearStaleThreadProof),
        AxisBSite::WatchdogExplicitBackground => {
            Some(RelayRecoveryActionKind::ClearOrphanPendingToken)
        }
        _ => None,
    }) else {
        return true;
    };
    let structural_candidate_apply = relay_recovery::structural_candidate_apply(true);
    let destructive_warrant_bind = relay_recovery::destructive_warrant_bind(
        structural_candidate_apply,
        action,
        provider,
        Some(snapshot),
        false,
    );
    destructive_warrant_bind.eligible
}

/// Converges inside ~1 cycle of the `2x` staleness window while staying well
/// below the gateway-lease keepalive cadence.
pub(crate) const STALL_WATCHDOG_INTERVAL_SECS: u64 = 30;

/// Mirrors `placeholder_sweeper::INITIAL_DELAY_SECS` so a freshly recovered
/// turn is never observed as "desynced" mid-bootstrap.
pub(crate) const STALL_WATCHDOG_INITIAL_DELAY_SECS: u64 = 90;

/// Strictly larger than THREAD-GUARD staleness so the watchdog can't race an in-flight intake call.
pub(crate) const STALL_WATCHDOG_THRESHOLD_SECS: u64 =
    2 * discord::inflight::INFLIGHT_STALENESS_THRESHOLD_SECS;

/// #3169: jsonl-mtime freshness window — events inside it prove mid-write, not a hung desync.
pub(crate) const STALL_WATCHDOG_LIVENESS_FRESHNESS_SECS: u64 = STALL_WATCHDOG_THRESHOLD_SECS;
