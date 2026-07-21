use crate::services::discord::relay_health::{RelayActiveTurn, RelayStallState};
use crate::services::discord::{self as discord};

/// #1446 stall-deadlock recovery - pure decision helper for the
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
/// Either staleness signal alone is insufficient - a fresh desynced watcher
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
/// committed - the genuine hang / abnormal-exit signature where the transcript
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

/// #3126 false-positive guard: a turn that finished normally commits its
/// terminal response to the outbound delivery path
/// (`InflightTurnState::terminal_delivery_committed`) and then leaves the
/// session idle - e.g. the agent scheduled a `ScheduleWakeup` or the loop
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
    // #4178: a relay can be stalled while the underlying tmux turn is still
    // alive. Advancing capture offset is the discriminator; never force-clean
    // inflight while capture bytes are still moving across watchdog ticks.
    if capture_advancing {
        return false;
    }
    // #3126: a normally-completed turn that is now idle (wakeup/loop
    // wind-down) is not a hang - never force-clean it.
    if inflight_terminal_delivery_committed {
        return false;
    }
    let Some(started_at) = inflight_started_at else {
        return false;
    };
    let Some(started_at_unix) = discord::inflight::parse_updated_at_unix(started_at) else {
        return false;
    };
    // #3041: never count staleness that accrued before this process booted -
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
/// Callers must NOT clean on this signal alone - the user may be reading the
/// delivered response and about to send the next message. The helper exists
/// so the watchdog can emit telemetry without altering recovery behaviour.
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

/// #3629: clean-vs-preserve fork for a completed-stale inflight that has NO
/// unrelayed answer. Reached only after [`inflight_completed_stale_leak_detected`]
/// already held - i.e. the relay is healthy, the mailbox has NO active turn,
/// the tmux session is alive, and the row is stale. The sole remaining
/// discriminator is whether the turn ever committed a terminal delivery:
///
/// - `terminal_delivery_committed == true` -> the answer WAS delivered and the
///   session is merely idle now (e.g. a #3126 wakeup-waiting loop, or a turn
///   whose answer the watcher relayed). PRESERVE - the user may still send the
///   next message and the delivered response must not be disturbed.
/// - `terminal_delivery_committed == false` -> nothing was ever delivered AND
///   there is nothing left to deliver (no unrelayed answer): a NO_REPLY / empty
///   terminal turn. The bridge left an inflight row that no answer will ever
///   fill and that no live turn owns, so it never self-clears and the external
///   deadlock monitor flags it every ~30 min forever (#3629). CLEAN it.
///
/// The removal at the call site is identity-guarded against the on-disk
/// `user_msg_id`, so a newer turn's row is never clobbered - this predicate only
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

/// Watchdog tick interval. Picked to converge inside ~1 cycle once the
/// `2x` staleness window has elapsed, while staying well below the
/// gateway-lease keepalive cadence so we never starve the gateway loop.
pub(crate) const STALL_WATCHDOG_INTERVAL_SECS: u64 = 30;

/// Initial delay before the first watchdog pass - mirrors
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
