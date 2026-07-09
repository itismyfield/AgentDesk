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
//!
//! Lives under `recovery_engine` (a non-giant) so declaring it never re-inflates
//! the `discord/mod.rs` giant; the yield-gate predicate is re-exported at
//! `pub(in crate::services::discord)` for the `tmux.rs` call site.

use std::sync::Arc;

use poise::serenity_prelude::ChannelId;
use sqlx::PgPool;

use crate::services::discord::SharedData;
use crate::services::discord::inflight::{InflightTurnState, RelayOwnerKind};
use crate::services::provider::ProviderKind;

/// The structural shape of a re-adopted **real-user** bridge turn that a **crash**
/// left still live (uncommitted) and still bridge-owned (the now-dead bridge) —
/// independent of whether the `readopted_from_inflight` marker durably persisted.
///
/// This is the real-user, crash-only **subset** of the yield-gate black-hole set,
/// NOT an exact mirror: the gate itself does not check `rebind_origin` / owner id /
/// `user_msg_id` (it only inspects `relay_owner_kind`, the turn range, `restart_mode`
/// and `terminal_delivery_committed`). This predicate narrows further ON PURPOSE so
/// the backstop never dead-letters a row the gate actually resumes. The narrowing is
/// safe (never misses a genuine black-hole) because every row it drops is not one:
/// id-0 / owner-0 / synthetic-owner rows are not real-user turns (out of #4380 scope
/// — handled by the synthetic reclaim paths), `rebind_origin` rows are owned by the
/// rebind API, and planned-restart rows are resumed by the gate's own
/// `restart_mode.is_some()` hatch — none is a `None`-owner crash turn the recovered
/// watcher would silently drop.
///
/// Excludes, each closing a concrete false-positive:
///   - `rebind_origin` rows — owned by the rebind API, not this path.
///   - committed turns (`terminal_delivery_committed`) — already delivered, so a
///     watcher relay would be a duplicate (the gate yields them too).
///   - **planned restarts (`restart_mode.is_some()`)** — the yield gate resumes
///     those via its OWN `restart_mode.is_some() && !committed` escape hatch, so
///     they are NEVER black-holed; DLQ-ing them would be a false loss + a
///     double-delivery on recovery (review defect 1).
///   - the TUI-direct synthetic owner and **id-0 rows** (`user_msg_id == 0`,
///     injected / task-notification turns) — not real-user turns; the #4370 marker
///     gate deliberately never marks them, so the backstop must not mistake that for
///     a failed write (review defect 2). Enforced by sharing
///     [`super::runtime::readopt_marker_eligible_real_user`] verbatim with that gate.
///   - any row already owned by a live relay path (`Watcher` / `StandbyRelay` /
///     `SessionBoundRelay`) — only a `None` (bridge-owned/default) owner reaches the
///     yield-gate escape hatch.
pub(in crate::services::discord) fn crash_readopt_real_user_live_turn(
    state: &InflightTurnState,
) -> bool {
    !state.rebind_origin
        && state.restart_mode.is_none()
        && !state.terminal_delivery_committed
        && super::runtime::readopt_marker_eligible_real_user(state)
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

/// Pure DLQ-fire decision for the #4380 backstop, extracted so it is unit-testable
/// WITHOUT Postgres (the sink `record_detached` needs a live pool). Fires iff the
/// reloaded row is still the crash black-hole shape
/// ([`crash_readopt_real_user_live_turn`]) AND lacks the `readopted_from_inflight`
/// marker — i.e. the resume guard could not be armed (the marker WRITE failed with
/// IoError), so the recovered watcher WILL yield to the dead bridge. The shape
/// already excludes planned restarts and id-0 rows, so a marker that is absent
/// BY DESIGN (never eligible) can never trip this.
pub(in crate::services::discord) fn readopt_relay_black_hole_dead_letter_required(
    state: &InflightTurnState,
) -> bool {
    crash_readopt_real_user_live_turn(state) && !state.readopted_from_inflight
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
    let Some(reloaded) =
        crate::services::discord::inflight::load_inflight_state(provider, channel_id.get())
    else {
        return;
    };
    if readopt_relay_black_hole_dead_letter_required(&reloaded) {
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

    // --- #4380 review round 2: guard/DLQ-fire decision ---
    // These pin the pure fire predicate `readopt_relay_black_hole_dead_letter_required`
    // (the decision inside `guard_readopt_relay_resume_or_dead_letter`) so the fire
    // condition is verified WITHOUT Postgres — the sink `record_detached` needs a
    // live pool, but the *decision* is a pure function.

    /// #4380 review defect 1: a PLANNED restart (DrainRestart) row is resumed by the
    /// yield gate's own `restart_mode.is_some()` hatch, so it is NEVER black-holed.
    /// The backstop must NOT dead-letter it even with the marker absent (else false
    /// loss + double-delivery on recovery). MUTATION: delete `restart_mode.is_none()`
    /// from `crash_readopt_real_user_live_turn` → this assert FAILS.
    #[test]
    fn planned_restart_missing_marker_does_not_dead_letter() {
        with_readopted_crash_turn(|mut state| {
            state.readopted_from_inflight = false;
            state.set_restart_mode(crate::services::discord::InflightRestartMode::DrainRestart);
            assert!(
                !readopt_relay_black_hole_dead_letter_required(&state),
                "a planned-restart row is resumed by the restart_mode escape hatch, not black-holed"
            );
        });
    }

    /// #4380 review defect 2: a real-owner but id-0 (`user_msg_id == 0`) row is an
    /// injected / task-notification turn the #4370 marker gate deliberately never
    /// marks. The backstop must NOT mistake that BY-DESIGN absence for a failed
    /// write. MUTATION: delete `user_msg_id != 0` from
    /// `runtime::readopt_marker_eligible_real_user` → this assert FAILS.
    #[test]
    fn real_owner_id0_missing_marker_does_not_dead_letter() {
        with_readopted_crash_turn(|mut state| {
            state.readopted_from_inflight = false;
            state.user_msg_id = 0;
            assert!(
                !readopt_relay_black_hole_dead_letter_required(&state),
                "id-0 rows are never marker-eligible; an absent marker is by-design, not a failed write"
            );
        });
    }

    /// A genuine crash (`restart_mode == None`) real-user (`user_msg_id != 0`) live
    /// turn whose marker write FAILED → the recovered watcher WILL yield to the dead
    /// bridge, so the backstop MUST dead-letter the undelivered body.
    #[test]
    fn crash_real_user_missing_marker_dead_letters() {
        with_readopted_crash_turn(|mut state| {
            state.readopted_from_inflight = false;
            assert!(
                readopt_relay_black_hole_dead_letter_required(&state),
                "a crash re-adopt whose marker failed to persist is the real black-hole → must DLQ"
            );
        });
    }

    /// The normal path: the marker persisted (helper sets it), so the resume guard
    /// is armed and the backstop is a no-op.
    #[test]
    fn marker_present_is_a_no_op() {
        with_readopted_crash_turn(|state| {
            assert!(
                !readopt_relay_black_hole_dead_letter_required(&state),
                "when the marker persisted the resume guard is armed → no dead-letter"
            );
        });
    }
}
