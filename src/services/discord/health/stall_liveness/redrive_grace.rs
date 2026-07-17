//! #4181 item-2: monotonic no-progress grace for the redrive destructive
//! trigger, extracted from `stall_liveness` so that parent module stays under
//! the 1000-prod-line giant threshold (the structural split of the
//! stall/liveness judgment authority is tracked by #4615).
//!
//! The redrive no-progress grace measures how long the committed relay offset
//! has stayed frozen. It gates a *destructive* redrive, so it must measure PURE
//! elapsed time: a forward NTP/wall-clock step must never inflate the measured
//! frozen-duration and fire a redrive early. It therefore runs on a
//! process-monotonic `Instant` clock, isolated in its own observation map so it
//! neither perturbs nor is perturbed by the wall-clock liveness observations in
//! the parent module. The clock is process-local (does not survive restart),
//! which is correct for an in-process elapsed measurement; a restart-surviving
//! baseline (#4181 item-3) is a separate concern needing durable absolute time.

use std::sync::LazyLock;

use poise::serenity_prelude::ChannelId;

use crate::services::provider::ProviderKind;

use super::{
    OffsetObservationKind, STALL_WATCHDOG_BACKLOG_NO_PROGRESS_GRACE_SECS, StallLivenessKey,
    WatcherStateSnapshot, live_undelivered_backlog, liveness_state_expired,
};

/// Process-start monotonic anchor. `Instant` advances monotonically and cannot
/// be stepped by an NTP correction or a manual clock change, so it is the
/// correct basis for a pure elapsed-time measurement that gates a destructive
/// redrive.
#[cfg(not(test))]
static MONO_ANCHOR: LazyLock<std::time::Instant> = LazyLock::new(std::time::Instant::now);

// Test-only monotonic override. Tests drive time through injected wall-clock
// seconds; with no explicit override the grace mirrors that injected wall clock
// (so every existing time-driven test still exercises the grace), and a test
// can set this to decouple monotonic from wall time and prove the grace tracks
// the monotonic clock alone.
#[cfg(test)]
thread_local! {
    static MONO_OVERRIDE: std::cell::Cell<Option<i64>> = const { std::cell::Cell::new(None) };
}

/// Monotonic seconds for the grace. Production reads the `Instant` anchor and
/// ignores `wall_now_unix_secs`; test builds mirror the injected wall clock
/// unless a test set an explicit monotonic override.
fn mono_now_secs(wall_now_unix_secs: i64) -> i64 {
    #[cfg(test)]
    {
        MONO_OVERRIDE
            .with(|cell| cell.get())
            .unwrap_or(wall_now_unix_secs)
    }
    #[cfg(not(test))]
    {
        let _ = wall_now_unix_secs;
        MONO_ANCHOR.elapsed().as_secs() as i64
    }
}

#[cfg(test)]
fn set_mono_override_for_test(secs: i64) {
    MONO_OVERRIDE.with(|cell| cell.set(Some(secs)));
}

#[cfg(test)]
fn clear_mono_override_for_test() {
    MONO_OVERRIDE.with(|cell| cell.set(None));
}

/// Dedicated monotonic no-progress tracker, kept separate from the wall-clock
/// `OFFSET_OBSERVATIONS` the liveness path uses.
#[derive(Clone, Debug)]
struct NoProgressObservation {
    offset: u64,
    /// Monotonic process-seconds at which `offset` last changed. The grace
    /// measures elapsed against this, so a wall-clock/NTP step cannot fire a
    /// destructive redrive early.
    unchanged_since_mono_secs: i64,
    /// Wall-clock unix seconds of the last observation. Used only for TTL GC
    /// parity with the sibling liveness maps — never for the grace gate.
    last_updated_unix_secs: i64,
}

static NO_PROGRESS_OBSERVATIONS: LazyLock<
    dashmap::DashMap<StallLivenessKey, NoProgressObservation>,
> = LazyLock::new(dashmap::DashMap::new);

/// Returns `true` iff the committed relay offset has stayed UNCHANGED for at
/// least `STALL_WATCHDOG_BACKLOG_NO_PROGRESS_GRACE_SECS` of MONOTONIC time (and
/// did not advance on this tick). Mirrors the wall-clock relay-offset
/// observation semantics used by the liveness path, but anchored on the
/// monotonic clock so NTP/wall-clock steps cannot shorten the grace.
fn relay_offset_stalled_past_grace(
    key: &StallLivenessKey,
    current_offset: u64,
    mono_now: i64,
    now_unix_secs: i64,
) -> bool {
    let key = key.for_offset_kind(OffsetObservationKind::RelayDelivered);
    let previous = NO_PROGRESS_OBSERVATIONS
        .get(&key)
        .map(|entry| entry.clone());
    let advanced_this_tick = previous
        .as_ref()
        .is_some_and(|prev| current_offset > prev.offset);
    let unchanged_since_mono_secs = match previous.as_ref() {
        Some(prev) if current_offset == prev.offset => prev.unchanged_since_mono_secs,
        _ => mono_now,
    };
    NO_PROGRESS_OBSERVATIONS.insert(
        key,
        NoProgressObservation {
            offset: current_offset,
            unchanged_since_mono_secs,
            last_updated_unix_secs: now_unix_secs,
        },
    );
    let unchanged_age_secs = mono_now.saturating_sub(unchanged_since_mono_secs).max(0);
    !advanced_this_tick
        && unchanged_age_secs >= STALL_WATCHDOG_BACKLOG_NO_PROGRESS_GRACE_SECS as i64
}

/// #4181 item-2: a live undelivered backlog whose committed relay offset has
/// been frozen past the no-progress grace (monotonic), so the redrive
/// destructive trigger is eligible to fire.
pub(in crate::services::discord::health) fn stalled_undelivered_backlog_for_redrive(
    provider: &ProviderKind,
    channel_id: ChannelId,
    snapshot: &WatcherStateSnapshot,
    now_unix_secs: i64,
) -> bool {
    if !live_undelivered_backlog(snapshot) {
        return false;
    }

    let key = StallLivenessKey::from_snapshot(provider, channel_id, snapshot);
    relay_offset_stalled_past_grace(
        &key,
        snapshot.last_relay_offset,
        mono_now_secs(now_unix_secs),
        now_unix_secs,
    )
}

/// Drop redrive no-progress observations for a cleared session. Called by the
/// parent's `clear_stall_watchdog_liveness_state`.
pub(super) fn clear_for_session(probe: &StallLivenessKey) {
    NO_PROGRESS_OBSERVATIONS.retain(|key, _| !key.matches_session(probe));
}

/// TTL-GC redrive no-progress observations. Called by the parent's
/// `gc_stall_watchdog_liveness_state`.
pub(super) fn gc(now_unix_secs: i64) {
    NO_PROGRESS_OBSERVATIONS
        .retain(|_, obs| !liveness_state_expired(obs.last_updated_unix_secs, now_unix_secs));
}

#[cfg(test)]
mod tests {
    use poise::serenity_prelude::ChannelId;

    use crate::services::discord::relay_health::{
        RelayActiveTurn, RelayHealthSnapshot, RelayStallState,
    };
    use crate::services::provider::ProviderKind;

    use super::*;

    /// A frozen, still-live undelivered backlog: unread bytes present, pane
    /// alive, terminal delivery not committed, and `last_relay_offset` fixed at
    /// 10 so the relay offset reads as frozen across observations.
    fn frozen_backlog_snapshot(
        channel_id: u64,
        tmux_session: &str,
        capture_offset: u64,
    ) -> WatcherStateSnapshot {
        let unread = capture_offset.saturating_sub(10);
        WatcherStateSnapshot {
            provider: ProviderKind::Codex.as_str().to_string(),
            attached: true,
            tmux_session: Some(tmux_session.to_string()),
            watcher_owner_channel_id: Some(channel_id),
            last_relay_offset: 10,
            inflight_state_present: true,
            last_relay_ts_ms: 1_700_000_000_000,
            last_capture_offset: Some(capture_offset),
            unread_bytes: Some(unread),
            desynced: true,
            reconnect_count: 0,
            inflight_started_at: Some("2026-06-12 00:00:00".to_string()),
            inflight_updated_at: Some("2026-06-12 00:00:00".to_string()),
            inflight_user_msg_id: Some(9001),
            inflight_current_msg_id: Some(9002),
            tmux_session_alive: Some(true),
            has_pending_queue: false,
            mailbox_active_user_msg_id: Some(9001),
            bound_output_path: None,
            bound_session_id: None,
            inflight_terminal_delivery_committed: false,
            inflight_identity: None,
            inflight_finalizer_turn_id: None,
            inflight_output_path: Some(format!("/tmp/{tmux_session}.jsonl")),
            relay_stall_state: RelayStallState::TmuxAliveRelayDead,
            relay_health: RelayHealthSnapshot {
                provider: ProviderKind::Codex.as_str().to_string(),
                channel_id,
                active_turn: RelayActiveTurn::Foreground,
                tmux_session: Some(tmux_session.to_string()),
                tmux_alive: Some(true),
                watcher_attached: true,
                watcher_attached_stale: false,
                watcher_owner_channel_id: Some(channel_id),
                watcher_owns_live_relay: true,
                bridge_inflight_present: true,
                bridge_current_msg_id: Some(9002),
                mailbox_has_cancel_token: true,
                mailbox_active_user_msg_id: Some(9001),
                mailbox_turn_started_at_ms: None,
                queue_depth: 0,
                pending_discord_callback_msg_id: Some(9002),
                pending_thread_proof: false,
                parent_channel_id: None,
                thread_channel_id: None,
                last_relay_ts_ms: Some(1_700_000_000_000),
                last_outbound_activity_ms: None,
                last_capture_offset: Some(capture_offset),
                last_relay_offset: 10,
                unread_bytes: Some(unread),
                desynced: true,
                stale_thread_proof: false,
            },
        }
    }

    /// #4181 item-2: the redrive no-progress grace must measure elapsed time on
    /// the MONOTONIC clock, not wall-clock. A forward wall-clock/NTP step must
    /// not inflate the frozen-duration and trip the *destructive* redrive early;
    /// only genuine monotonic elapsed past the grace may.
    ///
    /// Mutation proof: revert the grace gate to the wall-clock `now_unix_secs`
    /// (drop `mono_now_secs`) — the pre-#4181 defect — and step (2)'s inflated
    /// wall delta (≥ grace) flips the result to `true`, failing that assertion.
    /// Step (3) independently pins the gate to the monotonic clock: with the
    /// wall clock barely moved, only a monotonic elapsed ≥ grace makes it fire,
    /// so a mutant that reads wall time there would return `false` and fail (3).
    #[test]
    fn redrive_no_progress_grace_is_monotonic_not_wall_clock_4181() {
        let provider = ProviderKind::Codex;
        let channel = ChannelId::new(4_181_002);
        let tmux_session = "AgentDesk-codex-4181-mono-grace";
        super::super::clear_stall_watchdog_liveness_state(&provider, channel, Some(tmux_session));

        let snap = frozen_backlog_snapshot(channel.get(), tmux_session, 301_613);
        assert!(
            live_undelivered_backlog(&snap),
            "precondition: the frozen backlog must be live"
        );

        let wall_base = 1_800_000_000;
        let grace = STALL_WATCHDOG_BACKLOG_NO_PROGRESS_GRACE_SECS as i64;

        // (1) Prime the observation at monotonic t=0.
        set_mono_override_for_test(0);
        assert!(
            !stalled_undelivered_backlog_for_redrive(&provider, channel, &snap, wall_base),
            "the first observation can never be past the grace"
        );

        // (2) Forward wall-clock/NTP jump: the wall clock leaps past the grace,
        // but only 10s of real MONOTONIC time elapsed. The destructive grace must
        // see 10s, not the inflated wall delta, and must NOT fire.
        set_mono_override_for_test(10);
        assert!(
            !stalled_undelivered_backlog_for_redrive(
                &provider,
                channel,
                &snap,
                wall_base + grace + 100,
            ),
            "a forward wall-clock/NTP jump must not trip the monotonic no-progress grace"
        );

        // (3) Genuine monotonic elapsed past the grace (offset still frozen)
        // while the wall clock barely moved: the redrive MUST fire.
        set_mono_override_for_test(grace);
        assert!(
            stalled_undelivered_backlog_for_redrive(&provider, channel, &snap, wall_base + 5),
            "genuine monotonic elapsed past the grace must trip the redrive"
        );

        clear_mono_override_for_test();
        super::super::clear_stall_watchdog_liveness_state(&provider, channel, Some(tmux_session));
    }
}
