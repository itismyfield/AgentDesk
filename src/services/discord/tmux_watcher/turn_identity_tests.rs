use super::*;
use crate::services::discord::inflight::InflightTurnState;

fn state_for_turn(user_msg_id: u64, tmux_session_name: &str) -> InflightTurnState {
    InflightTurnState::new(
        ProviderKind::Codex,
        42,
        Some("adk-cdx".to_string()),
        7,
        user_msg_id,
        user_msg_id + 1,
        "prompt".to_string(),
        None,
        Some(tmux_session_name.to_string()),
        Some("/tmp/out.jsonl".to_string()),
        Some("/tmp/in.fifo".to_string()),
        0,
    )
}

#[test]
fn watcher_identity_refreshes_for_next_turn_on_same_long_lived_session() {
    let first = state_for_turn(100, "AgentDesk-codex-adk-cdx");
    let second = state_for_turn(200, "AgentDesk-codex-adk-cdx");
    let mut identity = matching_watcher_turn_identity(Some(&first), "AgentDesk-codex-adk-cdx");
    assert_eq!(identity.as_ref().unwrap().user_msg_id, 100);

    identity = matching_watcher_turn_identity(Some(&second), "AgentDesk-codex-adk-cdx");

    assert_eq!(identity.unwrap().user_msg_id, 200);
}

#[test]
fn watcher_identity_does_not_adopt_different_session_name() {
    let first = state_for_turn(100, "AgentDesk-codex-adk-cdx");
    let second = state_for_turn(200, "AgentDesk-codex-adk-cdx-fresh");
    let mut identity = matching_watcher_turn_identity(Some(&first), "AgentDesk-codex-adk-cdx");
    assert_eq!(identity.as_ref().unwrap().user_msg_id, 100);

    identity = matching_watcher_turn_identity(Some(&second), "AgentDesk-codex-adk-cdx");

    assert!(identity.is_none());
}

// #3016 codex R2 (offset-aliasing id-selection). Exercises the SELECTION
// path the call site uses (`pinned_finalize_user_msg_id`) — which the
// direct-helper `stale_normal_completion_does_not_release_newer_active_turn`
// test does NOT cover. The hazard: a follow-up turn on the SAME session whose
// `turn_start_offset >= current_offset` (it begins AFTER the range this
// completion covers) sits in `inflight_before_relay`; passing its id to the
// finalizer would release the WRONG (newer, still-running) turn. The
// selection must return 0 in that case, mirroring the watcher-yield guard at
// tmux.rs:2110-2111.
fn state_with_offsets(
    user_msg_id: u64,
    tmux_session_name: &str,
    turn_start_offset: Option<u64>,
    last_offset: u64,
) -> InflightTurnState {
    let mut state = state_for_turn(user_msg_id, tmux_session_name);
    state.last_offset = last_offset;
    state.turn_start_offset = turn_start_offset;
    state
}

#[test]
fn pinned_finalize_id_matching_turn_in_range_returns_its_id() {
    // (a) The pinned turn's output reaches current_offset
    // (turn_start_offset 10 < current_offset 50) → return its id.
    let state = state_with_offsets(700, "AgentDesk-codex-adk-cdx", Some(10), 10);
    assert_eq!(
        pinned_finalize_user_msg_id(Some(&state), "AgentDesk-codex-adk-cdx", 50),
        700
    );
}

#[test]
fn pinned_finalize_id_newer_followup_turn_after_range_returns_zero() {
    // (b) Follow-up turn started AFTER this range
    // (turn_start_offset 50 >= current_offset 50) → 0, NOT the newer id.
    let newer = state_with_offsets(800, "AgentDesk-codex-adk-cdx", Some(50), 50);
    assert_eq!(
        pinned_finalize_user_msg_id(Some(&newer), "AgentDesk-codex-adk-cdx", 50),
        0
    );
    // Also strictly-after (start 60 > 50) → 0.
    let later = state_with_offsets(801, "AgentDesk-codex-adk-cdx", Some(60), 60);
    assert_eq!(
        pinned_finalize_user_msg_id(Some(&later), "AgentDesk-codex-adk-cdx", 50),
        0
    );
}

#[test]
fn pinned_finalize_id_falls_back_to_last_offset_like_the_guard() {
    // Mirror the guard's `turn_start_offset.unwrap_or(last_offset)`: with no
    // turn_start_offset, last_offset 50 >= current_offset 50 → 0.
    let no_start = state_with_offsets(802, "AgentDesk-codex-adk-cdx", None, 50);
    assert_eq!(
        pinned_finalize_user_msg_id(Some(&no_start), "AgentDesk-codex-adk-cdx", 50),
        0
    );
    // last_offset 10 < 50 → return id.
    let in_range = state_with_offsets(803, "AgentDesk-codex-adk-cdx", None, 10);
    assert_eq!(
        pinned_finalize_user_msg_id(Some(&in_range), "AgentDesk-codex-adk-cdx", 50),
        803
    );
}

#[test]
fn pinned_finalize_id_wrong_session_returns_zero() {
    // (c) Different tmux session → 0 even though it is in range.
    let other = state_with_offsets(900, "AgentDesk-codex-adk-cdx-fresh", Some(10), 10);
    assert_eq!(
        pinned_finalize_user_msg_id(Some(&other), "AgentDesk-codex-adk-cdx", 50),
        0
    );
}

#[test]
fn pinned_finalize_id_zero_user_msg_id_returns_zero() {
    // (d) Anchorless turn (user_msg_id == 0) → 0.
    let anchorless = state_with_offsets(0, "AgentDesk-codex-adk-cdx", Some(10), 10);
    assert_eq!(
        pinned_finalize_user_msg_id(Some(&anchorless), "AgentDesk-codex-adk-cdx", 50),
        0
    );
}

#[test]
fn pinned_finalizer_turn_id_uses_synthetic_identity_for_zero_user_msg_id() {
    let mut state = state_with_offsets(0, "AgentDesk-codex-adk-cdx", Some(10), 10);
    state.finalizer_turn_id = 1_234_567;
    assert_eq!(
        pinned_finalizer_turn_id(Some(&state), "AgentDesk-codex-adk-cdx", 50),
        1_234_567
    );
    assert_eq!(
        pinned_finalizer_turn_id(Some(&state), "AgentDesk-codex-adk-cdx", 10),
        0
    );
}

#[test]
fn pinned_finalize_id_none_returns_zero() {
    // (e) No pre-relay snapshot → 0.
    assert_eq!(
        pinned_finalize_user_msg_id(None, "AgentDesk-codex-adk-cdx", 50),
        0
    );
}

// #3016 codex R3 (wrong-turn lifecycle corruption). The SAME committed block
// that finalizes also runs `⏳ → ✅` + transcript/analytics + clear on the
// LATE-read inflight. `committed_completion_is_stale_for_newer_turn` is the
// exact complement of `pinned_finalize_user_msg_id`'s `< current_offset`
// range test: it returns TRUE iff EITHER snapshot is a real NEWER turn on the
// SAME session that began AT/AFTER this range (so those side-effects must be
// skipped). Mirrors the yield guard's offset/fallback semantics.
#[test]
fn committed_completion_stale_for_newer_turn_matrix() {
    let session = "AgentDesk-codex-adk-cdx";
    // (a) newer turn after range (start 50 >= current 50, same session,
    // id != 0) → true. Here it sits in inflight_state (late read).
    let newer = state_with_offsets(800, session, Some(50), 50);
    assert!(committed_completion_is_stale_for_newer_turn(
        None,
        Some(&newer),
        session,
        50
    ));
    // strictly-after (start 60 > 50) → true.
    let later = state_with_offsets(801, session, Some(60), 60);
    assert!(committed_completion_is_stale_for_newer_turn(
        None,
        Some(&later),
        session,
        50
    ));

    // (b) current/older turn (start 10 < current 50) → false (normal path).
    let in_range = state_with_offsets(700, session, Some(10), 10);
    assert!(!committed_completion_is_stale_for_newer_turn(
        Some(&in_range),
        Some(&in_range),
        session,
        50
    ));

    // (c) wrong session, even though it is a newer turn → false.
    let other_session = state_with_offsets(900, "AgentDesk-codex-adk-cdx-fresh", Some(50), 50);
    assert!(!committed_completion_is_stale_for_newer_turn(
        None,
        Some(&other_session),
        session,
        50
    ));

    // (d) id == 0 (anchorless / rebind-style) newer turn → false.
    let anchorless = state_with_offsets(0, session, Some(50), 50);
    assert!(!committed_completion_is_stale_for_newer_turn(
        None,
        Some(&anchorless),
        session,
        50
    ));

    // (e) None / None → false (no inflight at all).
    assert!(!committed_completion_is_stale_for_newer_turn(
        None, None, session, 50
    ));

    // (f) only inflight_before_relay is newer (inflight_state older) → true.
    assert!(committed_completion_is_stale_for_newer_turn(
        Some(&newer),
        Some(&in_range),
        session,
        50
    ));
    // …and vice-versa: only inflight_state is newer → true.
    assert!(committed_completion_is_stale_for_newer_turn(
        Some(&in_range),
        Some(&newer),
        session,
        50
    ));

    // Fallback parity with the guard: no turn_start_offset → use last_offset.
    // last_offset 50 >= current 50 → newer → true.
    let no_start_after = state_with_offsets(802, session, None, 50);
    assert!(committed_completion_is_stale_for_newer_turn(
        None,
        Some(&no_start_after),
        session,
        50
    ));
    // last_offset 10 < current 50 → not newer → false.
    let no_start_before = state_with_offsets(803, session, None, 10);
    assert!(!committed_completion_is_stale_for_newer_turn(
        None,
        Some(&no_start_before),
        session,
        50
    ));
}

/// #3016 (codex B1): the call-site guard proof. In the stale-newer-turn
/// scenario the watcher MUST skip `finish_restored_watcher_active_turn`
/// because `pinned_finalize_user_msg_id` would return 0 and an id-0
/// `Complete` would collapse onto the newer live turn (see
/// `turn_finalizer::tests::stale_completion_skips_finalize_no_id0_collapse`).
/// This asserts the two predicates the call site relies on line up:
///   1. `committed_completion_is_stale_for_newer_turn` is TRUE (→ guard skips
///      the finalize), AND
///   2. `pinned_finalize_user_msg_id` is 0 for the SAME snapshot (→ the id
///      that WOULD have been submitted is the unsafe channel-collapse id),
/// so "stale" ⇔ "id 0" ⇔ "skip" by construction.
#[test]
fn stale_completion_skips_finalize_no_id0_collapse() {
    let session = "AgentDesk-codex-adk-cdx";
    // A NEWER same-session turn (id 999) that started AT/AFTER this range
    // (turn_start_offset 50 >= current_offset 50). This is the late-read
    // inflight a follow-up turn rewrote onto disk before this stale pass.
    let newer = state_with_offsets(999, session, Some(50), 50);

    // (1) Guard predicate: stale → the call site skips the finalize entirely.
    assert!(
        committed_completion_is_stale_for_newer_turn(Some(&newer), Some(&newer), session, 50),
        "newer same-session turn at/after the range must be classified stale so the \
             call site skips finish_restored_watcher_active_turn"
    );

    // (2) The id that WOULD have been submitted is 0 — the unsafe
    // channel-collapse id proven hazardous in the turn_finalizer test.
    assert_eq!(
        pinned_finalize_user_msg_id(Some(&newer), session, 50),
        0,
        "stale newer turn pins to 0 — submitting Complete with this id would \
             collapse onto the newer live ledger entry (wrong-turn finalize)"
    );
}

// #3142 test scaffolding: build a snapshot at given offsets that ALSO carries
// an anchor/external identity (injected_prompt_message_id and/or
// ExternalInput turn_source). `state_with_offsets` cannot set those fields, so
// mutate them directly (all pub).
fn state_with_anchor(
    user_msg_id: u64,
    tmux_session_name: &str,
    turn_start_offset: Option<u64>,
    last_offset: u64,
    injected_prompt_message_id: Option<u64>,
    external_input: bool,
) -> InflightTurnState {
    let mut state = state_with_offsets(
        user_msg_id,
        tmux_session_name,
        turn_start_offset,
        last_offset,
    );
    state.injected_prompt_message_id = injected_prompt_message_id;
    if external_input {
        state.turn_source = crate::services::discord::inflight::TurnSource::ExternalInput;
    }
    state
}

// #3142 (EPIC, follow-up to #3141). The committed-output anchor-cleanup
// branches act on a `user_msg_id == 0` external-input / injected-anchor newer
// turn that the id!=0 `committed_completion_is_stale_for_newer_turn`
// deliberately excludes. `committed_anchor_cleanup_is_stale_for_newer_turn` is
// the id==0-inclusive sibling for those branches. This matrix mirrors
// `committed_completion_stale_for_newer_turn_matrix` and locks the divergence.
#[test]
fn committed_anchor_cleanup_stale_for_newer_turn_matrix() {
    let session = "AgentDesk-codex-adk-cdx";

    // (a) id!=0 newer turn after range (start 50 >= current 50) → true.
    let newer_id = state_with_offsets(800, session, Some(50), 50);
    assert!(committed_anchor_cleanup_is_stale_for_newer_turn(
        None,
        Some(&newer_id),
        session,
        50
    ));

    // (b) id==0 + injected_prompt_message_id newer after range → true.
    // THE new coverage: assert the id!=0 sibling returns FALSE on the SAME
    // state, locking the divergence.
    let newer_injected = state_with_anchor(0, session, Some(50), 50, Some(4242), false);
    assert!(committed_anchor_cleanup_is_stale_for_newer_turn(
        None,
        Some(&newer_injected),
        session,
        50
    ));
    assert!(
        !committed_completion_is_stale_for_newer_turn(None, Some(&newer_injected), session, 50),
        "id!=0 sibling must NOT classify an id==0 injected newer turn as stale"
    );

    // (c) id==0 + ExternalInput turn_source newer after range → true.
    let newer_external = state_with_anchor(0, session, Some(50), 50, None, true);
    assert!(committed_anchor_cleanup_is_stale_for_newer_turn(
        None,
        Some(&newer_external),
        session,
        50
    ));
    assert!(!committed_completion_is_stale_for_newer_turn(
        None,
        Some(&newer_external),
        session,
        50
    ));

    // (d) this/older turn (start 10 < 50), any id → false.
    let in_range_id = state_with_offsets(700, session, Some(10), 10);
    assert!(!committed_anchor_cleanup_is_stale_for_newer_turn(
        Some(&in_range_id),
        Some(&in_range_id),
        session,
        50
    ));
    let in_range_injected = state_with_anchor(0, session, Some(10), 10, Some(4243), false);
    assert!(!committed_anchor_cleanup_is_stale_for_newer_turn(
        Some(&in_range_injected),
        Some(&in_range_injected),
        session,
        50
    ));

    // (e) wrong session, even though newer → false.
    let other_session = state_with_anchor(
        0,
        "AgentDesk-codex-adk-cdx-fresh",
        Some(50),
        50,
        Some(9),
        true,
    );
    assert!(!committed_anchor_cleanup_is_stale_for_newer_turn(
        None,
        Some(&other_session),
        session,
        50
    ));

    // (f) None / None → false.
    assert!(!committed_anchor_cleanup_is_stale_for_newer_turn(
        None, None, session, 50
    ));

    // (g) id==0 + no anchor + no external (plain empty newer row) → false:
    // the anchor-relevance disjunct gates it out.
    let empty_newer = state_with_anchor(0, session, Some(50), 50, None, false);
    assert!(!committed_anchor_cleanup_is_stale_for_newer_turn(
        None,
        Some(&empty_newer),
        session,
        50
    ));

    // (h) only inflight_before_relay newer (state older) → true; and vice-versa.
    assert!(committed_anchor_cleanup_is_stale_for_newer_turn(
        Some(&newer_injected),
        Some(&in_range_id),
        session,
        50
    ));
    assert!(committed_anchor_cleanup_is_stale_for_newer_turn(
        Some(&in_range_id),
        Some(&newer_injected),
        session,
        50
    ));

    // (i) turn_start_offset=None fallback to last_offset parity.
    let no_start_after = state_with_anchor(0, session, None, 50, Some(7), false);
    assert!(committed_anchor_cleanup_is_stale_for_newer_turn(
        None,
        Some(&no_start_after),
        session,
        50
    ));
    let no_start_before = state_with_anchor(0, session, None, 10, Some(7), false);
    assert!(!committed_anchor_cleanup_is_stale_for_newer_turn(
        None,
        Some(&no_start_before),
        session,
        50
    ));
}

// #3142 consumer-1 (dispatch finalization). The call site at the
// `else if let Some(did) = resolved_did.as_deref().filter(|_| !stale)` arm
// falls through to the `else => true` no-finalize arm when stale. Pure
// predicate alignment: stale → skip; in-range → run.
#[test]
fn dispatch_finalization_skips_when_stale() {
    let session = "AgentDesk-codex-adk-cdx";
    // (a) newer id!=0 snapshot → stale true → dispatch finalize skipped.
    let newer = state_with_offsets(999, session, Some(50), 50);
    assert!(committed_completion_is_stale_for_newer_turn(
        None,
        Some(&newer),
        session,
        50
    ));
    // (b) common case: in-range turn → stale false → finalize runs.
    let in_range = state_with_offsets(700, session, Some(10), 10);
    assert!(!committed_completion_is_stale_for_newer_turn(
        Some(&in_range),
        Some(&in_range),
        session,
        50
    ));
}

// #3142 consumer-2 (history append). The push is gated on
// `!completion_is_stale_for_newer_turn`. Same predicate as dispatch; assert
// both directions so the (user_text, response) pair is never cross-paired.
#[test]
fn history_append_skips_when_stale() {
    let session = "AgentDesk-codex-adk-cdx";
    // The #3142 history push gate is
    // `!completion_is_stale_for_newer_turn && !anchor_cleanup_is_stale_for_newer_turn`.
    let newer = state_with_offsets(800, session, Some(50), 50);
    assert!(
        committed_completion_is_stale_for_newer_turn(None, Some(&newer), session, 50),
        "id!=0 stale newer turn → history push suppressed (no cross-paired user_text/response)"
    );
    let in_range = state_with_offsets(700, session, Some(10), 10);
    assert!(
        !committed_completion_is_stale_for_newer_turn(
            Some(&in_range),
            Some(&in_range),
            session,
            50
        ),
        "common case → not id!=0-stale (history push runs)"
    );
    assert!(
        !committed_anchor_cleanup_is_stale_for_newer_turn(
            Some(&in_range),
            Some(&in_range),
            session,
            50
        ),
        "common case → not anchor-stale either (history push runs)"
    );

    // #3142 history-append id==0 gap: a NEWER external-input turn with
    // `user_msg_id == 0` is DELIBERATELY excluded by the id!=0 sibling, so the
    // push must ALSO gate on the id==0-inclusive anchor helper — otherwise the
    // newer turn's `user_text` cross-pairs with the OLDER response in TUI
    // history (the exact residual the EPIC's proposed direction called out).
    let newer_external_id0 = state_with_anchor(0, session, Some(50), 50, None, true);
    assert!(
        !committed_completion_is_stale_for_newer_turn(None, Some(&newer_external_id0), session, 50),
        "id!=0 sibling alone does NOT suppress an id==0 external newer turn (the gap)"
    );
    assert!(
        committed_anchor_cleanup_is_stale_for_newer_turn(
            None,
            Some(&newer_external_id0),
            session,
            50
        ),
        "id==0-inclusive anchor helper suppresses the id==0 external newer turn (gap closed)"
    );
}

// #3142 consumer-4 (status-panel). The completion identity is offset-pinned
// via `pinned_finalize_user_msg_id`: None for a newer pre-relay snapshot,
// Some(id) in-range, None for rebind_origin/id==0. Mirrors the new derivation.
#[test]
fn status_panel_id_none_when_pre_relay_snapshot_is_newer() {
    let session = "AgentDesk-codex-adk-cdx";

    // Reproduce the new derivation as a pure expression.
    let derive = |inflight: Option<&InflightTurnState>, current: u64| -> Option<u64> {
        let pinned = pinned_finalize_user_msg_id(inflight, session, current);
        inflight
            .filter(|i| !i.rebind_origin)
            .and_then(|_| (pinned != 0).then_some(pinned))
    };

    // (a) newer pre-relay snapshot (start 50 >= current 50) → None.
    let newer = state_with_offsets(800, session, Some(50), 50);
    assert_eq!(derive(Some(&newer), 50), None);

    // (b) in-range snapshot → Some(its id).
    let in_range = state_with_offsets(700, session, Some(10), 10);
    assert_eq!(derive(Some(&in_range), 50), Some(700));

    // (c) rebind_origin in-range → None (parity with the old filter).
    let mut rebind = state_with_offsets(701, session, Some(10), 10);
    rebind.rebind_origin = true;
    assert_eq!(derive(Some(&rebind), 50), None);

    // (d) id==0 in-range → None.
    let id_zero = state_with_offsets(0, session, Some(10), 10);
    assert_eq!(derive(Some(&id_zero), 50), None);

    // (e) absent → None.
    assert_eq!(derive(None, 50), None);
}

// #3142 consumer-4 (status-panel ADOPT + EDIT gate). codex review found a
// residual aliasing gap: the completion IDENTITY is offset-pinned (covered
// above), but the adopt site (L8328) and the EDIT/finalize site (L10063)
// were NOT gated, so the older committed range could still pull
// `status_message_id` from a stale NEWER pre-relay snapshot and EDIT that
// newer turn's panel. Both sites now gate on
// `!committed_anchor_cleanup_is_stale_for_newer_turn(inflight_before_relay,
// None, session, current_offset)`. This test exercises:
//   (a) stale newer turn (incl. id==0 external/injected) → NOT adopted, NOT
//       edited;
//   (b) in-range id==0 watcher-direct turn → STILL adopts + edits (the
//       over-suppression guard: gate keys off OFFSET staleness, not pinned==0,
//       so the common id==0 case is NOT suppressed);
//   (c) in-range id!=0 normal turn → unchanged (adopts + edits).
#[test]
fn status_panel_adopt_and_edit_gate_is_turn_aliasing_safe() {
    let session = "AgentDesk-codex-adk-cdx";

    // Mirror the call-site predicate exactly: pre-relay snapshot + None second
    // arg + the function-level current_offset.
    let gate_skips = |inflight: Option<&InflightTurnState>, current: u64| -> bool {
        committed_anchor_cleanup_is_stale_for_newer_turn(inflight, None, session, current)
    };

    // Model the adopt site: a status_panel_msg_id is pulled from the snapshot's
    // status_message_id ONLY when the gate does NOT skip.
    let adopt = |inflight: &InflightTurnState, current: u64| -> Option<serenity::MessageId> {
        let mut placeholder: Option<serenity::MessageId> = None;
        let mut placeholder_from_restored = false;
        let mut status_panel: Option<serenity::MessageId> = None;
        if !gate_skips(Some(inflight), current) {
            adopt_watcher_terminal_message_ids_from_inflight(
                &mut placeholder,
                &mut placeholder_from_restored,
                &mut status_panel,
                inflight,
                session,
            );
        }
        status_panel
    };

    let with_panel = |mut state: InflightTurnState, panel: u64| -> InflightTurnState {
        state.status_message_id = Some(panel);
        state
    };

    // (a-i) stale newer id!=0 turn (start 50 >= current 50): owns panel 5550.
    // Gate skips → not adopted, not edited.
    let newer_id = with_panel(state_with_offsets(800, session, Some(50), 50), 5550);
    assert!(
        gate_skips(Some(&newer_id), 50),
        "newer id!=0 → EDIT gate skips"
    );
    assert_eq!(
        adopt(&newer_id, 50),
        None,
        "newer id!=0 → panel NOT adopted"
    );

    // (a-ii) stale newer id==0 EXTERNAL-input turn: owns panel 5551. The id!=0
    // sibling would MISS this; the anchor variant catches it.
    let newer_ext = with_panel(
        state_with_anchor(0, session, Some(50), 50, None, true),
        5551,
    );
    assert!(gate_skips(Some(&newer_ext), 50));
    assert!(
        !committed_completion_is_stale_for_newer_turn(Some(&newer_ext), None, session, 50),
        "id!=0 sibling MISSES the id==0 external panel owner — anchor variant required"
    );
    assert_eq!(
        adopt(&newer_ext, 50),
        None,
        "newer id==0 external → panel NOT adopted"
    );

    // (a-iii) stale newer id==0 INJECTED turn: owns panel 5552 → not adopted.
    let newer_inj = with_panel(
        state_with_anchor(0, session, Some(50), 50, Some(4242), false),
        5552,
    );
    assert!(gate_skips(Some(&newer_inj), 50));
    assert_eq!(adopt(&newer_inj, 50), None);

    // (b) OVER-SUPPRESSION GUARD: in-range id==0 watcher-direct turn
    // (start 10 < current 50, user_msg_id==0). pinned==0 here (ambiguous), but
    // the OFFSET gate is FALSE → STILL adopts + edits its panel 6660.
    let in_range_id0 = with_panel(state_with_offsets(0, session, Some(10), 10), 6660);
    assert!(
        !gate_skips(Some(&in_range_id0), 50),
        "in-range id==0 watcher-direct → EDIT gate must NOT skip (not over-suppressed)"
    );
    assert_eq!(
        adopt(&in_range_id0, 50),
        Some(serenity::MessageId::new(6660)),
        "in-range id==0 watcher-direct → panel STILL adopted"
    );

    // (c) in-range id!=0 normal turn → unchanged: adopts + edits panel 7770.
    let in_range_id = with_panel(state_with_offsets(700, session, Some(10), 10), 7770);
    assert!(!gate_skips(Some(&in_range_id), 50));
    assert_eq!(
        adopt(&in_range_id, 50),
        Some(serenity::MessageId::new(7770)),
        "in-range id!=0 normal → panel adopted as today"
    );

    // (d) no pre-relay snapshot → gate false (no-op): nothing to adopt anyway.
    assert!(!gate_skips(None, 50));
}

// #3142 consumer-3 (anchor cleanup, id==0). The second branch is gated on
// `!committed_anchor_cleanup_is_stale_for_newer_turn`. Stale id==0 injected
// newer → skipped; in-range id==0 injected → runs (not over-suppressed).
#[test]
fn anchor_cleanup_skips_when_stale_id0() {
    let session = "AgentDesk-codex-adk-cdx";
    let newer = state_with_anchor(0, session, Some(50), 50, Some(4242), false);
    assert!(committed_anchor_cleanup_is_stale_for_newer_turn(
        None,
        Some(&newer),
        session,
        50
    ));
    // Common case: id==0 injected turn whose output reaches current_offset.
    let in_range = state_with_anchor(0, session, Some(10), 10, Some(4243), false);
    assert!(!committed_anchor_cleanup_is_stale_for_newer_turn(
        None,
        Some(&in_range),
        session,
        50
    ));
}

// #3142 first-anchor branch (lifecycle_stage_paused). The branch head is
// gated on `!committed_anchor_cleanup_is_stale_for_newer_turn`. A paused turn
// WITH a newer inflight present → stale true → first branch skipped; with an
// in-range inflight → false → runs. (Helper is independent of the paused flag;
// the paused flag selects WHICH branch reaches the gate at the call site.)
#[test]
fn paused_first_branch_anchor_gate() {
    let session = "AgentDesk-codex-adk-cdx";
    // Newer inflight present (the paused-with-inflight scenario) → stale.
    let newer = state_with_anchor(0, session, Some(50), 50, Some(55), true);
    assert!(committed_anchor_cleanup_is_stale_for_newer_turn(
        None,
        Some(&newer),
        session,
        50
    ));
    // In-range inflight → not stale → first branch runs.
    let in_range = state_with_anchor(0, session, Some(10), 10, Some(55), true);
    assert!(!committed_anchor_cleanup_is_stale_for_newer_turn(
        None,
        Some(&in_range),
        session,
        50
    ));
    // !inflight_present arm: inflight_state is None, only before_relay
    // inspected. Absent/in-range before_relay → false → legitimate cleanup.
    assert!(!committed_anchor_cleanup_is_stale_for_newer_turn(
        None, None, session, 50
    ));
    assert!(!committed_anchor_cleanup_is_stale_for_newer_turn(
        Some(&in_range),
        None,
        session,
        50
    ));
}

#[test]
fn watcher_creates_status_panel_for_external_input_when_v2_on_and_panel_absent() {
    // #3003: pure TUI-direct (ExternalInput) turn with v2 enabled and no panel
    // yet must proactively create one.
    assert!(watcher_should_create_external_input_status_panel(
        true,  // status_panel_v2_enabled
        false, // status_panel_present
        true,  // inflight_represents_external_input
    ));
}

#[test]
fn watcher_skips_status_panel_creation_when_panel_already_present() {
    // An adopted/existing panel must never be duplicated.
    assert!(!watcher_should_create_external_input_status_panel(
        true, true, true
    ));
}

#[test]
fn watcher_skips_status_panel_creation_for_non_external_input_turns() {
    // Discord-intake (Managed) turns are owned by turn_bridge, which creates
    // the panel itself — the watcher must not create a second one.
    assert!(!watcher_should_create_external_input_status_panel(
        true, false, false
    ));
}

#[test]
fn watcher_skips_status_panel_creation_when_v2_disabled() {
    assert!(!watcher_should_create_external_input_status_panel(
        false, false, true
    ));
}

// #3099: a TUI-injected task-notification turn completes with an inflight
// whose `user_msg_id == 0`; the `⏳ → ✅` reaction block skips it (no real
// anchored user message), so it must route to the anchor-lifecycle cleanup
// that removes `⏳` from the injected notify-bot message itself.
#[test]
fn watcher_external_input_user_msg_zero_needs_anchor_cleanup() {
    let mut external = state_for_turn(0, "AgentDesk-claude-adk-cc");
    external.turn_source = crate::services::discord::inflight::TurnSource::ExternalInput;
    assert!(watcher_inflight_needs_anchor_lifecycle_cleanup(&external));

    // A rebind_origin synthetic (also user_msg_id == 0) likewise needs it.
    let mut rebind = state_for_turn(0, "AgentDesk-claude-adk-cc");
    rebind.turn_source = crate::services::discord::inflight::TurnSource::ExternalInput;
    rebind.rebind_origin = true;
    assert!(watcher_inflight_needs_anchor_lifecycle_cleanup(&rebind));
}

// An external-input turn that DOES carry a real anchored message id is
// handled by the `⏳ → ✅` block directly, so it must NOT also run the
// anchor-lifecycle cleanup (which would double-react / clear the anchor).
#[test]
fn watcher_external_input_with_real_user_msg_skips_anchor_cleanup() {
    let mut external = state_for_turn(900, "AgentDesk-claude-adk-cc");
    external.turn_source = crate::services::discord::inflight::TurnSource::ExternalInput;
    assert!(!watcher_inflight_needs_anchor_lifecycle_cleanup(&external));
}

// A normal managed (Discord-intake) turn never uses the injected-anchor path.
#[test]
fn watcher_managed_turn_never_needs_anchor_cleanup() {
    let managed = state_for_turn(0, "AgentDesk-claude-adk-cc");
    assert!(!watcher_inflight_needs_anchor_lifecycle_cleanup(&managed));
}

#[test]
fn watcher_external_input_predicate_matches_external_turn_sources() {
    let mut external = state_for_turn(0, "AgentDesk-codex-adk-cdx");
    external.turn_source = crate::services::discord::inflight::TurnSource::ExternalInput;
    assert!(watcher_inflight_represents_external_input(Some(&external)));

    let mut adopted = state_for_turn(0, "AgentDesk-codex-adk-cdx");
    adopted.turn_source = crate::services::discord::inflight::TurnSource::ExternalAdopted;
    assert!(watcher_inflight_represents_external_input(Some(&adopted)));

    let managed = state_for_turn(100, "AgentDesk-codex-adk-cdx");
    assert!(!watcher_inflight_represents_external_input(Some(&managed)));
    assert!(!watcher_inflight_represents_external_input(None));
}

#[test]
fn watcher_adopts_persisted_panel_for_matching_session() {
    // #3003 codex P2: a panel persisted on this turn's inflight (status set,
    // current_msg_id still 0) must be adopted on restart, not re-created.
    let mut state = state_for_turn(0, "AgentDesk-codex-adk-cdx");
    state.status_message_id = Some(1_510_747_006_337_945_732);
    assert_eq!(
        watcher_persisted_status_panel_msg_id(Some(&state), "AgentDesk-codex-adk-cdx"),
        Some(serenity::MessageId::new(1_510_747_006_337_945_732))
    );
}

#[test]
fn watcher_does_not_adopt_synthetic_headless_persisted_panel() {
    // #3003 codex P2 r3: a synthetic headless id must not be adopted as a
    // real Discord message (>= 8e18 is the synthetic range).
    let mut state = state_for_turn(0, "AgentDesk-codex-adk-cdx");
    state.status_message_id = Some(8_000_000_000_000_000_001);
    assert_eq!(
        watcher_persisted_status_panel_msg_id(Some(&state), "AgentDesk-codex-adk-cdx"),
        None
    );
}

#[test]
fn watcher_does_not_adopt_persisted_panel_from_other_session() {
    let mut state = state_for_turn(0, "AgentDesk-codex-adk-cdx");
    state.status_message_id = Some(1_510_747_006_337_945_732);
    assert_eq!(
        watcher_persisted_status_panel_msg_id(Some(&state), "AgentDesk-codex-adk-cdx-fresh"),
        None
    );
}

#[test]
fn watcher_does_not_adopt_persisted_panel_without_session_binding() {
    let mut state = state_for_turn(0, "AgentDesk-codex-adk-cdx");
    state.tmux_session_name = None;
    state.status_message_id = Some(1_510_747_006_337_945_732);
    assert_eq!(
        watcher_persisted_status_panel_msg_id(Some(&state), "AgentDesk-codex-adk-cdx"),
        None
    );
}

#[test]
fn watcher_has_no_persisted_panel_without_status_message_id() {
    let state = state_for_turn(0, "AgentDesk-codex-adk-cdx");
    assert_eq!(
        watcher_persisted_status_panel_msg_id(Some(&state), "AgentDesk-codex-adk-cdx"),
        None
    );
    assert_eq!(
        watcher_persisted_status_panel_msg_id(None, "AgentDesk-codex-adk-cdx"),
        None
    );
}

// #3077 (codex P1): the TUI-direct publish site must adopt the just-sent
// panel ONLY when the atomic bind recorded it on the inflight row. A
// successful bind (or one where the row already owns this exact id) adopts
// the handle and never deletes; any other outcome means the row does not
// reference our panel, so we delete the just-sent duplicate (not leak it)
// and never adopt it as the watcher's owned handle.
#[test]
fn tui_status_panel_bind_bound_adopts_without_delete() {
    let decision = resolve_tui_status_panel_bind_decision(
        crate::services::discord::inflight::StatusPanelBindOutcome::Bound,
    );
    assert!(decision.adopt_sent_panel);
    assert!(!decision.delete_sent_panel);
}

#[test]
fn tui_status_panel_bind_already_bound_adopts_without_delete() {
    let decision = resolve_tui_status_panel_bind_decision(
        crate::services::discord::inflight::StatusPanelBindOutcome::AlreadyBound,
    );
    assert!(decision.adopt_sent_panel);
    assert!(!decision.delete_sent_panel);
}

#[test]
fn tui_status_panel_bind_skipped_panel_already_set_deletes_and_adopts_owned() {
    // #3077 codex P2 #2: the inflight row already carries a DIFFERENT panel id
    // (observed under the bind's flock). Our just-sent panel is a duplicate and
    // must be deleted, never adopted as our handle. The decision must surface
    // the row's CURRENT owned id so the caller adopts the real panel instead of
    // the (possibly stale) pre-bind snapshot.
    let decision = resolve_tui_status_panel_bind_decision(
        crate::services::discord::inflight::StatusPanelBindOutcome::SkippedPanelAlreadySet(4242),
    );
    assert!(decision.delete_sent_panel);
    assert!(!decision.adopt_sent_panel);
    assert_eq!(decision.owned_panel_id, Some(4242));
}

#[test]
fn tui_status_panel_bind_guard_mismatch_deletes_and_disowns() {
    let decision = resolve_tui_status_panel_bind_decision(
        crate::services::discord::inflight::StatusPanelBindOutcome::GuardMismatch,
    );
    assert!(decision.delete_sent_panel);
    assert!(!decision.adopt_sent_panel);
    // No owned id to adopt → handle left unset (safe).
    assert_eq!(decision.owned_panel_id, None);
}

#[test]
fn tui_status_panel_bind_missing_deletes_and_disowns() {
    let decision = resolve_tui_status_panel_bind_decision(
        crate::services::discord::inflight::StatusPanelBindOutcome::Missing,
    );
    assert!(decision.delete_sent_panel);
    assert!(!decision.adopt_sent_panel);
    assert_eq!(decision.owned_panel_id, None);
}

#[test]
fn tui_status_panel_bind_io_error_deletes_and_disowns() {
    // A persist/IO failure means the bind did not happen; do not keep a
    // local handle that claims ownership of an unrecorded panel.
    let decision = resolve_tui_status_panel_bind_decision(
        crate::services::discord::inflight::StatusPanelBindOutcome::IoError,
    );
    assert!(decision.delete_sent_panel);
    assert!(!decision.adopt_sent_panel);
    assert_eq!(decision.owned_panel_id, None);
}

#[test]
fn watcher_external_input_for_session_requires_session_match() {
    // #3003 codex P2 r2: an ExternalInput inflight for a *different* tmux
    // session in the same channel must not trigger panel creation here.
    let mut external = state_for_turn(0, "AgentDesk-codex-adk-cdx");
    external.turn_source = crate::services::discord::inflight::TurnSource::ExternalInput;
    external.set_relay_owner_kind(crate::services::discord::inflight::RelayOwnerKind::Watcher);
    assert!(watcher_inflight_is_external_input_for_session(
        Some(&external),
        "AgentDesk-codex-adk-cdx"
    ));
    assert!(!watcher_inflight_is_external_input_for_session(
        Some(&external),
        "AgentDesk-codex-adk-cdx-other"
    ));

    // #3003 codex P2 r25: an external-input turn owned by the session-bound
    // relay (not the watcher) must NOT enter the watcher panel path.
    let mut session_bound = state_for_turn(0, "AgentDesk-codex-adk-cdx");
    session_bound.turn_source = crate::services::discord::inflight::TurnSource::ExternalInput;
    session_bound.set_relay_owner_kind(
        crate::services::discord::inflight::RelayOwnerKind::SessionBoundRelay,
    );
    assert!(!watcher_inflight_is_external_input_for_session(
        Some(&session_bound),
        "AgentDesk-codex-adk-cdx"
    ));

    // Managed turn on the matching session is still not external input.
    let managed = state_for_turn(100, "AgentDesk-codex-adk-cdx");
    assert!(!watcher_inflight_is_external_input_for_session(
        Some(&managed),
        "AgentDesk-codex-adk-cdx"
    ));
    assert!(!watcher_inflight_is_external_input_for_session(
        None,
        "AgentDesk-codex-adk-cdx"
    ));
}

// status-panel-v2: the synthetic monitor/self-paced-loop turn
// (`TurnSource::MonitorTriggered`, made watcher-relay-owned by
// `ensure_monitor_auto_turn_inflight`) must be panel-eligible for its own
// tmux session — but only when the watcher owns the relay and the session
// matches, mirroring the external-input guard.
#[test]
fn watcher_panel_eligible_for_session_includes_monitor_turn() {
    let mut monitor = state_for_turn(0, "AgentDesk-claude-monitor-relay");
    monitor.turn_source = crate::services::discord::inflight::TurnSource::MonitorTriggered;
    monitor.set_relay_owner_kind(crate::services::discord::inflight::RelayOwnerKind::Watcher);
    assert!(watcher_inflight_is_panel_eligible_for_session(
        Some(&monitor),
        "AgentDesk-claude-monitor-relay"
    ));

    // relay_owner=None (the pre-fix synthetic shape) is NOT watcher-owned, so
    // the panel guard rejects it.
    let mut owner_none = state_for_turn(0, "AgentDesk-claude-monitor-relay");
    owner_none.turn_source = crate::services::discord::inflight::TurnSource::MonitorTriggered;
    owner_none.set_relay_owner_kind(
        crate::services::discord::inflight::RelayOwnerKind::SessionBoundRelay,
    );
    assert!(!watcher_inflight_is_panel_eligible_for_session(
        Some(&owner_none),
        "AgentDesk-claude-monitor-relay"
    ));

    // Wrong session must not adopt the monitor panel.
    assert!(!watcher_inflight_is_panel_eligible_for_session(
        Some(&monitor),
        "AgentDesk-claude-monitor-relay-other"
    ));

    // A plain managed turn is never panel-eligible via this path.
    let mut managed = state_for_turn(100, "AgentDesk-claude-monitor-relay");
    managed.set_relay_owner_kind(crate::services::discord::inflight::RelayOwnerKind::Watcher);
    assert!(!watcher_inflight_is_panel_eligible_for_session(
        Some(&managed),
        "AgentDesk-claude-monitor-relay"
    ));
}

// Regression guard: broadening panel eligibility must NOT leak into the
// shared external-input predicate that backs the delivery lease and the
// ⏳ anchor lifecycle (#3164/#3174). A MonitorTriggered turn stays false there.
#[test]
fn watcher_external_input_predicate_excludes_monitor_turn() {
    let mut monitor = state_for_turn(0, "AgentDesk-claude-monitor-relay");
    monitor.turn_source = crate::services::discord::inflight::TurnSource::MonitorTriggered;
    assert!(!watcher_inflight_represents_external_input(Some(&monitor)));
}

// The monitor/self-paced-loop synthetic (user_msg_id == 0, rebind_origin)
// must NOT enter the anchor-lifecycle cleanup — that path is external-input
// only, and the synthetic has no injected notify-bot anchor to clean up.
#[test]
fn watcher_monitor_turn_never_needs_anchor_cleanup() {
    let mut monitor = state_for_turn(0, "AgentDesk-claude-monitor-relay");
    monitor.turn_source = crate::services::discord::inflight::TurnSource::MonitorTriggered;
    monitor.rebind_origin = true;
    assert!(!watcher_inflight_needs_anchor_lifecycle_cleanup(&monitor));
}
