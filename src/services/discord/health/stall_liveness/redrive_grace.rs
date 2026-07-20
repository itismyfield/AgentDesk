//! #4181 item-2: monotonic no-progress grace for the redrive destructive
//! trigger, extracted from `stall_liveness` so the parent module stays under
//! the 1000-prod-line giant threshold (the structural split of the
//! stall/liveness judgment authority is tracked by #4615).
//!
//! The redrive no-progress grace measures how long the committed relay offset
//! has stayed frozen. It gates a *destructive* redrive, so the WHOLE lifecycle —
//! grace judgment AND TTL garbage-collection — runs on a process-monotonic
//! clock with ZERO wall-clock dependence. A forward NTP/wall-clock step must be
//! unable to (a) inflate the frozen-duration and fire a redrive early, or
//! (b) evict a monotonically-recent observation and re-arm the grace. The clock
//! is injected via the `RedriveClock` trait rather than a `#[cfg]`-split free
//! function, so the production `MonotonicRedriveClock` is compiled in every
//! build and the real decision + GC path is what tests exercise (with a fake
//! clock). Observations live in a dedicated map, isolated from the wall-clock
//! liveness observations in the parent module, and are updated under the
//! DashMap entry lock so concurrent watchdog passes for the same key serialize.
//!
//! The clock is process-local (does not survive restart), which is correct for
//! an in-process elapsed measurement; a restart-surviving baseline (#4181
//! item-3) is a separate concern needing durable absolute time.

use std::sync::LazyLock;

use dashmap::mapref::entry::Entry;
use poise::serenity_prelude::ChannelId;

use crate::services::provider::ProviderKind;

use super::super::snapshot::WatcherStateSnapshot;
use super::{
    STALL_LIVENESS_STATE_TTL_SECS, STALL_WATCHDOG_BACKLOG_NO_PROGRESS_GRACE_SECS, StallLivenessKey,
    live_undelivered_backlog,
};
use crate::services::discord::RelayFrontierToken;

/// Monotonic seconds source for the redrive no-progress lifecycle (grace + TTL
/// GC). Injected — rather than `#[cfg]`-splitting a free function — so the
/// production reader (`MonotonicRedriveClock`) is compiled in every build and
/// tests drive the real decision + GC path with deterministic time.
trait RedriveClock {
    /// Monotonic seconds: non-decreasing within a process and immune to
    /// wall-clock/NTP steps. There is deliberately NO wall-clock accessor — the
    /// entire redrive no-progress lifecycle must be free of wall dependence.
    fn mono_secs(&self) -> i64;
}

/// Process-start `Instant` anchor. Monotonic and unaffected by wall-clock/NTP
/// steps. Always compiled (never `#[cfg]`-gated) so the production reader below
/// is a real test target, not code that only exists in release builds.
static MONO_ANCHOR: LazyLock<std::time::Instant> = LazyLock::new(std::time::Instant::now);

/// Production monotonic clock: seconds since the process-start `Instant`.
struct MonotonicRedriveClock;

impl RedriveClock for MonotonicRedriveClock {
    fn mono_secs(&self) -> i64 {
        // #4181 item-2 F2: the relay_auto_heal / placeholder_reclaim redrive
        // integration tests drive the grace through the deep production call
        // chain (which threads no clock), so they inject their simulated `now`
        // as the monotonic reading via this override. The real `Instant` reader
        // below stays compiled AND is exercised whenever the override is unset
        // (see `redrive_production_monotonic_clock_path_runs_4181`). The unit
        // tests in this module instead inject a `FakeClock` directly through the
        // `RedriveClock` trait (`*_with_clock`), never touching this override.
        #[cfg(test)]
        {
            if let Some(secs) = TEST_CLOCK_OVERRIDE.with(|cell| cell.get()) {
                return secs;
            }
        }
        MONO_ANCHOR.elapsed().as_secs() as i64
    }
}

#[cfg(test)]
thread_local! {
    static TEST_CLOCK_OVERRIDE: std::cell::Cell<Option<i64>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
pub(in crate::services::discord::health) struct RedriveGraceTestClockGuard {
    previous: Option<i64>,
}

#[cfg(test)]
impl Drop for RedriveGraceTestClockGuard {
    fn drop(&mut self) {
        TEST_CLOCK_OVERRIDE.with(|cell| cell.set(self.previous));
    }
}

/// Scoped monotonic override for deep redrive integration tests. Restores the
/// prior thread-local value on normal return or panic and is absent in production.
#[cfg(test)]
pub(in crate::services::discord::health) fn set_redrive_grace_test_clock(
    mono_secs: i64,
) -> RedriveGraceTestClockGuard {
    let previous = TEST_CLOCK_OVERRIDE.with(|cell| cell.replace(Some(mono_secs)));
    RedriveGraceTestClockGuard { previous }
}

/// #4181 item-2 P3-2: clear the override so the production `Instant` reader runs.
/// Required under `--test-threads=1`, where a prior deep-chain test's override
/// would otherwise leak into the production-clock coverage test and silently
/// bypass the real `MONO_ANCHOR.elapsed()` reader.
#[cfg(test)]
fn clear_redrive_grace_test_clock() {
    TEST_CLOCK_OVERRIDE.with(|cell| cell.set(None));
}

/// Dedicated no-progress tracker, kept separate from the wall-clock
/// `OFFSET_OBSERVATIONS` the liveness path uses. Every timestamp here is
/// monotonic: the redrive lifecycle has zero wall dependence (#4181 item-2).
#[derive(Debug)]
struct NoProgressObservation {
    /// Highest committed relay offset observed for this key. A later snapshot
    /// reporting a LOWER offset is treated as stale and rejected, so a stale
    /// concurrent watchdog pass cannot rewind the freeze anchor (#4181 P3).
    offset: u64,
    /// Monotonic seconds at which `offset` last advanced — the grace anchor.
    unchanged_since_mono_secs: i64,
    /// Monotonic seconds of the last observation — the TTL-GC freshness anchor.
    /// Monotonic (not wall) so a forward wall-clock jump cannot evict a
    /// monotonically-recent observation and re-arm the grace (#4181 item-2 P2).
    last_seen_mono_secs: i64,
}

static NO_PROGRESS_OBSERVATIONS: LazyLock<
    dashmap::DashMap<(StallLivenessKey, u64), NoProgressObservation>,
> = LazyLock::new(dashmap::DashMap::new);

/// Returns `true` iff the observed committed relay offset has stayed UNCHANGED
/// for at least `STALL_WATCHDOG_BACKLOG_NO_PROGRESS_GRACE_SECS` of MONOTONIC time.
///
/// The read-modify-write runs under the DashMap entry lock so concurrent
/// watchdog passes for the same key serialize (#4181 P3).
///
/// The map key includes the frontier reset incarnation. Every legitimate
/// downward reset publishes a distinct incarnation, so it automatically re-arms
/// at the new coordinate. A lower value inside one incarnation is necessarily a
/// stale snapshot and cannot rewind its monotonic freeze anchor.
fn relay_offset_stalled_past_grace(
    key: &StallLivenessKey,
    token: RelayFrontierToken,
    mono_now: i64,
) -> bool {
    let observed_offset = token.committed_offset;
    match NO_PROGRESS_OBSERVATIONS.entry((key.clone(), token.reset_incarnation)) {
        Entry::Occupied(mut occupied) => {
            let observation = occupied.get_mut();
            observation.last_seen_mono_secs = mono_now;
            if observed_offset > observation.offset {
                // Relay advanced: reset the freeze anchor.
                observation.offset = observed_offset;
                observation.unchanged_since_mono_secs = mono_now;
                false
            } else if observed_offset < observation.offset {
                // A lower value under one reset incarnation is a stale snapshot;
                // a legitimate reset always obtains a distinct incarnation key.
                false
            } else {
                // Frozen at the same offset: measure the monotonic freeze age.
                mono_now.saturating_sub(observation.unchanged_since_mono_secs)
                    >= STALL_WATCHDOG_BACKLOG_NO_PROGRESS_GRACE_SECS as i64
            }
        }
        Entry::Vacant(vacant) => {
            vacant.insert(NoProgressObservation {
                offset: observed_offset,
                unchanged_since_mono_secs: mono_now,
                last_seen_mono_secs: mono_now,
            });
            // The first observation can never be past the grace.
            false
        }
    }
}

/// #4181 item-2: a live undelivered backlog whose committed relay offset has
/// been frozen past the no-progress grace (monotonic), so the redrive
/// destructive trigger is eligible to fire. The production path measures time
/// with the process `MonotonicRedriveClock`.
///
/// `token` carries the current authoritative `confirmed_end_offset` together
/// with its reset incarnation, so a JSONL-rotation coordinate reset can be told
/// apart from a stale lagging snapshot (see `relay_offset_stalled_past_grace`).
pub(in crate::services::discord::health) fn stalled_undelivered_backlog_for_redrive(
    provider: &ProviderKind,
    channel_id: ChannelId,
    snapshot: &WatcherStateSnapshot,
    token: RelayFrontierToken,
) -> bool {
    stalled_undelivered_backlog_for_redrive_with_token_and_clock(
        provider,
        channel_id,
        snapshot,
        token,
        &MonotonicRedriveClock,
    )
}

fn stalled_undelivered_backlog_for_redrive_with_token_and_clock(
    provider: &ProviderKind,
    channel_id: ChannelId,
    snapshot: &WatcherStateSnapshot,
    token: RelayFrontierToken,
    clock: &dyn RedriveClock,
) -> bool {
    if !live_undelivered_backlog(snapshot) || snapshot.last_relay_offset != token.committed_offset {
        return false;
    }
    let key = StallLivenessKey::from_snapshot(provider, channel_id, snapshot);
    relay_offset_stalled_past_grace(&key, token, clock.mono_secs())
}

#[cfg(test)]
fn stalled_undelivered_backlog_for_redrive_with_clock(
    provider: &ProviderKind,
    channel_id: ChannelId,
    snapshot: &WatcherStateSnapshot,
    live_committed_offset: u64,
    clock: &dyn RedriveClock,
) -> bool {
    stalled_undelivered_backlog_for_redrive_with_token_and_clock(
        provider,
        channel_id,
        snapshot,
        RelayFrontierToken {
            reset_incarnation: 0,
            committed_offset: live_committed_offset,
        },
        clock,
    )
}

/// Drop redrive no-progress observations for a cleared session. Called by the
/// parent's `clear_stall_watchdog_liveness_state`.
pub(super) fn clear_for_session(probe: &StallLivenessKey) {
    NO_PROGRESS_OBSERVATIONS.retain(|(key, _), _| !key.matches_session(probe));
}

/// TTL-GC on the MONOTONIC freshness anchor (production clock). Called by the
/// parent's `gc_stall_watchdog_liveness_state`. Using monotonic age (not wall)
/// means a forward wall-clock jump cannot evict a monotonically-recent
/// observation and re-arm the grace (#4181 item-2 P2).
pub(super) fn gc() {
    gc_with_clock(&MonotonicRedriveClock);
}

fn gc_with_clock(clock: &dyn RedriveClock) {
    let mono_now = clock.mono_secs();
    NO_PROGRESS_OBSERVATIONS.retain(|_, observation| {
        mono_now.saturating_sub(observation.last_seen_mono_secs)
            <= STALL_LIVENESS_STATE_TTL_SECS as i64
    });
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use poise::serenity_prelude::ChannelId;

    use crate::services::discord::relay_health::{
        RelayActiveTurn, RelayHealthSnapshot, RelayStallState,
    };
    use crate::services::provider::ProviderKind;

    use super::*;

    /// Deterministic monotonic clock for tests, injected through the same
    /// `RedriveClock` trait the production `MonotonicRedriveClock` implements —
    /// so tests exercise the real decision + GC code path (no `#[cfg]` split).
    struct FakeClock {
        mono: Cell<i64>,
    }

    impl FakeClock {
        fn new(mono: i64) -> Self {
            Self {
                mono: Cell::new(mono),
            }
        }

        fn set(&self, mono: i64) {
            self.mono.set(mono);
        }
    }

    impl RedriveClock for FakeClock {
        fn mono_secs(&self) -> i64 {
            self.mono.get()
        }
    }

    /// A frozen, still-live undelivered backlog: unread bytes present, pane
    /// alive, terminal delivery not committed, and `last_relay_offset` fixed at
    /// `relay_offset` so the relay offset reads as frozen across observations.
    fn frozen_backlog_snapshot(
        channel_id: u64,
        tmux_session: &str,
        relay_offset: u64,
        capture_offset: u64,
    ) -> WatcherStateSnapshot {
        let unread = capture_offset.saturating_sub(relay_offset);
        WatcherStateSnapshot {
            provider: ProviderKind::Codex.as_str().to_string(),
            attached: true,
            tmux_session: Some(tmux_session.to_string()),
            watcher_owner_channel_id: Some(channel_id),
            last_relay_offset: relay_offset,
            inflight_state_present: true,
            last_relay_ts_ms: 1_700_000_000_000,
            last_capture_offset: Some(capture_offset),
            capture_coordinate: crate::services::discord::health::liveness_authority::CaptureCoordinateObservation {
                offset: Some(capture_offset),
                path_hash: 0,
                file_id: None,
                status: crate::services::discord::health::liveness_authority::CoordinateStatus::Observed,
            },
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
                last_relay_offset: relay_offset,
                unread_bytes: Some(unread),
                desynced: true,
                stale_thread_proof: false,
            },
        }
    }

    /// #4181 item-2 (grace): the no-progress grace measures elapsed time on the
    /// MONOTONIC clock. Only genuine monotonic elapsed past the grace fires;
    /// wall-clock/NTP steps are irrelevant because the path takes no wall input.
    ///
    /// Mutation proof: making `relay_offset_stalled_past_grace` gate on anything
    /// other than the injected monotonic seconds (e.g. resetting the anchor when
    /// frozen, or comparing against a different clock) flips step (2) or (3).
    #[test]
    fn redrive_no_progress_grace_is_monotonic_4181() {
        let provider = ProviderKind::Codex;
        let channel = ChannelId::new(4_181_002);
        let tmux_session = "AgentDesk-codex-4181-mono-grace";
        super::super::clear_stall_watchdog_liveness_state(&provider, channel, Some(tmux_session));

        let snap = frozen_backlog_snapshot(channel.get(), tmux_session, 10, 301_613);
        assert!(
            live_undelivered_backlog(&snap),
            "precondition: the frozen backlog must be live"
        );
        let grace = STALL_WATCHDOG_BACKLOG_NO_PROGRESS_GRACE_SECS as i64;
        let clock = FakeClock::new(0);
        // The frozen offset equals the live committed frontier throughout.
        let live = 10;

        // (1) Prime at monotonic t=0.
        assert!(
            !stalled_undelivered_backlog_for_redrive_with_clock(
                &provider, channel, &snap, live, &clock,
            ),
            "the first observation can never be past the grace"
        );

        // (2) Only 10s of monotonic time elapsed: must NOT fire.
        clock.set(10);
        assert!(
            !stalled_undelivered_backlog_for_redrive_with_clock(
                &provider, channel, &snap, live, &clock,
            ),
            "10s of monotonic elapsed is inside the grace"
        );

        // (3) Genuine monotonic elapsed past the grace: MUST fire.
        clock.set(grace);
        assert!(
            stalled_undelivered_backlog_for_redrive_with_clock(
                &provider, channel, &snap, live, &clock,
            ),
            "monotonic elapsed past the grace must trip the redrive"
        );

        super::super::clear_stall_watchdog_liveness_state(&provider, channel, Some(tmux_session));
    }

    /// #4181 item-2 P2 (TTL GC is monotonic): a monotonically-recent observation
    /// survives GC (so the grace is NOT re-armed), while a monotonically-stale
    /// one is evicted. Under the pre-fix wall-clock GC, a forward wall jump > TTL
    /// between prime and check could evict the fresh observation and re-arm the
    /// grace; monotonic GC makes that impossible because the path takes no wall
    /// input. This test crosses the TTL boundary, which the earlier test did not.
    ///
    /// Mutation proof: reverting GC to a wall-clock freshness anchor cannot even
    /// compile (the observation carries only monotonic fields); reverting the GC
    /// comparison so it evicts inside the TTL flips the "survives" assertion.
    #[test]
    fn redrive_ttl_gc_is_monotonic_and_survives_wall_jumps_4181() {
        let provider = ProviderKind::Codex;
        let channel = ChannelId::new(4_181_003);
        let tmux_session = "AgentDesk-codex-4181-mono-gc";
        super::super::clear_stall_watchdog_liveness_state(&provider, channel, Some(tmux_session));

        let snap = frozen_backlog_snapshot(channel.get(), tmux_session, 10, 301_613);
        let grace = STALL_WATCHDOG_BACKLOG_NO_PROGRESS_GRACE_SECS as i64;
        let ttl = STALL_LIVENESS_STATE_TTL_SECS as i64;
        let clock = FakeClock::new(1_000);
        let live = 10;

        // Prime the frozen observation at mono=1000.
        assert!(!stalled_undelivered_backlog_for_redrive_with_clock(
            &provider, channel, &snap, live, &clock,
        ));

        // GC one second before the grace boundary. Monotonic age is grace-1
        // (far below the TTL), so the observation MUST survive — no matter how
        // far an (unread) wall clock jumped, since GC never reads wall.
        clock.set(1_000 + grace - 1);
        gc_with_clock(&clock);

        // The freeze anchor survived: crossing the grace fires WITHOUT re-priming.
        clock.set(1_000 + grace);
        assert!(
            stalled_undelivered_backlog_for_redrive_with_clock(
                &provider, channel, &snap, live, &clock,
            ),
            "monotonic GC must not evict a monotonically-recent observation"
        );

        // GC past the monotonic TTL DOES evict genuine staleness, so the next
        // observation re-primes and is not immediately past the grace.
        clock.set(1_000 + grace + ttl + 1);
        gc_with_clock(&clock);
        assert!(
            !stalled_undelivered_backlog_for_redrive_with_clock(
                &provider, channel, &snap, live, &clock,
            ),
            "monotonic GC must evict a monotonically-stale observation (re-prime)"
        );

        super::super::clear_stall_watchdog_liveness_state(&provider, channel, Some(tmux_session));
    }

    /// #4181 P3 (atomic + stale-offset rejection): a stale concurrent snapshot
    /// reporting a LOWER committed offset than already recorded must not rewind
    /// the freeze anchor and re-arm the grace.
    ///
    /// Mutation proof: dropping the stale sub-branch (so a lower offset always
    /// re-arms `unchanged_since_mono_secs`) makes the final grace-boundary check
    /// measure ~1s of freeze and return `false`, failing.
    #[test]
    fn redrive_rejects_stale_lower_offset_4181() {
        let provider = ProviderKind::Codex;
        let channel = ChannelId::new(4_181_004);
        let tmux_session = "AgentDesk-codex-4181-stale-offset";
        super::super::clear_stall_watchdog_liveness_state(&provider, channel, Some(tmux_session));

        let grace = STALL_WATCHDOG_BACKLOG_NO_PROGRESS_GRACE_SECS as i64;
        let clock = FakeClock::new(0);
        // The live committed frontier stays at 100 the whole time — the lower
        // snapshot below is stale (lagging), NOT an authoritative rotation.
        let live = 100;

        // Frozen backlog at a HIGH committed offset (100). Prime at mono=0.
        let high = frozen_backlog_snapshot(channel.get(), tmux_session, 100, 301_613);
        assert!(!stalled_undelivered_backlog_for_redrive_with_clock(
            &provider, channel, &high, live, &clock,
        ));

        // A stale concurrent snapshot reporting a LOWER offset (50) arrives near
        // the grace boundary while the live frontier is still 100. It must NOT
        // rewind the freeze anchor: the recorded high-offset freeze keeps aging.
        clock.set(grace - 1);
        let stale_low = frozen_backlog_snapshot(channel.get(), tmux_session, 50, 301_613);
        assert!(!stalled_undelivered_backlog_for_redrive_with_clock(
            &provider, channel, &stale_low, live, &clock,
        ));

        // Crossing the grace with the true high offset fires — the stale lower
        // snapshot did not re-arm the grace.
        clock.set(grace);
        assert!(
            stalled_undelivered_backlog_for_redrive_with_clock(
                &provider, channel, &high, live, &clock,
            ),
            "a stale lower-offset snapshot must not rewind the monotonic freeze anchor"
        );

        super::super::clear_stall_watchdog_liveness_state(&provider, channel, Some(tmux_session));
    }

    /// #4181 (rotation re-arm): a JSONL size-cap rotation lowers the authoritative
    /// `confirmed_end_offset` H->L on the SAME session/turn (same key). The grace
    /// must RE-ARM at the new coordinate L — not treat L as a stale snapshot
    /// forever — so a real stall at L still fires. This is the failure the
    /// unconditional stale-rejection introduced: the observed lower offset equals
    /// the live frontier (rotation reset), unlike the stale case above where it
    /// is below the live frontier.
    ///
    /// Mutation proof: removing the `observed_offset >= live_offset` re-arm arm
    /// (so a rotation-lowered offset is rejected like a stale snapshot) leaves the
    /// grace anchored at H forever; the post-rotation stall then never fires and
    /// the final assertion fails.
    #[test]
    fn redrive_rotation_reset_rearms_grace_4181() {
        let provider = ProviderKind::Codex;
        let channel = ChannelId::new(4_181_006);
        let tmux_session = "AgentDesk-codex-4181-rotation";
        super::super::clear_stall_watchdog_liveness_state(&provider, channel, Some(tmux_session));

        let grace = STALL_WATCHDOG_BACKLOG_NO_PROGRESS_GRACE_SECS as i64;
        let clock = FakeClock::new(0);

        // Frozen at a HIGH offset H=100 (live frontier 100). Prime at mono=0 and
        // let it age right up to the grace boundary.
        let high = frozen_backlog_snapshot(channel.get(), tmux_session, 100, 301_613);
        let high_token = RelayFrontierToken {
            reset_incarnation: 0,
            committed_offset: 100,
        };
        assert!(
            !stalled_undelivered_backlog_for_redrive_with_token_and_clock(
                &provider, channel, &high, high_token, &clock,
            )
        );
        clock.set(grace - 1);
        assert!(
            !stalled_undelivered_backlog_for_redrive_with_token_and_clock(
                &provider, channel, &high, high_token, &clock,
            )
        );

        // Resetting H→L advances the incarnation, so L receives a fresh grace
        // namespace even though the session and turn identity did not change.
        let low = frozen_backlog_snapshot(channel.get(), tmux_session, 40, 301_613);
        let low_token = RelayFrontierToken {
            reset_incarnation: 1,
            committed_offset: 40,
        };
        clock.set(grace);
        assert!(
            !stalled_undelivered_backlog_for_redrive_with_token_and_clock(
                &provider, channel, &low, low_token, &clock,
            ),
            "a new reset incarnation must re-arm the grace at L"
        );

        clock.set(grace + grace);
        assert!(
            stalled_undelivered_backlog_for_redrive_with_token_and_clock(
                &provider, channel, &low, low_token, &clock,
            ),
            "a real stall at the post-reset frontier must fire after the grace"
        );

        super::super::clear_stall_watchdog_liveness_state(&provider, channel, Some(tmux_session));
    }

    #[test]
    fn scoped_test_clock_restores_nested_and_panicking_overrides_4181() {
        clear_redrive_grace_test_clock();
        let outer = set_redrive_grace_test_clock(11);
        assert_eq!(MonotonicRedriveClock.mono_secs(), 11);
        {
            let _inner = set_redrive_grace_test_clock(22);
            assert_eq!(MonotonicRedriveClock.mono_secs(), 22);
        }
        assert_eq!(MonotonicRedriveClock.mono_secs(), 11);

        let panic = std::panic::catch_unwind(|| {
            let _panicking = set_redrive_grace_test_clock(33);
            assert_eq!(MonotonicRedriveClock.mono_secs(), 33);
            panic!("exercise panic-safe clock restoration");
        });
        assert!(panic.is_err());
        assert_eq!(MonotonicRedriveClock.mono_secs(), 11);
        drop(outer);
        assert!(TEST_CLOCK_OVERRIDE.with(|cell| cell.get()).is_none());
    }

    /// #4181 F2 (production clock coverage): exercise the real
    /// `MonotonicRedriveClock` (process `Instant`) through the production entry
    /// points, so the clock reader is compiled AND executed under test rather
    /// than hidden behind a `#[cfg]`. A first observation is deterministically
    /// not past the grace regardless of the clock's absolute value.
    #[test]
    fn redrive_production_monotonic_clock_path_runs_4181() {
        let provider = ProviderKind::Codex;
        let channel = ChannelId::new(4_181_005);
        let tmux_session = "AgentDesk-codex-4181-prod-clock";
        super::super::clear_stall_watchdog_liveness_state(&provider, channel, Some(tmux_session));
        // #4181 P3-2: under `--test-threads=1` a prior deep-chain test may have
        // left the override set on this thread; clear it so the REAL `Instant`
        // reader runs (otherwise this coverage assertion is silently bypassed).
        clear_redrive_grace_test_clock();

        let snap = frozen_backlog_snapshot(channel.get(), tmux_session, 10, 301_613);
        assert!(
            !stalled_undelivered_backlog_for_redrive(
                &provider,
                channel,
                &snap,
                RelayFrontierToken {
                    reset_incarnation: 0,
                    committed_offset: 10,
                },
            ),
            "first observation on the real monotonic clock is not past the grace"
        );
        // The production GC runs on the real clock without panicking.
        gc();

        super::super::clear_stall_watchdog_liveness_state(&provider, channel, Some(tmux_session));
    }
}
