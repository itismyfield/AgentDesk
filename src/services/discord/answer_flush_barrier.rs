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
//! * The waiter (`wait_for_flush`) polls with a hard deadline. If the flush is
//!   stuck or the turn errored without dropping the guard (impossible given the
//!   guard, but defended anyway), the waiter proceeds once the deadline elapses
//!   and the queued card still posts. No deadlock, no permanent suppression.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use poise::serenity_prelude::ChannelId;

/// Maximum time the queued-card POST path will defer behind an in-flight
/// multi-chunk answer flush before proceeding anyway. Generous enough to cover
/// the inter-chunk 500ms sleeps of a realistic multi-chunk answer, but bounded
/// so a stuck flush never strands the queued card.
pub(super) const ANSWER_FLUSH_WAIT_TIMEOUT: Duration = Duration::from_secs(8);

/// Poll interval while waiting for the gate to clear.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Default)]
pub(in crate::services::discord) struct AnswerFlushBarrier {
    /// Per-channel count of in-flight multi-chunk answer flushes. A channel is
    /// "flushing" while its count is > 0. A count (not a bool) tolerates the
    /// rare overlap of two multi-chunk sends on the same channel without one
    /// clearing the other's gate.
    inflight: Mutex<HashMap<ChannelId, usize>>,
}

impl AnswerFlushBarrier {
    /// Mark the start of a multi-chunk answer flush for `channel_id`. The
    /// returned guard clears the mark on drop. Call this ONLY for genuine
    /// multi-chunk (>1 chunk) sends — single-chunk answers cannot be split, so
    /// there is no interleaving window to guard.
    pub(in crate::services::discord) fn begin_flush(
        self: &std::sync::Arc<Self>,
        channel_id: ChannelId,
    ) -> AnswerFlushGuard {
        if let Ok(mut map) = self.inflight.lock() {
            *map.entry(channel_id).or_insert(0) += 1;
        }
        AnswerFlushGuard {
            barrier: self.clone(),
            channel_id,
        }
    }

    fn is_flushing(&self, channel_id: ChannelId) -> bool {
        self.inflight
            .lock()
            .map(|map| map.get(&channel_id).copied().unwrap_or(0) > 0)
            .unwrap_or(false)
    }

    fn end_flush(&self, channel_id: ChannelId) {
        if let Ok(mut map) = self.inflight.lock() {
            if let Some(count) = map.get_mut(&channel_id) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    map.remove(&channel_id);
                }
            }
        }
    }

    /// Wait until no multi-chunk answer flush is in flight for `channel_id`, or
    /// until `timeout` elapses — whichever comes first. Returns `true` if the
    /// gate cleared before the deadline, `false` if it timed out (caller should
    /// proceed anyway). Always returns promptly when no flush is active.
    pub(in crate::services::discord) async fn wait_for_flush(
        &self,
        channel_id: ChannelId,
        timeout: Duration,
    ) -> bool {
        if !self.is_flushing(channel_id) {
            return true;
        }
        let deadline = Instant::now() + timeout;
        loop {
            tokio::time::sleep(POLL_INTERVAL).await;
            if !self.is_flushing(channel_id) {
                return true;
            }
            if Instant::now() >= deadline {
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

    #[tokio::test]
    async fn no_flush_returns_immediately() {
        let barrier = Arc::new(AnswerFlushBarrier::default());
        let channel = ChannelId::new(1);
        let start = Instant::now();
        assert!(
            barrier
                .wait_for_flush(channel, Duration::from_secs(5))
                .await,
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
            barrier
                .wait_for_flush(channel, Duration::from_secs(5))
                .await,
            "waiter sees cleared gate after guard drop"
        );
    }

    #[tokio::test]
    async fn waiter_unblocks_when_flush_ends() {
        let barrier = Arc::new(AnswerFlushBarrier::default());
        let channel = ChannelId::new(3);
        let guard = barrier.begin_flush(channel);
        let barrier_for_wait = barrier.clone();
        let waiter = tokio::spawn(async move {
            barrier_for_wait
                .wait_for_flush(channel, Duration::from_secs(5))
                .await
        });
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
        // flush that never dropped its guard. The waiter MUST still return
        // (false) so the queued card is never permanently suppressed.
        let guard = barrier.begin_flush(channel);
        std::mem::forget(guard);
        let cleared = barrier
            .wait_for_flush(channel, Duration::from_millis(200))
            .await;
        assert!(
            !cleared,
            "a never-ending flush must time out (false), not deadlock"
        );
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
}
