//! #4380: crash-restart relay-resume guard.
//!
//! When dcserver **crashes** (no graceful drain, so no `restart_report` and the
//! inflight row keeps `restart_mode == None`) while a real user's turn is inflight,
//! recovery re-adopts the row and spawns a recovery watcher. The bridge that had
//! been streaming that turn died mid-stream **without** a Watcher handoff, so the
//! row still carries `relay_owner_kind == None` (bridge-owned / default).
//!
//! The watcher-yield gate (`tmux::watcher_should_yield_to_inflight_state`) then
//! yields a `None`-owner in-range turn to the "active bridge" — but that bridge no
//! longer exists. Before #4380 the gate's escape hatch only exempted **planned**
//! restarts (`restart_mode.is_some()`), so a crash re-adopt yielded to the dead
//! bridge and **black-holed 100% of the turn's remaining output** with zero
//! observability (the recurring `.stuck-manual-*` hand-recovery).
//!
//! Root cause / fix: the crash re-adopt path stamps `readopted_from_inflight` on
//! the row **before** the recovery watcher spawns (`mark_readopted_from_inflight`),
//! which is the durable "the bridge is gone; the recovered watcher owns relay
//! resumption" signal. [`crash_readopt_live_relay_resume_required`] lets the yield
//! gate honour it exactly like the planned-restart hatch. The scout's
//! `response_sent_offset > full_response.len()` hypothesis was a coordinate-system
//! misdiagnosis (`last_offset` transcript-bytes vs `full_response.len()`
//! text-chars) — the seed always clamps the offset, so it never zeroes the delta.
//!
//! Backstop (NOT a substitute for the fix): if the marker write did not durably
//! persist (`IoError`) the recovered watcher still yields, so
//! [`guard_readopt_relay_resume_or_dead_letter`] dead-letters the undelivered body
//! (`KIND_READOPT_RELAY_STUCK`) with a WARN, turning a silent 30-minute wedge into
//! an observable, recoverable row.

use std::sync::Arc;

use poise::serenity_prelude::ChannelId;
use sqlx::PgPool;

use super::SharedData;
use super::inflight::{InflightTurnState, RelayOwnerKind};
use crate::services::provider::ProviderKind;

/// The structural shape of a re-adopted **real-user** bridge turn that is still
/// live (uncommitted) and whose relay owner is still the (now dead) bridge —
/// independent of whether the `readopted_from_inflight` marker durably persisted.
///
/// Excludes rebind-origin rows (owned by the rebind API), committed turns (already
/// delivered → a watcher relay would be a duplicate), the TUI-direct synthetic
/// owner and id-0 rows (not real-user turns), and any row whose relay is already
/// owned by a live path (`Watcher` / `StandbyRelay` / `SessionBoundRelay`). Only a
/// `None` (bridge-owned/default) owner reaches the yield-gate escape hatch, so this
/// is exactly the population at risk of the #4380 black-hole.
pub(in crate::services::discord) fn crash_readopt_real_user_live_turn(
    state: &InflightTurnState,
) -> bool {
    !state.rebind_origin
        && !state.terminal_delivery_committed
        && state.request_owner_user_id != 0
        && state.request_owner_user_id
            != crate::services::discord::tui_prompt_relay::TUI_DIRECT_SYNTHETIC_OWNER_USER_ID
        && state.effective_relay_owner_kind() == RelayOwnerKind::None
}

/// True for a still-live, real-user bridge turn that a **crash** restart re-adopted
/// from its on-disk inflight row (`readopted_from_inflight`), whose relay the
/// recovered watcher MUST resume rather than yield to the now-dead bridge (#4380).
///
/// This is the root-fix predicate consumed by `watcher_should_yield_to_inflight_state`:
/// when it returns `true` the gate must NOT yield.
pub(in crate::services::discord) fn crash_readopt_live_relay_resume_required(
    state: &InflightTurnState,
) -> bool {
    state.readopted_from_inflight && crash_readopt_real_user_live_turn(state)
}

/// #4380 backstop: WARN + durable dead-letter for a re-adopted real-user live turn
/// whose relay-resume guard could NOT be armed (the `readopted_from_inflight`
/// marker did not durably persist), so the recovered watcher will yield to the dead
/// bridge and silently drop the remaining output. Fire-and-forget: the DLQ insert
/// rides a detached task (`record_detached`), never blocking recovery.
pub(in crate::services::discord) fn record_readopt_relay_black_hole_dead_letter(
    pool: Option<&PgPool>,
    channel_id: ChannelId,
    state: &InflightTurnState,
    reason: &str,
) {
    // The undelivered body is `full_response[response_sent_offset..]`; fall back to
    // the whole body if the (clamped) offset is somehow out of bounds so the DLQ
    // never loses content to a slice panic.
    let undelivered = state
        .full_response
        .get(state.response_sent_offset..)
        .unwrap_or(state.full_response.as_str());
    tracing::warn!(
        channel_id = channel_id.get(),
        request_owner_user_id = state.request_owner_user_id,
        user_msg_id = state.user_msg_id,
        response_sent_offset = state.response_sent_offset,
        full_response_len = state.full_response.len(),
        reason,
        "[#4380] re-adopted live turn relay could not resume; dead-lettering undelivered output to end the silent loss"
    );
    crate::db::relay_dead_letter::record_detached(
        pool,
        crate::db::relay_dead_letter::RelayDeadLetterRecord {
            kind: crate::db::relay_dead_letter::KIND_READOPT_RELAY_STUCK.to_string(),
            channel_id: channel_id.to_string(),
            author_id: Some(state.request_owner_user_id.to_string()),
            message_id: (state.current_msg_id != 0).then(|| state.current_msg_id.to_string()),
            content: undelivered.to_string(),
            reason: reason.to_string(),
        },
    );
}

/// #4380 backstop entry point, called from the crash-recovery re-adopt path right
/// after `reregister_active_turn_from_inflight` (which stamps the marker). Reloads
/// the durable row and, iff it is still an at-risk re-adopted real-user live turn
/// that LACKS the `readopted_from_inflight` marker (marker write failed), records a
/// dead letter. On the normal path the marker is present, so this is a no-op.
pub(in crate::services::discord) fn guard_readopt_relay_resume_or_dead_letter(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
) {
    let Some(reloaded) = super::inflight::load_inflight_state(provider, channel_id.get()) else {
        return;
    };
    if crash_readopt_real_user_live_turn(&reloaded) && !reloaded.readopted_from_inflight {
        record_readopt_relay_black_hole_dead_letter(
            shared.pg_pool.as_ref(),
            channel_id,
            &reloaded,
            "readopted_from_inflight marker did not persist; recovered watcher will yield to the dead bridge (#4380)",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::agent_protocol::RuntimeHandoffKind;

    const REAL_OWNER: u64 = 343_742_347_365_974_026;

    /// Build the #4380 stuck-row shape under an isolated `AGENTDESK_ROOT_DIR`
    /// tempdir (`InflightTurnState::new` resolves the runtime generation from the
    /// root and asserts a test never touches the live release store) and run the
    /// pure-predicate assertion while the env guard is held. Mirrors the
    /// `active_bridge_turn_guard_tests` helper in `tmux.rs`.
    fn with_readopted_crash_turn(test: impl FnOnce(InflightTurnState)) {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let tmp = tempfile::tempdir().expect("tempdir");
        let previous = std::env::var_os("AGENTDESK_ROOT_DIR");
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", tmp.path()) };

        let mut state = InflightTurnState::new(
            ProviderKind::Claude,
            1_479_671_301_387_059_200,
            Some("adk-cc".to_string()),
            REAL_OWNER,
            1_520_972_895_491_325_952,
            1_520_975_526_431_424_663,
            "diagnose relay".to_string(),
            Some("019f10e3-3dad-73c2-9d8c-e6188e4ccc7c".to_string()),
            Some("AgentDesk-claude-adk-cc".to_string()),
            Some("/tmp/claude-transcript.jsonl".to_string()),
            None,
            12_837,
        );
        state.runtime_kind = Some(RuntimeHandoffKind::ClaudeTui);
        state.turn_start_offset = Some(0);
        state.full_response = "partial answer that never finished streaming".to_string();
        state.response_sent_offset = state.full_response.len();
        // Crash: no planned drain, bridge-owned relay, re-adopted from inflight.
        state.set_relay_owner_kind(RelayOwnerKind::None);
        state.readopted_from_inflight = true;

        test(state);

        match previous {
            Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
            None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
        }
    }

    #[test]
    fn crash_readopt_real_user_live_turn_matches_the_black_hole_shape() {
        with_readopted_crash_turn(|state| {
            assert!(crash_readopt_real_user_live_turn(&state));
        });
    }

    #[test]
    fn resume_required_holds_for_the_readopted_crash_turn() {
        with_readopted_crash_turn(|state| {
            assert!(crash_readopt_live_relay_resume_required(&state));
        });
    }

    #[test]
    fn resume_required_needs_the_readopted_marker() {
        with_readopted_crash_turn(|mut state| {
            state.readopted_from_inflight = false;
            assert!(
                !crash_readopt_live_relay_resume_required(&state),
                "without the marker the yield gate must fall back to the planned-restart hatch"
            );
            // …but the row is still the at-risk shape → the DLQ backstop must fire.
            assert!(crash_readopt_real_user_live_turn(&state));
        });
    }

    #[test]
    fn committed_turn_is_not_a_black_hole_risk() {
        with_readopted_crash_turn(|mut state| {
            state.terminal_delivery_committed = true;
            assert!(
                !crash_readopt_real_user_live_turn(&state),
                "an already-delivered turn's watcher relay would be a duplicate, not a loss"
            );
            assert!(!crash_readopt_live_relay_resume_required(&state));
        });
    }

    #[test]
    fn watcher_owned_turn_is_not_a_black_hole_risk() {
        with_readopted_crash_turn(|mut state| {
            state.set_relay_owner_kind(RelayOwnerKind::Watcher);
            assert!(
                !crash_readopt_real_user_live_turn(&state),
                "a watcher-owned turn already resumes relay; the None-owner escape hatch must not touch it"
            );
        });
    }

    #[test]
    fn session_bound_relay_turn_is_not_a_black_hole_risk() {
        with_readopted_crash_turn(|mut state| {
            state.set_relay_owner_kind(RelayOwnerKind::SessionBoundRelay);
            assert!(!crash_readopt_real_user_live_turn(&state));
            assert!(!crash_readopt_live_relay_resume_required(&state));
        });
    }

    #[test]
    fn synthetic_owner_turn_is_not_a_real_user_black_hole() {
        with_readopted_crash_turn(|mut state| {
            state.request_owner_user_id =
                crate::services::discord::tui_prompt_relay::TUI_DIRECT_SYNTHETIC_OWNER_USER_ID;
            assert!(!crash_readopt_real_user_live_turn(&state));
        });
    }

    #[test]
    fn rebind_origin_turn_is_owned_by_the_rebind_api() {
        with_readopted_crash_turn(|mut state| {
            state.rebind_origin = true;
            assert!(!crash_readopt_real_user_live_turn(&state));
        });
    }
}
