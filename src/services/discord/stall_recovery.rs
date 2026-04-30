//! #1446 stall-deadlock recovery — shared post-clear bookkeeping.
//!
//! The THREAD-GUARD's stale-thread cleanup (`router::intake_gate`) and the
//! stall watchdog's force-clean path (`health::run_stall_watchdog_pass`)
//! both call `mailbox_clear_channel` on a thread/channel whose original
//! turn task has already died. `mailbox_clear_channel` returns the
//! orphaned `cancel_token` in `ClearChannelResult.removed_token`, but the
//! normal turn-finish lifecycle (`finalize_turn_state` →
//! `cancel_active_token` → `global_active.fetch_sub(1)`) was never run,
//! so without this helper:
//!   - `global_active` stays > 0 forever, blocking deferred-restart
//!     drain (`/api/restart-deferred`) and confusing health-status
//!     reporters that key off active-turn count;
//!   - any leftover child process / tmux session attached to the orphaned
//!     token keeps running outside the mailbox where no watchdog can
//!     reach it.
//!
//! `finalize_orphaned_clear` mirrors the
//! `placeholder_sweeper::finalize_abandoned_mailbox` cleanup pattern so
//! both stall-recovery layers honour the same global-counter invariants
//! as every other turn-end path in the system.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use poise::serenity_prelude as serenity;

use super::SharedData;
use crate::services::provider::CancelToken;

/// Finalize the bookkeeping that `mailbox_clear_channel` does **not**
/// perform on its own when the original turn task is already dead.
///
/// Specifically, when `removed_token` is `Some(token)`:
///   1. Cancel the orphaned `CancelToken` so any leftover child process
///      / tmux session keyed off it is torn down (mirrors what the
///      normal `finish_turn` → `cancel_active_token` path would have
///      done). We use `CleanupSession` policy so the tmux session is
///      removed too, matching the stall-recovery contract that no
///      orphaned tmux outlives a force-clean.
///   2. Decrement `global_active` so deferred-restart drains and health
///      reporters do not see a phantom active turn.
///
/// `removed_token == None` means the mailbox was already idle — no
/// bookkeeping is required and the helper is a no-op.
///
/// `reason` is purely diagnostic; it is passed through to
/// `cancel_active_token` for log attribution.
pub(super) fn finalize_orphaned_clear(
    shared: &Arc<SharedData>,
    channel_id: serenity::ChannelId,
    removed_token: Option<Arc<CancelToken>>,
    reason: &'static str,
) {
    let Some(token) = removed_token else {
        return;
    };
    super::turn_bridge::cancel_active_token(
        &token,
        super::TmuxCleanupPolicy::CleanupSession {
            termination_reason_code: Some(reason),
        },
        reason,
    );
    shared.global_active.fetch_sub(1, Ordering::Relaxed);
    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!(
        "  [{ts}] 🔄 stall-recovery: finalized orphaned clear for channel {} (reason={})",
        channel_id,
        reason
    );
}
