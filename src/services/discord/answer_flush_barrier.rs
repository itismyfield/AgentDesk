//! #3082 part B — per-channel answer-flush barrier.
//!
//! Problem: when the active turn's final answer exceeds Discord's 2000-char
//! limit it is delivered as multiple chunks (`send_long_message_raw*`). The
//! chunk loop sleeps ~500ms between sends, opening a window where a queued-turn
//! notice POST (`send_intake_placeholder`) could land *between* answer chunks:
//!
//! ```text
//! answer chunk 1
//! 📬 메시지 대기 중   <- interleaved, wrong order
//! answer chunk 2
//! ```
//!
//! This barrier lets the queued-card POST path wait until the in-flight
//! multi-chunk answer has finished flushing, so the notice always lands AFTER
//! the final chunk (a single trailing card).
//!
//! Safety: the barrier is *advisory and bounded*. It never blocks indefinitely.
//!
//! * Setting the gate is done through an RAII [`AnswerFlushGuard`]; the count is
//!   decremented on `Drop`, so every exit path of the chunk loop (success,
//!   early `return Err`, `?`, panic-unwind) clears it. It can never strand set.
//! * The waiter (`wait_for_flush`) is *progress-aware*: the holder bumps a
//!   per-channel "last progress" instant on guard acquire, on EACH chunk
//!   delivered, and on guard release. The waiter proceeds when the gate clears,
//!   when no progress has been observed for an inactivity grace window
//!   ([`ANSWER_FLUSH_WAIT_TIMEOUT`]), or when an absolute hard ceiling
//!   ([`ANSWER_FLUSH_WAIT_HARD_CEILING`]) is hit — whichever comes first. A long
//!   answer that keeps making progress never trips the inactivity timeout, but a
//!   genuinely stuck/crashed flush still releases. No deadlock, no permanent
//!   suppression.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use poise::serenity_prelude::ChannelId;

/// Inactivity grace window: the queued-card POST path proceeds once the
/// in-flight flush has made NO progress (no new chunk delivered) for this long.
/// This is a *per-chunk* inactivity bound, NOT a total-time bound — a long
/// answer that keeps delivering chunks resets the window on every chunk and so
/// never trips it. Generous enough to cover the inter-chunk 500ms sleeps plus
/// realistic rate-limit/network delay on a single chunk.
pub(super) const ANSWER_FLUSH_WAIT_TIMEOUT: Duration = Duration::from_secs(8);

/// Absolute hard ceiling on the total wait, regardless of progress. A final
/// backstop so the queued card can never wait forever even if a flush somehow
/// keeps bumping progress without ever finishing. Large enough that a genuine
/// long multi-chunk answer (~16+ chunks) completes well within it.
pub(super) const ANSWER_FLUSH_WAIT_HARD_CEILING: Duration = Duration::from_secs(60);

/// Poll interval while waiting for the gate to clear.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Default)]
struct ChannelFlushState {
    /// Number of in-flight multi-chunk answer flushes on this channel. A count
    /// (not a bool) tolerates the rare overlap of two multi-chunk sends on the
    /// same channel without one clearing the other's gate.
    inflight: usize,
    /// Instant of the most recent observable progress (guard acquire, chunk
    /// delivered, or guard release) for this channel. The waiter reads this to
    /// decide whether the flush is still making progress.
    last_progress: Option<Instant>,
}

#[derive(Default)]
pub(in crate::services::discord) struct AnswerFlushBarrier {
    /// Per-channel flush bookkeeping. A channel is "flushing" while its
    /// `inflight` count is > 0.
    channels: Mutex<HashMap<ChannelId, ChannelFlushState>>,
}

impl AnswerFlushBarrier {
    /// Mark the start of a multi-chunk answer flush for `channel_id`. The
    /// returned guard clears the mark on drop. Call this ONLY for genuine
    /// multi-chunk (>1 chunk) sends — single-chunk answers cannot be split, so
    /// there is no interleaving window to guard. Acquiring counts as progress.
    pub(in crate::services::discord) fn begin_flush(
        self: &std::sync::Arc<Self>,
        channel_id: ChannelId,
    ) -> AnswerFlushGuard {
        if let Ok(mut map) = self.channels.lock() {
            let state = map.entry(channel_id).or_default();
            state.inflight += 1;
            state.last_progress = Some(Instant::now());
        }
        AnswerFlushGuard {
            barrier: self.clone(),
            channel_id,
        }
    }

    /// Record that the flush on `channel_id` just made progress (e.g. delivered
    /// another chunk). Resets the waiter's inactivity window so a legitimately
    /// long answer is never cut off mid-flush. No-op if the channel is not
    /// currently flushing.
    pub(in crate::services::discord) fn note_progress(&self, channel_id: ChannelId) {
        if let Ok(mut map) = self.channels.lock()
            && let Some(state) = map.get_mut(&channel_id)
            && state.inflight > 0
        {
            state.last_progress = Some(Instant::now());
        }
    }

    // #3034: flushing-state probe used only by the unit tests; the live waiter
    // uses `flush_snapshot` (single-lock) instead. Test contract.
    #[allow(dead_code)]
    fn is_flushing(&self, channel_id: ChannelId) -> bool {
        self.channels
            .lock()
            .map(|map| {
                map.get(&channel_id)
                    .map(|state| state.inflight > 0)
                    .unwrap_or(false)
            })
            .unwrap_or(false)
    }

    /// Snapshot `(still_flushing, last_progress)` for `channel_id` under a
    /// single lock acquire, so the waiter never holds the lock across `.await`.
    fn flush_snapshot(&self, channel_id: ChannelId) -> (bool, Option<Instant>) {
        self.channels
            .lock()
            .map(|map| {
                map.get(&channel_id)
                    .map(|state| (state.inflight > 0, state.last_progress))
                    .unwrap_or((false, None))
            })
            .unwrap_or((false, None))
    }

    fn end_flush(&self, channel_id: ChannelId) {
        if let Ok(mut map) = self.channels.lock()
            && let Some(state) = map.get_mut(&channel_id)
        {
            state.inflight = state.inflight.saturating_sub(1);
            if state.inflight == 0 {
                map.remove(&channel_id);
            } else {
                // A release while other overlapping flushes remain still counts
                // as progress for the survivors.
                state.last_progress = Some(Instant::now());
            }
        }
    }

    /// Wait until no multi-chunk answer flush is in flight for `channel_id`.
    ///
    /// Progress-aware (see module docs): returns `true` as soon as the gate
    /// clears; returns `false` (caller proceeds anyway) once the flush has made
    /// NO progress for `inactivity_grace`, OR once `hard_ceiling` of total wall
    /// time elapses — whichever comes first. A flush that keeps delivering
    /// chunks resets the inactivity window on each chunk and so is never cut
    /// off by it. Always returns `true` promptly when no flush is active.
    ///
    /// Deadlock-free: the lock is never held across an `.await`.
    pub(in crate::services::discord) async fn wait_for_flush(
        &self,
        channel_id: ChannelId,
        inactivity_grace: Duration,
        hard_ceiling: Duration,
    ) -> bool {
        let (flushing, _) = self.flush_snapshot(channel_id);
        if !flushing {
            return true;
        }
        let start = Instant::now();
        loop {
            tokio::time::sleep(POLL_INTERVAL).await;
            let now = Instant::now();
            let (flushing, last_progress) = self.flush_snapshot(channel_id);
            if !flushing {
                return true;
            }
            // Absolute backstop: never wait past the hard ceiling.
            if now.duration_since(start) >= hard_ceiling {
                return false;
            }
            // Inactivity backstop: proceed if the flush has stopped making
            // progress for the grace window. `last_progress` is bumped on
            // acquire and on each delivered chunk, so an actively-progressing
            // flush keeps this from tripping.
            let stalled_for = last_progress
                .map(|p| now.duration_since(p))
                .unwrap_or(Duration::ZERO);
            if stalled_for >= inactivity_grace {
                return false;
            }
        }
    }
}

/// RAII guard that clears its channel's answer-flush mark on drop. Holding it
/// across the entire multi-chunk send loop guarantees the gate is cleared on
/// every exit path (Ok, Err via `?`, panic-unwind).
pub(in crate::services::discord) struct AnswerFlushGuard {
    barrier: std::sync::Arc<AnswerFlushBarrier>,
    channel_id: ChannelId,
}

impl Drop for AnswerFlushGuard {
    fn drop(&mut self) {
        self.barrier.end_flush(self.channel_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// Convenience: wait with a generous hard ceiling so these tests exercise
    /// the gate-clear / inactivity paths rather than the ceiling.
    async fn wait(barrier: &AnswerFlushBarrier, channel: ChannelId, grace: Duration) -> bool {
        barrier
            .wait_for_flush(channel, grace, Duration::from_secs(30))
            .await
    }

    #[tokio::test]
    async fn no_flush_returns_immediately() {
        let barrier = Arc::new(AnswerFlushBarrier::default());
        let channel = ChannelId::new(1);
        let start = Instant::now();
        assert!(
            wait(&barrier, channel, Duration::from_secs(5)).await,
            "with no flush in flight the waiter must return true immediately"
        );
        assert!(
            start.elapsed() < Duration::from_millis(200),
            "no-flush wait must not block"
        );
    }

    #[tokio::test]
    async fn guard_clears_gate_on_drop() {
        let barrier = Arc::new(AnswerFlushBarrier::default());
        let channel = ChannelId::new(2);
        {
            let _guard = barrier.begin_flush(channel);
            assert!(barrier.is_flushing(channel), "gate set while guard held");
        }
        assert!(
            !barrier.is_flushing(channel),
            "gate must clear when guard drops"
        );
        assert!(
            wait(&barrier, channel, Duration::from_secs(5)).await,
            "waiter sees cleared gate after guard drop"
        );
    }

    #[tokio::test]
    async fn waiter_unblocks_when_flush_ends() {
        let barrier = Arc::new(AnswerFlushBarrier::default());
        let channel = ChannelId::new(3);
        let guard = barrier.begin_flush(channel);
        let barrier_for_wait = barrier.clone();
        let waiter =
            tokio::spawn(
                async move { wait(&barrier_for_wait, channel, Duration::from_secs(5)).await },
            );
        // Hold the gate briefly, then release; the waiter must observe the
        // release and return true (not time out).
        tokio::time::sleep(Duration::from_millis(120)).await;
        drop(guard);
        assert!(
            waiter.await.unwrap(),
            "waiter must unblock with true once the flush ends"
        );
    }

    #[tokio::test]
    async fn waiter_times_out_when_flush_never_ends() {
        let barrier = Arc::new(AnswerFlushBarrier::default());
        let channel = ChannelId::new(4);
        // Leak the guard so the gate stays set — simulates a stuck/errored
        // flush that never dropped its guard AND never reports progress. The
        // waiter MUST still return (false) via the inactivity backstop so the
        // queued card is never permanently suppressed.
        let guard = barrier.begin_flush(channel);
        std::mem::forget(guard);
        let cleared = wait(&barrier, channel, Duration::from_millis(200)).await;
        assert!(
            !cleared,
            "a never-ending flush must time out (false), not deadlock"
        );
    }

    /// P1-2: a long answer that keeps making progress beyond the (short)
    /// inactivity grace must NOT trip the timeout — the waiter keeps waiting as
    /// long as chunks are still being delivered, so the queued card waits to the
    /// very end of the answer.
    #[tokio::test]
    async fn progress_keeps_waiter_blocked_past_inactivity_grace() {
        let barrier = Arc::new(AnswerFlushBarrier::default());
        let channel = ChannelId::new(6);
        let guard = barrier.begin_flush(channel);

        let barrier_for_wait = barrier.clone();
        // Short 100ms inactivity grace, but a generous hard ceiling.
        let waiter = tokio::spawn(async move {
            barrier_for_wait
                .wait_for_flush(channel, Duration::from_millis(100), Duration::from_secs(30))
                .await
        });

        // Simulate a long answer: bump progress every 40ms (< grace) for ~500ms,
        // which is FAR longer than the 100ms inactivity grace. If the timeout
        // were absolute (old behavior) it would have fired; progress-awareness
        // must keep the waiter blocked.
        for _ in 0..12 {
            tokio::time::sleep(Duration::from_millis(40)).await;
            barrier.note_progress(channel);
        }
        assert!(
            !waiter.is_finished(),
            "an actively-progressing flush must keep the waiter blocked past the inactivity grace"
        );

        // The answer finishes — release the guard. The waiter must now return
        // true (it waited to the END of the answer, never timed out).
        drop(guard);
        assert!(
            waiter.await.unwrap(),
            "once the long answer finishes the waiter returns true, having waited to the end"
        );
    }

    /// P1-2: a flush that stops making progress (stuck/crashed mid-answer)
    /// still releases the waiter after the inactivity grace, even though it
    /// reported some early progress.
    #[tokio::test]
    async fn stalled_flush_releases_after_inactivity_grace() {
        let barrier = Arc::new(AnswerFlushBarrier::default());
        let channel = ChannelId::new(7);
        let guard = barrier.begin_flush(channel);
        // One early progress bump, then silence (simulating a crash mid-flush).
        barrier.note_progress(channel);
        std::mem::forget(guard);

        let start = Instant::now();
        let cleared = barrier
            .wait_for_flush(channel, Duration::from_millis(150), Duration::from_secs(30))
            .await;
        assert!(
            !cleared,
            "a stalled flush must release via the inactivity grace (false)"
        );
        // It should have waited at least roughly the grace window, but far less
        // than the 30s hard ceiling — proving inactivity, not ceiling, fired.
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "inactivity grace (not the hard ceiling) must release a stalled flush"
        );
    }

    /// P1-2: the hard ceiling is the final backstop — even a flush that keeps
    /// bumping progress forever cannot make the waiter wait past the ceiling.
    #[tokio::test]
    async fn hard_ceiling_releases_even_with_continuous_progress() {
        let barrier = Arc::new(AnswerFlushBarrier::default());
        let channel = ChannelId::new(8);
        let guard = barrier.begin_flush(channel);

        let barrier_for_wait = barrier.clone();
        // Tiny hard ceiling, large inactivity grace: only the ceiling can fire.
        let waiter = tokio::spawn(async move {
            barrier_for_wait
                .wait_for_flush(channel, Duration::from_secs(30), Duration::from_millis(200))
                .await
        });

        // Keep bumping progress continuously so the inactivity grace never
        // trips; only the hard ceiling can release the waiter.
        let pump = tokio::spawn({
            let barrier = barrier.clone();
            async move {
                for _ in 0..50 {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    barrier.note_progress(channel);
                }
            }
        });

        let cleared = waiter.await.unwrap();
        assert!(
            !cleared,
            "the hard ceiling must release the waiter (false) even under continuous progress"
        );
        pump.abort();
        drop(guard);
    }

    #[tokio::test]
    async fn nested_flushes_keep_gate_until_last_drop() {
        let barrier = Arc::new(AnswerFlushBarrier::default());
        let channel = ChannelId::new(5);
        let g1 = barrier.begin_flush(channel);
        let g2 = barrier.begin_flush(channel);
        drop(g1);
        assert!(
            barrier.is_flushing(channel),
            "gate stays set while one of two overlapping flushes is live"
        );
        drop(g2);
        assert!(
            !barrier.is_flushing(channel),
            "gate clears only after the last overlapping flush drops"
        );
    }

    /// P1-1: the edit/replace answer path (`replace_long_message_raw_with_outcome`)
    /// now holds the SAME barrier guard for multi-chunk sends as the plain send
    /// path. This test exercises that contract end-to-end from the queued card's
    /// perspective: while the edit/replace flush holds its guard AND keeps
    /// delivering continuation chunks (each a `note_progress`), the queued-card
    /// waiter must stay blocked — even past the inactivity grace — and only
    /// release (true) once the final continuation lands and the guard drops.
    #[tokio::test]
    async fn edit_replace_flush_holds_barrier_and_queued_card_waits_for_it() {
        let barrier = Arc::new(AnswerFlushBarrier::default());
        let channel = ChannelId::new(9);

        // The edit/replace path acquires the guard before the first edit when
        // total chunks > 1, exactly as the send path does.
        let replace_guard = barrier.begin_flush(channel);

        let barrier_for_card = barrier.clone();
        // The queued "📬" card POST path waits behind the in-flight flush.
        let queued_card = tokio::spawn(async move {
            barrier_for_card
                .wait_for_flush(channel, Duration::from_millis(120), Duration::from_secs(30))
                .await
        });

        // Simulate the edit/replace continuation loop delivering several chunks,
        // each bumping progress (mirroring the new `note_progress` call after a
        // successful continuation send). Span well past the inactivity grace.
        for _ in 0..6 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            barrier.note_progress(channel);
        }
        assert!(
            !queued_card.is_finished(),
            "queued card must wait while the edit/replace answer is still flushing chunks"
        );

        // Final continuation landed; the replace fn returns and drops its guard.
        drop(replace_guard);
        assert!(
            queued_card.await.unwrap(),
            "queued card releases with true once the edit/replace flush finishes — it lands as a trailing notice, never interleaved"
        );
    }

    /// P1-2 residual: in the edit/replace path the FIRST chunk is delivered via
    /// `edit_channel_message`, and only the continuation loop bumps progress.
    /// On a multi-chunk answer there is a real gap between acquiring the guard
    /// (which seeds `last_progress`) and the first continuation send: the first
    /// edit itself takes time (rate-limit wait + HTTP round-trip). If that gap
    /// exceeds the waiter's inactivity grace, the queued-card waiter would
    /// spuriously expire mid-flush. The fix bumps `note_progress` right after the
    /// first edit succeeds (multi-chunk path only). This test models that bridge:
    /// guard acquired, then a delay LONGER than the inactivity grace, then the
    /// first-edit progress bump — the waiter must NOT have expired across the gap.
    #[tokio::test]
    async fn first_edit_progress_bump_bridges_gap_before_first_continuation() {
        let barrier = Arc::new(AnswerFlushBarrier::default());
        let channel = ChannelId::new(11);

        // begin_flush seeds last_progress at acquire time.
        let guard = barrier.begin_flush(channel);

        let barrier_for_card = barrier.clone();
        // Short 100ms inactivity grace so the first-edit gap can exceed it.
        let queued_card = tokio::spawn(async move {
            barrier_for_card
                .wait_for_flush(channel, Duration::from_millis(100), Duration::from_secs(30))
                .await
        });

        // The first edit is in flight for ~80ms (under grace so far)...
        tokio::time::sleep(Duration::from_millis(80)).await;
        // ...the first edit SUCCEEDS — this is the new multi-chunk bump that the
        // residual fix adds. Without it, the next progress signal would only come
        // from the first continuation send, and the cumulative gap below would
        // trip the 100ms inactivity grace.
        barrier.note_progress(channel);

        // The first continuation is then itself in flight for another ~80ms. The
        // total elapsed (≈160ms) now exceeds the 100ms grace, but because the
        // first-edit bump reset the window the waiter must still be blocked.
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(
            !queued_card.is_finished(),
            "first-edit progress bump must keep the queued-card waiter blocked across the edit→continuation gap (multi-chunk path)"
        );

        // First continuation lands (continuation-loop bump), then the answer ends.
        barrier.note_progress(channel);
        drop(guard);
        assert!(
            queued_card.await.unwrap(),
            "queued card releases with true once the multi-chunk edit/replace answer finishes — never cut short mid-flush"
        );
    }
}
