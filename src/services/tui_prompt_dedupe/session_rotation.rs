//! #5188: the Claude SESSION-ROTATION ledger.
//!
//! `/clear` (and any other continuation cutover) makes Claude Code open a brand
//! new transcript JSONL and stop writing to the current one. The hook payload is
//! the first place AgentDesk learns about it — `adopt_claude_continuation_session`
//! rebinds the in-memory [`super::TuiRuntimeBinding`] there.
//!
//! Rebinding the mirror is necessary but NOT sufficient. **The inflight pinned to
//! the frozen transcript** can never receive a terminal — nothing will ever append
//! to the file it is waiting on — so every later turn on the channel reads
//! `FOREIGN prior inflight is still live` and aborts. This ledger carries the
//! rotation to the per-tick settle pass
//! (`discord::tui_prompt_relay::session_rotation_settle`) that resolves it.
//!
//! The record deliberately keeps the FIRST observed `old_output_path`: repeated
//! hooks may report further hops, but the delivery-critical fact is which
//! transcript may still hold undelivered bytes.
//!
//! This ledger is in-memory only. A dcserver restart re-derives the binding from
//! persisted artifacts (`persist_claude_continuation_session` rewrites them at
//! adoption time), so a lost record cannot strand delivery across a restart.
//!
//! ## Scope: this ledger is pending WORK, not a standing authority
//! It answers "is there rotation cleanup still owed for this pane?", and the
//! settle pass retires the record as soon as the answer is no — typically on the
//! first ~500ms tick after the rotation.
//!
//! That makes it the wrong home for the OTHER half of #5188, the launch-script
//! rehydration authority: that consumer runs on a 5s tick and needs an answer
//! that stays true for the life of the pane, so reading it out of this
//! short-lived record would give a signal that is almost always already gone.
//! The authority therefore lives in its own pane-lifetime store, added
//! separately; nothing here may be repurposed to serve it.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

/// Rotation records are pruned after this long. Generous relative to the ~500ms
/// idle poll that consumes them: a record normally lives for one or two ticks,
/// and this bound only exists so a pane whose owner channel never resolves
/// cannot leak an entry for the lifetime of the process.
const ROTATION_RECORD_TTL: Duration = Duration::from_secs(12 * 60 * 60);

/// One observed Claude session rotation for a tmux pane.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ClaudeSessionRotation {
    pub tmux_session_name: String,
    /// Session id the pane was bound to before the FIRST unsettled rotation.
    pub old_session_id: Option<String>,
    /// Transcript that stopped growing. May still hold undelivered bytes.
    pub old_output_path: String,
    /// Delivery cursor into `old_output_path` at the instant of rotation.
    pub old_last_offset: u64,
    /// Session id reported by the live hook payload (the newest hop).
    pub new_session_id: String,
    /// Transcript Claude is writing to now.
    pub new_output_path: String,
    /// Highest delivered frontier observed into `old_output_path` while waiting
    /// for the pre-rotation tail to drain.
    pub observed_drain_frontier: u64,
    /// Consecutive drain observations in which `observed_drain_frontier` did not
    /// advance. Bounds the "deliver the old transcript first" wait so a tail that
    /// is genuinely dead cannot hold the channel forever.
    pub polls_without_drain_progress: u32,
}

struct TimedRotation {
    rotation: ClaudeSessionRotation,
    recorded_at: Instant,
}

static ROTATIONS: LazyLock<Mutex<HashMap<String, TimedRotation>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn lock_rotations() -> std::sync::MutexGuard<'static, HashMap<String, TimedRotation>> {
    let mut guard = ROTATIONS.lock().unwrap_or_else(|error| error.into_inner());
    guard.retain(|_, entry| entry.recorded_at.elapsed() < ROTATION_RECORD_TTL);
    guard
}

/// Record a rotation observed at binding-adoption time.
///
/// Idempotent per pane in the delivery-critical direction: a second hop updates
/// the NEW side (`new_session_id` / `new_output_path`) but preserves the ORIGINAL
/// `old_output_path`, `old_last_offset` and drain bookkeeping, because that is
/// the transcript whose undelivered tail must still be drained. A hook that
/// merely re-reports the rotation already recorded is a no-op.
pub(crate) fn record_claude_session_rotation(rotation: ClaudeSessionRotation) {
    let mut rotations = lock_rotations();
    match rotations.get_mut(&rotation.tmux_session_name) {
        Some(existing) => {
            existing.rotation.new_session_id = rotation.new_session_id;
            existing.rotation.new_output_path = rotation.new_output_path;
            existing.recorded_at = Instant::now();
        }
        None => {
            rotations.insert(
                rotation.tmux_session_name.clone(),
                TimedRotation {
                    rotation,
                    recorded_at: Instant::now(),
                },
            );
        }
    }
}

/// The unsettled rotation for `tmux_session_name`, if any.
pub(crate) fn claude_session_rotation_for_tmux(
    tmux_session_name: &str,
) -> Option<ClaudeSessionRotation> {
    lock_rotations()
        .get(tmux_session_name.trim())
        .map(|entry| entry.rotation.clone())
}

/// Every unsettled rotation. Used by the per-tick Discord-side settle pass.
pub(crate) fn pending_claude_session_rotations() -> Vec<ClaudeSessionRotation> {
    lock_rotations()
        .values()
        .map(|entry| entry.rotation.clone())
        .collect()
}

/// Fold a fresh drain observation into the record and return the resulting
/// `polls_without_drain_progress`. A frontier that advanced resets the counter to
/// zero; a frontier that stood still increments it.
pub(crate) fn record_rotation_drain_progress(tmux_session_name: &str, frontier: u64) -> u32 {
    let mut rotations = lock_rotations();
    let Some(entry) = rotations.get_mut(tmux_session_name.trim()) else {
        return 0;
    };
    if frontier > entry.rotation.observed_drain_frontier {
        entry.rotation.observed_drain_frontier = frontier;
        entry.rotation.polls_without_drain_progress = 0;
    } else {
        entry.rotation.polls_without_drain_progress = entry
            .rotation
            .polls_without_drain_progress
            .saturating_add(1);
    }
    entry.rotation.polls_without_drain_progress
}

/// Drop the record once the rotation has been fully propagated (stale inflight
/// settled, delivery rebound). The pane keeps its adopted binding; only the
/// pending-work marker goes away.
pub(crate) fn clear_claude_session_rotation(tmux_session_name: &str) -> bool {
    lock_rotations().remove(tmux_session_name.trim()).is_some()
}

/// Serializes every test that touches the process-wide ledger above.
///
/// Module-scoped (not buried inside `mod tests`) so tests in OTHER modules that
/// drive the ledger the way production does can take the same lock instead of
/// clearing each other's fixtures.
#[cfg(test)]
static ROTATION_TEST_SERIAL: Mutex<()> = Mutex::new(());

/// Take the shared lock and start from an empty ledger.
#[cfg(test)]
pub(crate) fn lock_claude_session_rotations_for_tests() -> std::sync::MutexGuard<'static, ()> {
    let guard = ROTATION_TEST_SERIAL
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    reset_claude_session_rotations_for_tests();
    guard
}

#[cfg(test)]
pub(crate) fn reset_claude_session_rotations_for_tests() {
    ROTATIONS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The stores are process-wide statics, so these tests must not interleave —
    /// each one resets them. `cargo test` runs them on separate threads by
    /// default, and cross-module tests share this same lock.
    fn serial() -> std::sync::MutexGuard<'static, ()> {
        lock_claude_session_rotations_for_tests()
    }

    fn rotation(tmux: &str, old_path: &str, new_path: &str) -> ClaudeSessionRotation {
        ClaudeSessionRotation {
            tmux_session_name: tmux.to_string(),
            old_session_id: Some("old-uuid".to_string()),
            old_output_path: old_path.to_string(),
            old_last_offset: 10,
            new_session_id: "new-uuid".to_string(),
            new_output_path: new_path.to_string(),
            observed_drain_frontier: 0,
            polls_without_drain_progress: 0,
        }
    }

    #[test]
    fn second_hop_preserves_the_original_frozen_transcript() {
        let _serial = serial();
        record_claude_session_rotation(rotation("pane", "/tmp/a.jsonl", "/tmp/b.jsonl"));
        let mut second = rotation("pane", "/tmp/b.jsonl", "/tmp/c.jsonl");
        second.new_session_id = "third-uuid".to_string();
        record_claude_session_rotation(second);

        let stored = claude_session_rotation_for_tmux("pane").expect("record retained");
        assert_eq!(
            stored.old_output_path, "/tmp/a.jsonl",
            "the FIRST frozen transcript is the one that may still owe bytes; a \
             later hop must not repoint the drain target"
        );
        assert_eq!(stored.new_output_path, "/tmp/c.jsonl");
        assert_eq!(
            stored.new_session_id, "third-uuid",
            "the NEW side still tracks the newest hop"
        );
    }

    #[test]
    fn drain_progress_resets_the_stall_counter_and_stall_accumulates() {
        let _serial = serial();
        record_claude_session_rotation(rotation("pane", "/tmp/a.jsonl", "/tmp/b.jsonl"));

        assert_eq!(record_rotation_drain_progress("pane", 0), 1);
        assert_eq!(record_rotation_drain_progress("pane", 0), 2);
        assert_eq!(
            record_rotation_drain_progress("pane", 64),
            0,
            "an advancing frontier means the pre-rotation tail is still draining"
        );
        assert_eq!(record_rotation_drain_progress("pane", 64), 1);
        assert!(clear_claude_session_rotation("pane"));
        assert!(claude_session_rotation_for_tmux("pane").is_none());
    }
}
