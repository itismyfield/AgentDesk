//! #5188: the Claude SESSION-ROTATION ledger.
//!
//! `/clear` (and any other continuation cutover) makes Claude Code open a brand
//! new transcript JSONL and stop writing to the current one. The hook payload is
//! the first place AgentDesk learns about it — `adopt_claude_continuation_session`
//! rebinds the in-memory [`super::TuiRuntimeBinding`] there.
//!
//! Rebinding the mirror is necessary but NOT sufficient, and this ledger is what
//! carries the event to the two places that also have to react:
//!
//! 1. **The launch-script rehydration pass** (`tui_prompt_relay::rehydration`)
//!    re-derives a binding from the on-disk launch script every few seconds. That
//!    artifact still names the LAUNCH-time UUID, so without an explicit record of
//!    "this session id came from a live hook payload" the pass happily overwrote
//!    the freshly adopted binding back to the FROZEN transcript. That is the
//!    observed production signature: `adopted Claude continuation session …`
//!    followed by `rehydrated Claude TUI direct relay binding from launch script
//!    … transcript_path=<old>.jsonl`, and delivery never followed the rotation.
//!
//! 2. **The inflight pinned to the frozen transcript** can never receive a
//!    terminal — nothing will ever append to the file it is waiting on — so every
//!    later turn on the channel reads `FOREIGN prior inflight is still live` and
//!    aborts. It has to be settled deliberately at the rotation boundary
//!    (`discord::claude_session_rotation`).
//!
//! The record deliberately keeps the FIRST observed `old_output_path`: repeated
//! hooks may report further hops, but the delivery-critical fact is which
//! transcript may still hold undelivered bytes.
//!
//! This ledger is in-memory only. A dcserver restart re-derives the binding from
//! persisted artifacts (`persist_claude_continuation_session` rewrites them at
//! adoption time), so a lost record cannot strand delivery across a restart.
//!
//! ## Two stores, two lifetimes — and why that separation is load-bearing
//! The two consumers above want the SAME event but on OPPOSITE schedules, so
//! they cannot share one record:
//!
//! * consumer 2 (settle) is *pending work*. It runs on the ~500ms idle tick and
//!   must retire its record the moment the work is done, or it would re-settle
//!   forever.
//! * consumer 1 (rehydration) is a *standing authority*. It runs on the 5s
//!   rehydrate tick and must keep out-ranking the launch script for as long as
//!   the pane keeps that adopted session — which is the rest of the pane's life,
//!   not the few hundred milliseconds the settle work takes.
//!
//! An earlier revision of this module served consumer 1 out of the settle ledger.
//! That made the authority signal self-destruct: the settle pass reached
//! `RebindOnly` on its very first tick (a `/clear` creates no inflight to drain),
//! called [`clear_claude_session_rotation`], and the adopted id vanished ~500ms
//! after adoption — normally BEFORE the 5s rehydration pass ever read it. The
//! signal was therefore present for about one tick in ten and absent forever
//! after, so rehydration reverted the binding to the frozen launch-script
//! transcript in exactly the failure mode it was written to prevent.
//!
//! So the adopted session id lives in its OWN pane-lifetime store
//! ([`ADOPTED_SESSIONS`]) that the settle path never touches. Its availability no
//! longer depends on which of the two polls wins a race, because the pass that
//! used to destroy it — the ~500ms settle tick — can no longer reach it.
//!
//! To be precise about lifetime, the record leaves the store by exactly three
//! paths, and none of them fires while the pane is alive and still running the
//! adopted session:
//!
//! * a newer adoption for the same pane OVERWRITES the entry. That is the
//!   authority being restated, not lost.
//! * [`forget_hook_adopted_claude_session_id`] removes it. Its only caller is
//!   the 5s rehydrate pass, so this IS timer-driven — but it is gated on that
//!   pass having confirmed the pane DEAD/orphaned, and a dead pane has no
//!   delivery left to protect.
//! * the [`ROTATION_RECORD_TTL`] `retain` in `lock_adopted_sessions` prunes it.
//!   That runs on EVERY access rather than on a timer, but only evicts entries
//!   older than 12h; see that constant for why reaching it is harmless.
//!
//! (see [`hook_adopted_claude_session_id`] for how the authority is read)

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

/// Records in BOTH stores are pruned after this long.
///
/// For [`ROTATIONS`] the bound is nearly always moot — the settle pass retires a
/// record within a poll or two — and exists only so a pane whose owner channel
/// never resolves cannot leak an entry for the life of the process. For
/// [`ADOPTED_SESSIONS`] it is a backstop behind the real retirement paths (a
/// newer adoption overwrites the entry; confirmed pane death forgets it).
///
/// Reaching this bound is harmless — but NOT because "the ordinary launch-script
/// comparison takes over". That comparison reverting the binding to the FROZEN
/// transcript is the very bug this store exists to stop, so it cannot also be
/// the safety net. It is harmless because adoption is made DURABLE at the same
/// instant the authority is recorded: the hook guard that detects the rotation
/// (`claude_tui::hook_server`, the `command_session_id != payload_session_id`
/// branch) calls `persist_claude_continuation_session`, which rewrites the
/// on-disk launch script to name the adopted session. Long before 12h elapse the
/// launch script and the adopted binding already agree, so losing the in-memory
/// authority at that point changes nothing.
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

struct TimedAdoption {
    session_id: String,
    recorded_at: Instant,
}

/// Session ids adopted from live hook payloads, keyed by tmux session name.
///
/// Deliberately NOT part of [`ROTATIONS`]: see the module doc. This store is
/// pane-scoped and is never emptied by the settle pass, so its contents do not
/// depend on poll ordering.
static ADOPTED_SESSIONS: LazyLock<Mutex<HashMap<String, TimedAdoption>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn lock_adopted_sessions() -> std::sync::MutexGuard<'static, HashMap<String, TimedAdoption>> {
    let mut guard = ADOPTED_SESSIONS
        .lock()
        .unwrap_or_else(|error| error.into_inner());
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
    // The adopted-session authority is recorded from here too so the rotation
    // path cannot forget it, but it is stored SEPARATELY and outlives this
    // record — `clear_claude_session_rotation` must not take it down with the
    // pending-work marker (see the module doc).
    record_hook_adopted_claude_session_id(&rotation.tmux_session_name, &rotation.new_session_id);
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

/// Record the session id a live hook payload just reported for this pane.
///
/// Called on EVERY adoption-path hook, including the "already adopted" one that
/// records no rotation, so the authority signal is refreshed rather than written
/// exactly once per rotation.
pub(crate) fn record_hook_adopted_claude_session_id(tmux_session_name: &str, session_id: &str) {
    let tmux_session_name = tmux_session_name.trim();
    let session_id = session_id.trim();
    if tmux_session_name.is_empty() || session_id.is_empty() {
        return;
    }
    lock_adopted_sessions().insert(
        tmux_session_name.to_string(),
        TimedAdoption {
            session_id: session_id.to_string(),
            recorded_at: Instant::now(),
        },
    );
}

/// The session id most recently ADOPTED from a live hook payload for this pane.
///
/// This is the R1 authority signal: a launch script on disk is a LAUNCH-time
/// artifact, while this id came from the running Claude process itself, so a
/// binding carrying it must not be reverted to the launch script's stale UUID.
///
/// Read from the pane-lifetime [`ADOPTED_SESSIONS`] store, NOT from the settle
/// ledger — the settle ledger is retired within ~500ms of adoption while this
/// answer must stay true for the 5s rehydration pass and every one after it.
///
/// It needs no explicit retirement: the predicate built on it
/// (`hook_adopted_binding_supersedes_launch`) also requires that the LAUNCH
/// binding does not already carry this id, so once
/// `persist_claude_continuation_session` has rewritten the launch script the
/// signal stops applying on its own and the ordinary match-launch path wins
/// again.
pub(crate) fn hook_adopted_claude_session_id(tmux_session_name: &str) -> Option<String> {
    lock_adopted_sessions()
        .get(tmux_session_name.trim())
        .map(|entry| entry.session_id.clone())
}

/// Forget the adopted-session authority for a pane that is being torn down.
///
/// Only for genuine pane death / mirror eviction. The settle path must NEVER
/// call this — that coupling is the exact defect the two-store split removes.
pub(crate) fn forget_hook_adopted_claude_session_id(tmux_session_name: &str) -> bool {
    lock_adopted_sessions()
        .remove(tmux_session_name.trim())
        .is_some()
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
///
/// Touches [`ROTATIONS`] ONLY. The adopted-session authority in
/// [`ADOPTED_SESSIONS`] deliberately survives: this function runs on the ~500ms
/// settle tick, and taking the authority down with it is what previously left
/// the 5s rehydration pass with nothing to out-rank the launch script.
pub(crate) fn clear_claude_session_rotation(tmux_session_name: &str) -> bool {
    lock_rotations().remove(tmux_session_name.trim()).is_some()
}

/// Serializes every test that touches the two process-wide stores above.
///
/// Module-scoped (not buried inside `mod tests`) because the stores are read
/// from OTHER modules' tests too — notably the rehydration-gate predicates in
/// `discord::tui_prompt_relay::session_rotation_settle`, which look the adopted
/// id up exactly the way production does. Without one shared lock those tests
/// would clear each other's fixtures.
#[cfg(test)]
static ROTATION_TEST_SERIAL: Mutex<()> = Mutex::new(());

/// Take the shared lock and start from empty stores.
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
    ADOPTED_SESSIONS
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
            hook_adopted_claude_session_id("pane").as_deref(),
            Some("third-uuid"),
            "the adopted-session authority tracks the NEWEST hop"
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

    /// P1-A. The settle pass retires the rotation record on its FIRST ~500ms
    /// tick (a `/clear` leaves no inflight to drain, so the plan is
    /// `RebindOnly`), while the rehydration pass that consumes the adopted id
    /// only runs every 5s. If the two shared one record the authority signal
    /// would be gone before its reader ever woke up — present for roughly one
    /// tick in ten, absent forever after.
    #[test]
    fn settling_a_rotation_does_not_retire_the_adopted_session_authority() {
        let _serial = serial();
        record_claude_session_rotation(rotation("pane", "/tmp/a.jsonl", "/tmp/b.jsonl"));
        assert_eq!(
            hook_adopted_claude_session_id("pane").as_deref(),
            Some("new-uuid")
        );

        assert!(clear_claude_session_rotation("pane"));

        assert!(
            claude_session_rotation_for_tmux("pane").is_none(),
            "the pending-work marker is done and must not be re-settled"
        );
        assert_eq!(
            hook_adopted_claude_session_id("pane").as_deref(),
            Some("new-uuid"),
            "the adopted-session authority must OUTLIVE the settle record; it is \
             read by a slower poll and is what stops the launch script from \
             reverting delivery to the frozen transcript"
        );
    }

    /// The authority is refreshed by hooks that record no rotation at all (the
    /// already-adopted early return), so it does not depend on a rotation
    /// happening to be in flight.
    #[test]
    fn the_adopted_authority_is_writable_without_a_rotation_record() {
        let _serial = serial();
        record_hook_adopted_claude_session_id("pane", "hook-uuid");
        assert!(claude_session_rotation_for_tmux("pane").is_none());
        assert_eq!(
            hook_adopted_claude_session_id("pane").as_deref(),
            Some("hook-uuid")
        );

        record_hook_adopted_claude_session_id("pane", "  ");
        assert_eq!(
            hook_adopted_claude_session_id("pane").as_deref(),
            Some("hook-uuid"),
            "a blank payload id must not erase a real one"
        );

        assert!(forget_hook_adopted_claude_session_id("pane"));
        assert!(hook_adopted_claude_session_id("pane").is_none());
    }
}
